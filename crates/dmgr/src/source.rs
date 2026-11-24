//! Importing files and folders from the host file system.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::disk::Source;
use crate::error::{PathContext, Result, invalid};
use crate::hfs::{EntryId, Metadata, VolumeBuilder};

/// How host files are translated into volume entries.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Force this owner/group on every entry. When `None`, Unix hosts keep the
    /// source ownership and other hosts use 99/99 ("unknown", which macOS
    /// maps to the current user on disk images).
    pub owner: Option<(u32, u32)>,
    /// Copy extended attributes (and resource forks) where the host has them.
    pub xattrs: bool,
    /// Extended attributes never copied.
    pub skip_xattrs: Vec<String>,
    /// File-name patterns (`*` and `?` wildcards) to leave out.
    pub exclude: Vec<String>,
    /// Clamp modification times to at most this value (reproducible builds).
    pub clamp_mtime: Option<SystemTime>,
    /// Follow symbolic links instead of copying them as links.
    pub follow_symlinks: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            owner: None,
            xattrs: cfg!(target_os = "macos"),
            skip_xattrs: vec![
                "com.apple.quarantine".into(),
                "com.apple.provenance".into(),
                "com.apple.macl".into(),
                "com.apple.lastuseddate#PS".into(),
                "com.apple.decmpfs".into(),
            ],
            exclude: Vec::new(),
            clamp_mtime: None,
            follow_symlinks: false,
        }
    }
}

/// Matches `name` against a pattern with `*` and `?` wildcards.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub(crate) struct Importer<'a> {
    pub opts: &'a ImportOptions,
    pub now: SystemTime,
}

fn name_of(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid(format!("{} has no file name", path.display())))?;
    name.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("{} is not valid UTF-8", path.display())))
}

/// Mach-O, universal binary or script with a shebang.
#[cfg_attr(unix, allow(dead_code))]
fn looks_executable(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    if f.read_exact(&mut magic).is_err() {
        return false;
    }
    matches!(
        magic,
        [0xFE, 0xED, 0xFA, 0xCE | 0xCF]
            | [0xCE | 0xCF, 0xFA, 0xED, 0xFE]
            | [0xCA, 0xFE, 0xBA, 0xBE]
            | [0xBE, 0xBA, 0xFE, 0xCA]
    ) || magic.starts_with(b"#!")
}

impl Importer<'_> {
    fn excluded(&self, name: &str) -> bool {
        self.opts.exclude.iter().any(|p| wildcard_match(p, name))
    }

    fn metadata(&self, path: &Path, md: &fs::Metadata) -> Metadata {
        let modified = md.modified().unwrap_or(self.now);
        let modified = match self.opts.clamp_mtime {
            Some(c) if modified > c => c,
            _ => modified,
        };
        let created = md.created().unwrap_or(modified).min(modified);
        let mut m = Metadata::with_permissions(0o644, modified);
        m.created = created;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            m.permissions = (md.mode() & 0o7777) as u16;
            m.uid = md.uid();
            m.gid = md.gid();
        }
        #[cfg(not(unix))]
        {
            let in_macos_dir = path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == "MacOS");
            m.permissions =
                if md.is_dir() || (md.is_file() && (in_macos_dir || looks_executable(path))) {
                    0o755
                } else {
                    0o644
                };
            if md.permissions().readonly() && !md.is_dir() {
                m.permissions &= !0o222;
            }
        }
        if let Some((u, g)) = self.opts.owner {
            m.uid = u;
            m.gid = g;
        }
        if self.opts.xattrs && !md.file_type().is_symlink() {
            m.xattrs = self.read_xattrs(path);
        }
        m
    }

    #[cfg(unix)]
    fn read_xattrs(&self, path: &Path) -> Vec<(String, Vec<u8>)> {
        let Ok(names) = xattr::list(path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for n in names {
            let Some(name) = n.to_str() else { continue };
            let mapped = if cfg!(target_os = "macos") {
                name.to_owned()
            } else if let Some(rest) = name.strip_prefix("user.") {
                rest.to_owned()
            } else {
                continue;
            };
            if self.opts.skip_xattrs.iter().any(|s| s == &mapped) {
                continue;
            }
            if let Ok(Some(v)) = xattr::get(path, &n) {
                out.push((mapped, v));
            }
        }
        out.sort();
        out
    }

    #[cfg(not(unix))]
    fn read_xattrs(&self, _path: &Path) -> Vec<(String, Vec<u8>)> {
        Vec::new()
    }

    /// Imports `src` into `parent` under `name`, recursing into directories.
    /// Existing directories are merged.
    pub fn import(
        &self,
        b: &mut VolumeBuilder,
        parent: EntryId,
        name: &str,
        src: &Path,
    ) -> Result<EntryId> {
        let md = if self.opts.follow_symlinks {
            fs::metadata(src)
        } else {
            fs::symlink_metadata(src)
        }
        .with_path(src)?;
        let ft = md.file_type();
        let meta = self.metadata(src, &md);
        if ft.is_symlink() {
            let target = fs::read_link(src).with_path(src)?;
            let target = target
                .to_str()
                .ok_or_else(|| {
                    invalid(format!("symlink target of {} is not UTF-8", src.display()))
                })?
                .replace('\\', "/");
            let mut meta = meta;
            meta.permissions = 0o755;
            return b.add_symlink(parent, name, &target, meta);
        }
        if ft.is_dir() {
            let id = match b.lookup(parent, name) {
                Some(existing) if b.is_dir(existing) => existing,
                _ => b.add_dir(parent, name, meta)?,
            };
            self.import_contents(b, id, src)?;
            return Ok(id);
        }
        if ft.is_file() {
            let id = b.add_file(parent, name, Source::file(src, md.len()), meta)?;
            return Ok(id);
        }
        Err(invalid(format!(
            "{} is not a regular file, directory or symlink",
            src.display()
        )))
    }

    /// Imports the children of directory `src` into `dir`.
    pub fn import_contents(&self, b: &mut VolumeBuilder, dir: EntryId, src: &Path) -> Result<()> {
        let mut children: Vec<PathBuf> = fs::read_dir(src)
            .with_path(src)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()
            .with_path(src)?;
        children.sort();
        for child in children {
            let name = name_of(&child)?;
            if self.excluded(&name) {
                continue;
            }
            self.import(b, dir, &name, &child)?;
        }
        Ok(())
    }
}

pub(crate) fn file_name(path: &Path) -> Result<String> {
    name_of(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.pdb", "foo.pdb"));
        assert!(!wildcard_match("*.pdb", "foo.pdbx"));
        assert!(wildcard_match(".git", ".git"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXbYY"));
    }
}
