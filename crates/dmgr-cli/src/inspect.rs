//! Read-side commands: info, ls, cat, extract, verify, convert.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Args;
use dmgr::hfs::format::mode;
use dmgr::hfs::{Entry, EntryKind, Volume, check};
use dmgr::io::Slice;
use dmgr::udif::{WriteOptions, chunk};
use dmgr::{DiskImage, Format, ImageSource};

type Vol = Volume<Slice<ImageSource>>;

pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Formats a time as `YYYY-MM-DD HH:MM:SS` UTC.
fn timestamp(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn mode_string(e: &Entry) -> String {
    let m = e.mode();
    let t = match m & mode::S_IFMT {
        mode::S_IFDIR => 'd',
        mode::S_IFLNK => 'l',
        mode::S_IFIFO => 'p',
        mode::S_IFCHR => 'c',
        mode::S_IFBLK => 'b',
        mode::S_IFSOCK => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(t);
    for (i, c) in ['r', 'w', 'x', 'r', 'w', 'x', 'r', 'w', 'x']
        .iter()
        .enumerate()
    {
        s.push(if m & (0o400 >> i) != 0 { *c } else { '-' });
    }
    s
}

fn open_volume(path: &Path) -> Result<Vol> {
    let img = DiskImage::open(path).with_context(|| format!("opening {}", path.display()))?;
    img.into_volume()
        .with_context(|| format!("reading the volume in {}", path.display()))
}

fn find<'a>(vol: &'a Vol, path: &str) -> Result<&'a Entry> {
    vol.lookup(path)
        .with_context(|| format!("{path}: no such file or directory in the image"))
}

#[derive(Args, Debug)]
pub struct InfoArgs {
    /// Disk image.
    image: PathBuf,
    /// List every chunk of every block table.
    #[arg(long)]
    chunks: bool,
}

pub fn info(args: InfoArgs) -> Result<()> {
    let img = DiskImage::open(&args.image)
        .with_context(|| format!("opening {}", args.image.display()))?;
    let file_size = fs::metadata(&args.image)?.len();
    let mut out = io::stdout().lock();
    writeln!(out, "Image:        {}", args.image.display())?;
    writeln!(
        out,
        "File size:    {} ({file_size} bytes)",
        human(file_size)
    )?;
    writeln!(
        out,
        "Disk size:    {} ({} bytes)",
        human(img.size()),
        img.size()
    )?;
    match img.udif() {
        Some(u) => {
            let k = u.koly();
            writeln!(out, "Container:    UDIF ({})", u.format_guess())?;
            writeln!(out, "Segment ID:   {}", uuid_string(&k.segment_id))?;
            writeln!(
                out,
                "Data fork:    {} bytes, CRC32 {:08x}",
                k.data_fork_length, k.data_fork_checksum.data[0]
            )?;
            writeln!(out, "Master CRC32: {:08x}", k.master_checksum.data[0])?;
            writeln!(out, "Block tables:")?;
            for b in u.blkx() {
                let t = &b.table;
                let stored: u64 = t.chunks.iter().map(|c| c.length).sum();
                writeln!(
                    out,
                    "  {:>3} {:<44} sectors {:>10}..{:<10} {:>6} chunks {:>10} stored",
                    b.id,
                    b.name,
                    t.first_sector,
                    t.first_sector + t.sector_count,
                    t.chunks.len(),
                    human(stored)
                )?;
                if args.chunks {
                    writeln!(
                        out,
                        "      buffers needed {}, descriptors {}",
                        t.buffers_needed, t.block_descriptors
                    )?;
                    for c in &t.chunks {
                        writeln!(
                            out,
                            "      {:<8} sector {:>10} +{:<6} offset {:>12} length {:>8}",
                            chunk::name(c.kind),
                            c.sector,
                            c.sector_count,
                            c.offset,
                            c.length
                        )?;
                    }
                }
            }
        }
        None => writeln!(out, "Container:    raw")?,
    }
    writeln!(out, "Partition map: {:?}", img.partition_map())?;
    for p in img.partitions() {
        writeln!(
            out,
            "  {:<24} {:<38} offset {:>12} size {}",
            p.name,
            p.kind,
            p.offset,
            human(p.size)
        )?;
    }
    if img.hfs_partition().is_some() {
        let vol = img.into_volume()?;
        let h = vol.header();
        writeln!(out, "Volume:")?;
        writeln!(out, "  Name:          {}", vol.name())?;
        writeln!(
            out,
            "  File system:   {}",
            if h.is_hfsx() {
                "HFSX (case-sensitive HFS+)"
            } else {
                "HFS+"
            }
        )?;
        writeln!(out, "  Block size:    {}", h.block_size)?;
        writeln!(
            out,
            "  Capacity:      {} ({} free)",
            human(u64::from(h.total_blocks) * u64::from(h.block_size)),
            human(u64::from(h.free_blocks) * u64::from(h.block_size))
        )?;
        writeln!(out, "  Files:         {}", h.file_count)?;
        writeln!(out, "  Folders:       {}", h.folder_count)?;
        writeln!(
            out,
            "  Created:       {}",
            timestamp(dmgr::hfs::time::from_hfs(h.create_date))
        )?;
        writeln!(
            out,
            "  Volume ID:     {:08x}{:08x}",
            h.finder_info[6], h.finder_info[7]
        )?;
        if h.journal_info_block != 0 {
            writeln!(out, "  Journaled:     yes")?;
        }
    }
    Ok(())
}

fn uuid_string(b: &[u8; 16]) -> String {
    uuid::Uuid::from_bytes(*b).to_string().to_uppercase()
}

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Disk image.
    image: PathBuf,
    /// Folder inside the image.
    #[arg(default_value = "/")]
    path: String,
    /// Long listing (mode, owner, size, date).
    #[arg(short, long)]
    long: bool,
    /// List folders recursively.
    #[arg(short = 'R', long)]
    recursive: bool,
}

pub fn ls(args: LsArgs) -> Result<()> {
    let mut vol = open_volume(&args.image)?;
    let start = find(&vol, &args.path)?.clone();
    let mut out = io::stdout().lock();
    if !start.is_dir() {
        print_entry(&mut vol, &mut out, &start, &start.name, args.long)?;
        return Ok(());
    }
    let mut stack = vec![(start.cnid, String::new())];
    while let Some((cnid, prefix)) = stack.pop() {
        let mut children: Vec<Entry> = vol.list(cnid).into_iter().cloned().collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        let mut subdirs = Vec::new();
        for e in &children {
            let shown = format!("{prefix}{}", e.name);
            print_entry(&mut vol, &mut out, e, &shown, args.long)?;
            if args.recursive && e.is_dir() {
                subdirs.push((e.cnid, format!("{shown}/")));
            }
        }
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(())
}

fn print_entry(
    vol: &mut Vol,
    out: &mut impl Write,
    e: &Entry,
    shown: &str,
    long: bool,
) -> Result<()> {
    let suffix = if e.kind() == EntryKind::Symlink {
        format!(" -> {}", vol.read_link(e)?)
    } else if e.is_dir() {
        "/".into()
    } else {
        String::new()
    };
    if long {
        let p = e.permissions();
        let size = match e.kind() {
            EntryKind::HardLink(_) => vol.resolve_link(e)?.data_size(),
            _ => e.data_size(),
        };
        writeln!(
            out,
            "{} {:>5} {:>5} {:>12} {} {shown}{suffix}",
            mode_string(e),
            p.owner_id,
            p.group_id,
            size,
            timestamp(e.modified())
        )?;
    } else {
        writeln!(out, "{shown}{suffix}")?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct CatArgs {
    /// Disk image.
    image: PathBuf,
    /// File inside the image.
    path: String,
    /// Print the resource fork instead of the data fork.
    #[arg(long)]
    rsrc: bool,
}

pub fn cat(args: CatArgs) -> Result<()> {
    let mut vol = open_volume(&args.image)?;
    let e = find(&vol, &args.path)?.clone();
    if e.is_dir() {
        bail!("{} is a folder", args.path);
    }
    let mut out = io::stdout().lock();
    if args.rsrc {
        out.write_all(&vol.read_resource_fork(&e)?)?;
    } else {
        vol.copy_file(&e, &mut out)?;
    }
    out.flush()?;
    Ok(())
}

#[derive(Args, Debug)]
pub struct ExtractArgs {
    /// Disk image.
    image: PathBuf,
    /// Destination folder (created if missing).
    dest: PathBuf,
    /// Only extract this path from the image.
    #[arg(long, default_value = "/")]
    path: String,
    /// Also extract hidden Finder files (.DS_Store, .background, ...).
    #[arg(short, long)]
    all: bool,
    /// Print each extracted path.
    #[arg(short, long)]
    verbose: bool,
}

/// Replaces characters the host file system cannot store.
fn host_name(name: &str) -> String {
    let bad: &[char] = if cfg!(windows) {
        &['<', '>', ':', '"', '/', '\\', '|', '?', '*']
    } else {
        &['/']
    };
    let s: String = name
        .chars()
        .map(|c| {
            if bad.contains(&c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    match s.as_str() {
        "" | "." | ".." => "_".into(),
        _ => s,
    }
}

fn is_finder_metadata(name: &str) -> bool {
    matches!(
        name,
        ".DS_Store"
            | ".background"
            | ".VolumeIcon.icns"
            | ".fseventsd"
            | ".Trashes"
            | ".TemporaryItems"
    )
}

pub fn extract(args: ExtractArgs) -> Result<()> {
    let mut vol = open_volume(&args.image)?;
    let start = find(&vol, &args.path)?.clone();
    fs::create_dir_all(&args.dest).with_context(|| format!("creating {}", args.dest.display()))?;
    let mut count = 0u64;
    let mut dirs: Vec<(PathBuf, Entry)> = Vec::new();
    if start.is_dir() {
        let mut stack = vec![(start.cnid, args.dest.clone(), true)];
        while let Some((cnid, dir, top)) = stack.pop() {
            let children: Vec<Entry> = vol.list(cnid).into_iter().cloned().collect();
            for e in children {
                if top && start.cnid == 2 && !args.all && is_finder_metadata(&e.name) {
                    continue;
                }
                let target = dir.join(host_name(&e.name));
                extract_one(&mut vol, &e, &target, args.verbose)?;
                count += 1;
                if e.is_dir() {
                    stack.push((e.cnid, target.clone(), false));
                    dirs.push((target, e));
                }
            }
        }
    } else {
        let target = args.dest.join(host_name(&start.name));
        extract_one(&mut vol, &start, &target, args.verbose)?;
        count += 1;
    }
    // Apply folder times/permissions last so writing children does not change them.
    for (path, e) in dirs.iter().rev() {
        set_mtime(path, e.modified());
        set_mode(path, e.mode())?;
    }
    eprintln!("extracted {count} items to {}", args.dest.display());
    Ok(())
}

/// Best-effort modification time update (folders cannot be opened on Windows).
fn set_mtime(path: &Path, t: SystemTime) {
    let f = fs::File::options()
        .write(true)
        .open(path)
        .or_else(|_| fs::File::open(path));
    if let Ok(f) = f {
        let _ = f.set_modified(t);
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, m: u16) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(u32::from(m & 0o7777)))
        .with_context(|| format!("setting permissions of {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _m: u16) -> Result<()> {
    Ok(())
}

fn extract_one(vol: &mut Vol, e: &Entry, target: &Path, verbose: bool) -> Result<()> {
    if verbose {
        eprintln!("{}", target.display());
    }
    match e.kind() {
        EntryKind::Directory => {
            fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
        }
        EntryKind::Symlink => {
            let link = vol.read_link(e)?;
            let _ = fs::remove_file(target);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, target)
                .with_context(|| format!("creating symlink {}", target.display()))?;
            #[cfg(windows)]
            {
                let r = std::os::windows::fs::symlink_file(&link, target);
                if r.is_err() {
                    eprintln!(
                        "dmgr: warning: cannot create symlink {} -> {link} (needs Developer Mode or admin); skipped",
                        target.display()
                    );
                }
            }
        }
        EntryKind::File | EntryKind::HardLink(_) => {
            let f = fs::File::create(target)
                .with_context(|| format!("creating {}", target.display()))?;
            let mut w = io::BufWriter::new(f);
            vol.copy_file(e, &mut w)?;
            w.flush()?;
            drop(w);
            let src = vol.resolve_link(e)?.clone();
            set_mtime(target, src.modified());
            set_mode(target, src.mode())?;
        }
        other => {
            eprintln!("dmgr: warning: skipping {} ({other:?})", target.display());
        }
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Disk images to verify.
    #[arg(required = true)]
    images: Vec<PathBuf>,
    /// Only print problems.
    #[arg(short, long)]
    quiet: bool,
}

pub fn verify(args: VerifyArgs) -> Result<()> {
    let mut failed = 0;
    for path in &args.images {
        let mut problems: Vec<String> = Vec::new();
        let mut img =
            DiskImage::open(path).with_context(|| format!("opening {}", path.display()))?;
        if let Some(r) = img.verify_checksums()? {
            for (name, stored, computed) in &r.tables {
                if stored != computed {
                    problems.push(format!(
                        "checksum mismatch in {name}: stored {stored:08x}, computed {computed:08x}"
                    ));
                }
            }
            if let Some((s, c)) = r.master {
                if s != c {
                    problems.push(format!(
                        "master checksum mismatch: stored {s:08x}, computed {c:08x}"
                    ));
                }
            }
            if let Some((s, c)) = r.data_fork {
                if s != c {
                    problems.push(format!(
                        "data fork checksum mismatch: stored {s:08x}, computed {c:08x}"
                    ));
                }
            }
        }
        match img.hfs_partition().cloned() {
            Some(p) => {
                let mut vol = img.into_volume()?;
                problems.extend(check::check(&mut vol, p.size)?);
                // Read every file to make sure all data decompresses.
                let entries: Vec<Entry> = vol.entries().filter(|e| !e.is_dir()).cloned().collect();
                for e in entries {
                    if let Err(err) = vol.copy_file(&e, &mut io::sink()) {
                        problems.push(format!("{}: {err}", vol.path_of(e.cnid)));
                    }
                }
            }
            None => problems.push("no HFS+ volume found".into()),
        }
        if problems.is_empty() {
            if !args.quiet {
                println!("{}: OK", path.display());
            }
        } else {
            failed += 1;
            println!("{}: {} problem(s)", path.display(), problems.len());
            for p in problems {
                println!("  {p}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} image(s) failed verification");
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct ConvertArgs {
    /// Source image.
    input: PathBuf,
    /// Output image.
    #[arg(short, long)]
    output: PathBuf,
    /// Target format.
    #[arg(short, long, default_value = "UDZO")]
    format: String,
    /// Compression level.
    #[arg(short, long)]
    level: Option<u32>,
    /// Overwrite the output file if it exists.
    #[arg(short = 'y', long, visible_alias = "ov")]
    overwrite: bool,
}

pub fn convert(args: ConvertArgs) -> Result<()> {
    if args.output.exists() && !args.overwrite {
        bail!(
            "{} already exists (use --overwrite to replace it)",
            args.output.display()
        );
    }
    let mut opts = WriteOptions::new(args.format.parse::<Format>()?);
    opts.level = args.level;
    let s = dmgr::convert(&args.input, &args.output, &opts)?;
    eprintln!(
        "converted {} -> {} ({}, {})",
        args.input.display(),
        args.output.display(),
        opts.format,
        human(s.image_size)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_format() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01 00:00:00");
        assert_eq!(
            timestamp(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
            "2023-11-14 22:13:20"
        );
        assert_eq!(human(1536), "1.5 KiB");
    }

    #[test]
    fn names() {
        assert_eq!(host_name(".."), "_");
        assert_eq!(host_name("a/b"), "a_b");
    }
}
