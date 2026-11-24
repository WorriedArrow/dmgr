//! The high-level [`DmgBuilder`] API.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use uuid::Uuid;

use crate::disk::Source;
use crate::error::{PathContext, Result, invalid};
use crate::finder::alias::{AliasTarget, alias_record};
use crate::finder::{Background, WindowLayout, dsstore};
use crate::hfs::format::finder_flags;
use crate::hfs::time::to_hfs;
use crate::hfs::{BuiltVolume, EntryId, Metadata, VolumeBuilder, VolumeOptions};
use crate::partition::{self, DiskLayout, PartitionScheme};
use crate::source::{ImportOptions, Importer, file_name};
use crate::udif::{self, Format, WriteOptions, WriteSummary};

/// Something to put into the volume.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Item {
    /// Copy a file, folder or symlink to `dest` (a path inside the volume).
    Path { src: PathBuf, dest: String },
    /// Copy the contents of folder `src` into folder `dest` (`""` for the root).
    Contents { src: PathBuf, dest: String },
    /// Create a symbolic link at `dest` pointing to `target`.
    Symlink { dest: String, target: String },
    /// Create a file with the given contents and permission bits.
    File {
        dest: String,
        data: Vec<u8>,
        permissions: u16,
    },
    /// Create an empty folder.
    Dir { dest: String },
}

/// Progress callback: `(bytes processed, total bytes)`.
pub type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Builds a `.dmg` disk image.
///
/// ```no_run
/// use dmgr::{DmgBuilder, Format, finder::{WindowLayout, IconPosition}};
///
/// let mut dmg = DmgBuilder::new("My App");
/// dmg.format(Format::Udzo)
///     .add_path("target/release/My App.app")
///     .add_applications_link()
///     .window(WindowLayout {
///         icons: vec![
///             IconPosition { name: "My App.app".into(), x: 160, y: 180 },
///             IconPosition { name: "Applications".into(), x: 480, y: 180 },
///         ],
///         ..Default::default()
///     });
/// dmg.write("My App.dmg")?;
/// # Ok::<(), dmgr::Error>(())
/// ```
#[derive(Clone)]
pub struct DmgBuilder {
    volume_name: String,
    format: Format,
    level: Option<u32>,
    case_sensitive: bool,
    scheme: PartitionScheme,
    size: Option<u64>,
    free_space: Option<u64>,
    block_size: u32,
    chunk_sectors: u32,
    items: Vec<Item>,
    window: Option<WindowLayout>,
    volume_icon: Option<PathBuf>,
    hidden: Vec<String>,
    hide_extension: Vec<String>,
    open_folder: bool,
    timestamp: Option<SystemTime>,
    import: ImportOptions,
    progress: Option<ProgressFn>,
}

impl std::fmt::Debug for DmgBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmgBuilder")
            .field("volume_name", &self.volume_name)
            .field("format", &self.format)
            .field("items", &self.items)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

fn split_dest(dest: &str) -> (Vec<&str>, Option<&str>) {
    let mut parts: Vec<&str> = dest
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let last = parts.pop();
    (parts, last)
}

impl DmgBuilder {
    /// Starts an image whose volume is named `volume_name`.
    pub fn new(volume_name: impl Into<String>) -> Self {
        Self {
            volume_name: volume_name.into(),
            format: Format::Udzo,
            level: None,
            case_sensitive: false,
            scheme: PartitionScheme::Gpt,
            size: None,
            free_space: None,
            block_size: 4096,
            chunk_sectors: 2048,
            items: Vec::new(),
            window: None,
            volume_icon: None,
            hidden: Vec::new(),
            hide_extension: Vec::new(),
            open_folder: false,
            timestamp: None,
            import: ImportOptions::default(),
            progress: None,
        }
    }

    pub fn volume_name(&self) -> &str {
        &self.volume_name
    }

    /// Output format (default [`Format::Udzo`]).
    pub fn format(&mut self, format: Format) -> &mut Self {
        self.format = format;
        self
    }

    /// Compression level (format specific; zlib/bzip2 1-9, LZMA 0-9).
    pub fn level(&mut self, level: u32) -> &mut Self {
        self.level = Some(level);
        self
    }

    /// Use a case-sensitive HFSX volume.
    pub fn case_sensitive(&mut self, yes: bool) -> &mut Self {
        self.case_sensitive = yes;
        self
    }

    /// Partition scheme (default GPT, like `hdiutil`).
    pub fn partition_scheme(&mut self, scheme: PartitionScheme) -> &mut Self {
        self.scheme = scheme;
        self
    }

    /// Fixed volume size in bytes (default: fit the contents).
    pub fn size(&mut self, bytes: u64) -> &mut Self {
        self.size = Some(bytes);
        self
    }

    /// Extra free space when sizing automatically.
    pub fn free_space(&mut self, bytes: u64) -> &mut Self {
        self.free_space = Some(bytes);
        self
    }

    /// Allocation block size (default 4096).
    pub fn block_size(&mut self, bytes: u32) -> &mut Self {
        self.block_size = bytes;
        self
    }

    /// Sectors per compressed chunk (default 2048 = 1 MiB).
    pub fn chunk_sectors(&mut self, sectors: u32) -> &mut Self {
        self.chunk_sectors = sectors;
        self
    }

    /// Copies a file or folder into the volume root, keeping its name.
    pub fn add_path(&mut self, src: impl Into<PathBuf>) -> &mut Self {
        let src = src.into();
        let dest = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.items.push(Item::Path { src, dest });
        self
    }

    /// Copies a file or folder to `dest` inside the volume.
    pub fn add_path_as(&mut self, src: impl Into<PathBuf>, dest: impl Into<String>) -> &mut Self {
        self.items.push(Item::Path {
            src: src.into(),
            dest: dest.into(),
        });
        self
    }

    /// Copies the contents of a folder into the volume root (`hdiutil -srcfolder`).
    pub fn add_contents(&mut self, src: impl Into<PathBuf>) -> &mut Self {
        self.items.push(Item::Contents {
            src: src.into(),
            dest: String::new(),
        });
        self
    }

    /// Copies the contents of a folder into `dest` inside the volume.
    pub fn add_contents_to(
        &mut self,
        src: impl Into<PathBuf>,
        dest: impl Into<String>,
    ) -> &mut Self {
        self.items.push(Item::Contents {
            src: src.into(),
            dest: dest.into(),
        });
        self
    }

    /// Adds a symbolic link.
    pub fn add_symlink(&mut self, dest: impl Into<String>, target: impl Into<String>) -> &mut Self {
        self.items.push(Item::Symlink {
            dest: dest.into(),
            target: target.into(),
        });
        self
    }

    /// Adds the customary `Applications -> /Applications` link.
    pub fn add_applications_link(&mut self) -> &mut Self {
        self.add_symlink("Applications", "/Applications")
    }

    /// Adds a file with the given contents.
    pub fn add_file(
        &mut self,
        dest: impl Into<String>,
        data: impl Into<Vec<u8>>,
        permissions: u16,
    ) -> &mut Self {
        self.items.push(Item::File {
            dest: dest.into(),
            data: data.into(),
            permissions,
        });
        self
    }

    /// Adds an empty folder.
    pub fn add_dir(&mut self, dest: impl Into<String>) -> &mut Self {
        self.items.push(Item::Dir { dest: dest.into() });
        self
    }

    /// Adds an arbitrary [`Item`].
    pub fn add_item(&mut self, item: Item) -> &mut Self {
        self.items.push(item);
        self
    }

    /// Customizes the Finder window shown when the image is opened.
    pub fn window(&mut self, layout: WindowLayout) -> &mut Self {
        self.window = Some(layout);
        self
    }

    /// Sets the volume icon from an `.icns` file.
    pub fn volume_icon(&mut self, icns: impl Into<PathBuf>) -> &mut Self {
        self.volume_icon = Some(icns.into());
        self
    }

    /// Hides an item from Finder (sets the invisible flag).
    pub fn hide(&mut self, path: impl Into<String>) -> &mut Self {
        self.hidden.push(path.into());
        self
    }

    /// Hides the file name extension of an item in Finder.
    pub fn hide_extension(&mut self, path: impl Into<String>) -> &mut Self {
        self.hide_extension.push(path.into());
        self
    }

    /// Ask Finder to open the volume's window when the image is mounted.
    pub fn open_folder(&mut self, yes: bool) -> &mut Self {
        self.open_folder = yes;
        self
    }

    /// Makes the output reproducible: all generated timestamps use `t`,
    /// file modification times are clamped to it, and identifiers are
    /// derived from the contents instead of being random.
    pub fn reproducible(&mut self, t: SystemTime) -> &mut Self {
        self.timestamp = Some(t);
        self.import.clamp_mtime = Some(t);
        self
    }

    /// Options for importing host files.
    pub fn import_options(&mut self) -> &mut ImportOptions {
        &mut self.import
    }

    /// Leaves out files whose name matches `pattern` (`*`/`?` wildcards).
    pub fn exclude(&mut self, pattern: impl Into<String>) -> &mut Self {
        self.import.exclude.push(pattern.into());
        self
    }

    /// Reports progress while writing.
    pub fn progress(&mut self, f: impl Fn(u64, u64) + Send + Sync + 'static) -> &mut Self {
        self.progress = Some(Arc::new(f));
        self
    }

    fn uuid(&self, purpose: &str) -> Uuid {
        match self.timestamp {
            Some(t) => {
                let secs = t
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mut seed = format!("dmgr:{purpose}:{}:{secs}", self.volume_name);
                for item in &self.items {
                    seed.push_str(&format!(":{item:?}"));
                }
                Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes())
            }
            None => Uuid::new_v4(),
        }
    }

    fn ensure_dirs(b: &mut VolumeBuilder, parts: &[&str], time: SystemTime) -> Result<EntryId> {
        let mut cur = b.root();
        for p in parts {
            cur = match b.lookup(cur, p) {
                Some(id) if b.is_dir(id) => id,
                Some(_) => return Err(invalid(format!("{p:?} exists and is not a folder"))),
                None => b.add_dir(cur, p, Metadata::dir(time))?,
            };
        }
        Ok(cur)
    }

    /// Builds the file system tree and lays out the HFS+ volume.
    pub fn build_volume(&self) -> Result<BuiltVolume> {
        if self.volume_name.is_empty() {
            return Err(invalid("volume name must not be empty"));
        }
        let now = self.timestamp.unwrap_or_else(SystemTime::now);
        let vol_uuid = self.uuid("volume");
        let opts = VolumeOptions {
            name: self.volume_name.clone(),
            case_sensitive: self.case_sensitive,
            block_size: self.block_size,
            created: now,
            volume_id: u64::from_be_bytes(vol_uuid.as_bytes()[..8].try_into().expect("8 bytes")),
            open_folder: self.open_folder,
            size: self.size,
            free_space: self.free_space,
        };
        let mut b = VolumeBuilder::new(opts)?;
        let owner_meta = |m: &mut Metadata| {
            if let Some((u, g)) = self.import.owner {
                m.uid = u;
                m.gid = g;
            }
        };
        let root = b.root();
        owner_meta(b.metadata_mut(root));
        let importer = Importer {
            opts: &self.import,
            now,
        };

        for item in &self.items {
            match item {
                Item::Path { src, dest } => {
                    let dest = if dest.is_empty() {
                        file_name(src)?
                    } else {
                        dest.clone()
                    };
                    let (dirs, name) = split_dest(&dest);
                    let name = name.ok_or_else(|| {
                        invalid(format!("empty destination for {}", src.display()))
                    })?;
                    let parent = Self::ensure_dirs(&mut b, &dirs, now)?;
                    importer.import(&mut b, parent, name, src)?;
                }
                Item::Contents { src, dest } => {
                    let (mut dirs, name) = split_dest(dest);
                    dirs.extend(name);
                    let parent = Self::ensure_dirs(&mut b, &dirs, now)?;
                    if !src.is_dir() {
                        return Err(invalid(format!("{} is not a folder", src.display())));
                    }
                    importer.import_contents(&mut b, parent, src)?;
                }
                Item::Symlink { dest, target } => {
                    let (dirs, name) = split_dest(dest);
                    let name = name.ok_or_else(|| invalid("empty symlink name"))?;
                    let parent = Self::ensure_dirs(&mut b, &dirs, now)?;
                    let mut m = Metadata::with_permissions(0o755, now);
                    owner_meta(&mut m);
                    b.add_symlink(parent, name, target, m)?;
                }
                Item::File {
                    dest,
                    data,
                    permissions,
                } => {
                    let (dirs, name) = split_dest(dest);
                    let name = name.ok_or_else(|| invalid("empty file name"))?;
                    let parent = Self::ensure_dirs(&mut b, &dirs, now)?;
                    let mut m = Metadata::with_permissions(*permissions, now);
                    owner_meta(&mut m);
                    b.add_file(parent, name, data.clone(), m)?;
                }
                Item::Dir { dest } => {
                    let parts: Vec<&str> = dest.split('/').filter(|p| !p.is_empty()).collect();
                    Self::ensure_dirs(&mut b, &parts, now)?;
                }
            }
        }

        let hidden_meta = |b: &mut VolumeBuilder, id: EntryId, flag: u16| {
            let m = b.metadata_mut(id);
            let f = m.finder_flags() | flag;
            m.set_finder_flags(f);
        };

        // Volume icon.
        if let Some(icns) = &self.volume_icon {
            let data = fs::read(icns).with_path(icns)?;
            let root = b.root();
            if let Some(old) = b.lookup(root, ".VolumeIcon.icns") {
                b.set_file_data(old, data)?;
            } else {
                let mut m = Metadata::file(now);
                owner_meta(&mut m);
                let id = b.add_file(root, ".VolumeIcon.icns", data, m)?;
                hidden_meta(&mut b, id, finder_flags::IS_INVISIBLE);
            }
            hidden_meta(&mut b, root, finder_flags::HAS_CUSTOM_ICON);
        }

        // Finder window layout.
        if let Some(layout) = &self.window {
            let root = b.root();
            let mut alias = None;
            if let Background::Image(img) = &layout.background {
                let name = file_name(img)?;
                let bg_dir = match b.lookup(root, ".background") {
                    Some(id) if b.is_dir(id) => id,
                    Some(_) => return Err(invalid(".background exists and is not a folder")),
                    None => {
                        let mut m = Metadata::dir(now);
                        owner_meta(&mut m);
                        b.add_dir(root, ".background", m)?
                    }
                };
                hidden_meta(&mut b, bg_dir, finder_flags::IS_INVISIBLE);
                let len = fs::metadata(img).with_path(img)?.len();
                let img_id = match b.lookup(bg_dir, &name) {
                    Some(id) => {
                        b.set_file_data(id, Source::file(img, len))?;
                        id
                    }
                    None => {
                        let mut m = Metadata::file(now);
                        owner_meta(&mut m);
                        b.add_file(bg_dir, &name, Source::file(img, len), m)?
                    }
                };
                alias = Some(alias_record(&AliasTarget {
                    volume_name: self.volume_name.clone(),
                    volume_created: to_hfs(now),
                    is_folder: false,
                    parent_cnid: b.cnid(bg_dir),
                    cnid: b.cnid(img_id),
                    created: to_hfs(b.metadata(img_id).created),
                    components: vec![".background".into(), name],
                    ancestor_cnids: vec![b.cnid(bg_dir)],
                }));
            }
            for icon in &layout.icons {
                if b.lookup(root, &icon.name).is_none() {
                    return Err(invalid(format!(
                        "icon position given for {:?}, which is not in the volume root",
                        icon.name
                    )));
                }
            }
            let ds = dsstore::write(layout.ds_store_records(alias)?);
            match b.lookup(root, ".DS_Store") {
                Some(id) => b.set_file_data(id, ds)?,
                None => {
                    let mut m = Metadata::file(now);
                    owner_meta(&mut m);
                    let id = b.add_file(root, ".DS_Store", ds, m)?;
                    hidden_meta(&mut b, id, finder_flags::IS_INVISIBLE);
                }
            }
        }

        for p in &self.hidden {
            let id = b
                .lookup_path(p)
                .ok_or_else(|| invalid(format!("cannot hide {p:?}: not in the volume")))?;
            hidden_meta(&mut b, id, finder_flags::IS_INVISIBLE);
        }
        for p in &self.hide_extension {
            let id = b.lookup_path(p).ok_or_else(|| {
                invalid(format!("cannot hide extension of {p:?}: not in the volume"))
            })?;
            hidden_meta(&mut b, id, finder_flags::EXTENSION_HIDDEN);
        }

        b.build()
    }

    /// Builds the volume and wraps it in the partition scheme.
    pub fn build_disk(&self) -> Result<DiskLayout> {
        let vol = self.build_volume()?;
        partition::layout(
            self.scheme,
            &vol.disk,
            self.uuid("disk"),
            self.uuid("partition"),
        )
    }

    fn write_options(&self) -> WriteOptions {
        WriteOptions {
            format: self.format,
            level: self.level,
            chunk_sectors: self.chunk_sectors,
            segment_id: *self.uuid("segment").as_bytes(),
        }
    }

    /// Writes the image to any writer.
    pub fn write_to<W: Write>(&self, out: W) -> Result<WriteSummary> {
        let layout = self.build_disk()?;
        let progress = self.progress.clone();
        let cb = progress.as_deref().map(|f| f as &(dyn Fn(u64, u64) + Sync));
        udif::write(
            &layout.disk,
            &layout.regions,
            &self.write_options(),
            out,
            cb,
        )
    }

    /// Writes the image to `path` (atomically, via a temporary file).
    pub fn write(&self, path: impl AsRef<Path>) -> Result<WriteSummary> {
        let path = path.as_ref();
        let layout = self.build_disk()?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".partial");
        let tmp = PathBuf::from(tmp);
        let result = (|| {
            let f = File::create(&tmp).with_path(&tmp)?;
            let mut w = BufWriter::with_capacity(1 << 20, f);
            let progress = self.progress.clone();
            let cb = progress.as_deref().map(|f| f as &(dyn Fn(u64, u64) + Sync));
            let s = udif::write(
                &layout.disk,
                &layout.regions,
                &self.write_options(),
                &mut w,
                cb,
            )?;
            let f = w.into_inner().map_err(|e| e.into_error())?;
            f.sync_all().with_path(&tmp)?;
            drop(f);
            if path.exists() {
                fs::remove_file(path).with_path(path)?;
            }
            fs::rename(&tmp, path).with_path(path)?;
            Ok(s)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}
