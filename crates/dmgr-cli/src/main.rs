//! `dmgr`: create, inspect, verify and extract macOS disk images on any platform.

mod config;
mod inspect;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use dmgr::finder::{Background, IconPosition, WindowLayout};
use dmgr::{DmgBuilder, Format, PartitionScheme};

use crate::config::{Config, parse_size};

#[derive(Parser, Debug)]
#[command(name = "dmgr", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a disk image from files and folders.
    Create(Box<CreateArgs>),
    /// Show information about an image: format, partitions, volume.
    Info(inspect::InfoArgs),
    /// List files in an image.
    Ls(inspect::LsArgs),
    /// Print a file from an image to standard output.
    Cat(inspect::CatArgs),
    /// Extract files from an image.
    Extract(inspect::ExtractArgs),
    /// Verify checksums and file system consistency.
    Verify(inspect::VerifyArgs),
    /// Convert an image to another format.
    Convert(inspect::ConvertArgs),
}

/// Options for `dmgr create`.
#[derive(Args, Debug, Default)]
struct CreateArgs {
    /// Files or folders to copy into the volume root.
    sources: Vec<PathBuf>,

    /// Output image path.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Read settings from a TOML file (command-line options take precedence).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Volume name (defaults to the output file name).
    #[arg(short = 'V', long, visible_alias = "volume-name")]
    volname: Option<String>,

    /// Image format: UDZO (zlib), ULFO (lzfse), ULMO (lzma), UDBZ (bzip2), UDRO, UDRW.
    #[arg(short, long)]
    format: Option<String>,

    /// Compression level.
    #[arg(short, long)]
    level: Option<u32>,

    /// File system: hfs+ (case-insensitive) or hfsx (case-sensitive).
    #[arg(long, visible_alias = "fs")]
    filesystem: Option<String>,

    /// Partition layout: gpt or none.
    #[arg(long)]
    layout: Option<String>,

    /// Volume size, e.g. 200m (default: fit contents).
    #[arg(long, value_parser = parse_size)]
    size: Option<u64>,

    /// Extra free space when sizing automatically, e.g. 50m.
    #[arg(long, value_parser = parse_size)]
    free_space: Option<u64>,

    /// Copy the contents of this folder into the volume root (repeatable).
    #[arg(short = 's', long, value_name = "DIR")]
    srcfolder: Vec<PathBuf>,

    /// Copy SRC to DEST inside the volume (repeatable), as DEST=SRC.
    #[arg(long, value_name = "DEST=SRC")]
    copy: Vec<String>,

    /// Create a symlink NAME pointing to TARGET (repeatable), as NAME=TARGET.
    #[arg(long, value_name = "NAME=TARGET")]
    symlink: Vec<String>,

    /// Add an "Applications" link for drag-and-drop installation.
    #[arg(long, visible_alias = "app-drop-link")]
    applications_link: bool,

    /// Volume icon (.icns).
    #[arg(long, value_name = "ICNS", visible_alias = "volicon")]
    icon: Option<PathBuf>,

    /// Window background picture.
    #[arg(long, value_name = "IMAGE")]
    background: Option<PathBuf>,

    /// Window background color (#RRGGBB).
    #[arg(long, value_name = "COLOR")]
    background_color: Option<String>,

    /// Window position on screen, X,Y.
    #[arg(long, value_name = "X,Y")]
    window_pos: Option<String>,

    /// Window size, WIDTH,HEIGHT.
    #[arg(long, value_name = "W,H")]
    window_size: Option<String>,

    /// Icon size in points.
    #[arg(long)]
    icon_size: Option<u16>,

    /// Label text size in points.
    #[arg(long)]
    text_size: Option<u16>,

    /// Put labels to the right of icons instead of below.
    #[arg(long)]
    label_right: bool,

    /// Position an item's icon, NAME=X,Y (repeatable).
    #[arg(long, value_name = "NAME=X,Y")]
    position: Vec<String>,

    /// Hide an item in Finder (repeatable).
    #[arg(long, value_name = "PATH")]
    hide: Vec<String>,

    /// Hide an item's file extension in Finder (repeatable).
    #[arg(long, value_name = "PATH")]
    hide_extension: Vec<String>,

    /// Skip source files matching a pattern, e.g. "*.dSYM" (repeatable).
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,

    /// Set owner and group of all files, UID:GID.
    #[arg(long, value_name = "UID:GID")]
    owner: Option<String>,

    /// Copy extended attributes (default on macOS).
    #[arg(long, overrides_with = "no_xattrs")]
    xattrs: bool,

    /// Do not copy extended attributes.
    #[arg(long)]
    no_xattrs: bool,

    /// Open a Finder window when the image is mounted.
    #[arg(long)]
    open_folder: bool,

    /// Byte-for-byte reproducible output (uses SOURCE_DATE_EPOCH or the newest source mtime).
    #[arg(long)]
    reproducible: bool,

    /// Overwrite the output file if it exists.
    #[arg(short = 'y', long, visible_alias = "ov")]
    overwrite: bool,

    /// Do not print progress.
    #[arg(short, long)]
    quiet: bool,
}

fn split_kv(s: &str, what: &str) -> Result<(String, String)> {
    s.split_once('=')
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .with_context(|| format!("{what} must be of the form A=B, got {s:?}"))
}

fn pair<T: std::str::FromStr>(s: &str, what: &str) -> Result<(T, T)> {
    let (a, b) = s
        .split_once([',', 'x', ' '])
        .with_context(|| format!("{what} must be two numbers like 100,200, got {s:?}"))?;
    let p = |v: &str| {
        v.trim()
            .parse::<T>()
            .map_err(|_| anyhow::anyhow!("invalid number {v:?} in {what}"))
    };
    Ok((p(a)?, p(b)?))
}

fn newest_mtime(paths: &[PathBuf]) -> SystemTime {
    fn walk(p: &std::path::Path, best: &mut SystemTime) {
        if let Ok(md) = std::fs::symlink_metadata(p) {
            if let Ok(m) = md.modified() {
                *best = (*best).max(m);
            }
            if md.is_dir() {
                if let Ok(rd) = std::fs::read_dir(p) {
                    for e in rd.flatten() {
                        walk(&e.path(), best);
                    }
                }
            }
        }
    }
    let mut best = UNIX_EPOCH;
    for p in paths {
        walk(p, &mut best);
    }
    best
}

fn create(args: CreateArgs) -> Result<()> {
    let cfg = match &args.config {
        Some(p) => Config::load(p)?,
        None => Config::default(),
    };
    let output = args
        .output
        .clone()
        .or(cfg.output.clone())
        .context("no output file given (use -o/--output)")?;
    if output.exists() && !args.overwrite {
        bail!(
            "{} already exists (use --overwrite to replace it)",
            output.display()
        );
    }
    let volname = args
        .volname
        .clone()
        .or(cfg.volume_name.clone())
        .or_else(|| output.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .context("cannot determine a volume name")?;

    let mut b = DmgBuilder::new(volname);
    if let Some(f) = args.format.as_ref().or(cfg.format.as_ref()) {
        b.format(f.parse::<Format>()?);
    }
    if let Some(l) = args.level.or(cfg.level) {
        b.level(l);
    }
    if let Some(fs) = args.filesystem.as_ref().or(cfg.filesystem.as_ref()) {
        match fs.to_ascii_lowercase().as_str() {
            "hfs+" | "hfsplus" | "hfs" | "jhfs+" => b.case_sensitive(false),
            "hfsx" | "case-sensitive" | "hfs+x" | "jhfsx" => b.case_sensitive(true),
            "apfs" => {
                bail!("APFS images are not supported; use hfs+ (readable by every macOS version)")
            }
            other => bail!("unknown file system {other:?} (expected hfs+ or hfsx)"),
        };
    }
    if let Some(l) = args.layout.as_ref().or(cfg.layout.as_ref()) {
        b.partition_scheme(l.parse::<PartitionScheme>()?);
    }
    if let Some(s) = args.size.map(Ok).or(cfg.size.as_ref().map(|s| s.bytes())) {
        b.size(s?);
    }
    if let Some(s) = args
        .free_space
        .map(Ok)
        .or(cfg.free_space.as_ref().map(|s| s.bytes()))
    {
        b.free_space(s?);
    }

    // Contents.
    let mut all_sources = Vec::new();
    for dir in cfg.contents.iter().chain(&args.srcfolder) {
        b.add_contents(dir.clone());
        all_sources.push(dir.clone());
    }
    for src in cfg.files.iter().chain(&args.sources) {
        if !src.exists() {
            bail!("{} does not exist", src.display());
        }
        b.add_path(src.clone());
        all_sources.push(src.clone());
    }
    for (dest, src) in &cfg.copy {
        b.add_path_as(src.clone(), dest.clone());
        all_sources.push(src.clone());
    }
    for c in &args.copy {
        let (dest, src) = split_kv(c, "--copy")?;
        b.add_path_as(PathBuf::from(&src), dest);
        all_sources.push(PathBuf::from(src));
    }
    if all_sources.is_empty() && cfg.symlinks.is_empty() && args.symlink.is_empty() {
        bail!("nothing to put in the image: give files, --srcfolder or --copy");
    }
    if args.applications_link || cfg.applications_link.unwrap_or(false) {
        b.add_applications_link();
    }
    for s in &cfg.symlinks {
        b.add_symlink(s.name.clone(), s.target.clone());
    }
    for s in &args.symlink {
        let (name, target) = split_kv(s, "--symlink")?;
        b.add_symlink(name, target);
    }
    for p in cfg.exclude.iter().chain(&args.exclude) {
        b.exclude(p.clone());
    }
    if let Some(o) = args.owner.as_ref().or(cfg.owner.as_ref()) {
        let (u, g) = o
            .split_once(':')
            .and_then(|(u, g)| Some((u.parse().ok()?, g.parse().ok()?)))
            .context("--owner must be UID:GID")?;
        b.import_options().owner = Some((u, g));
    }
    if args.xattrs {
        b.import_options().xattrs = true;
    } else if args.no_xattrs {
        b.import_options().xattrs = false;
    } else if let Some(x) = cfg.xattrs {
        b.import_options().xattrs = x;
    }
    if let Some(icon) = args.icon.as_ref().or(cfg.icon.as_ref()) {
        b.volume_icon(icon.clone());
    }
    if args.open_folder || cfg.open_folder.unwrap_or(false) {
        b.open_folder(true);
    }
    for h in cfg.hide.iter().chain(&args.hide) {
        b.hide(h.clone());
    }
    for h in cfg.hide_extension.iter().chain(&args.hide_extension) {
        b.hide_extension(h.clone());
    }
    if args.reproducible || cfg.reproducible.unwrap_or(false) {
        let t = match std::env::var("SOURCE_DATE_EPOCH") {
            Ok(v) => {
                UNIX_EPOCH
                    + Duration::from_secs(v.trim().parse().context("invalid SOURCE_DATE_EPOCH")?)
            }
            Err(_) => newest_mtime(&all_sources),
        };
        b.reproducible(t);
    }

    // Window layout.
    let w = cfg.window.clone().unwrap_or_default();
    let wants_layout = cfg.window.is_some()
        || args.background.is_some()
        || args.background_color.is_some()
        || args.window_pos.is_some()
        || args.window_size.is_some()
        || args.icon_size.is_some()
        || args.text_size.is_some()
        || args.label_right
        || !args.position.is_empty();
    if wants_layout {
        let mut layout = WindowLayout::default();
        let d = WindowLayout::default();
        layout.x = w.x.unwrap_or(d.x);
        layout.y = w.y.unwrap_or(d.y);
        layout.width = w.width.unwrap_or(d.width);
        layout.height = w.height.unwrap_or(d.height);
        layout.icon_size = args.icon_size.or(w.icon_size).unwrap_or(d.icon_size);
        layout.text_size = args.text_size.or(w.text_size).unwrap_or(d.text_size);
        layout.label_on_bottom = !(args.label_right
            || w.label_position
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case("right")));
        layout.show_item_info = w.show_item_info.unwrap_or(d.show_item_info);
        layout.show_icon_preview = w.show_icon_preview.unwrap_or(d.show_icon_preview);
        layout.show_status_bar = w.show_status_bar.unwrap_or(d.show_status_bar);
        layout.show_toolbar = w.show_toolbar.unwrap_or(d.show_toolbar);
        layout.show_sidebar = w.show_sidebar.unwrap_or(d.show_sidebar);
        layout.show_pathbar = w.show_pathbar.unwrap_or(d.show_pathbar);
        layout.grid_spacing = w.grid_spacing.unwrap_or(d.grid_spacing);
        layout.arrange_by = w.arrange_by.clone().unwrap_or(d.arrange_by);
        if let Some(p) = &args.window_pos {
            (layout.x, layout.y) = pair(p, "--window-pos")?;
        }
        if let Some(s) = &args.window_size {
            (layout.width, layout.height) = pair(s, "--window-size")?;
        }
        let color = args
            .background_color
            .as_ref()
            .or(w.background_color.as_ref());
        layout.background = match (args.background.as_ref().or(w.background.as_ref()), color) {
            (Some(img), _) => Background::Image(img.clone()),
            (None, Some(c)) => Background::from_hex(c)
                .with_context(|| format!("invalid background color {c:?} (expected #RRGGBB)"))?,
            (None, None) => Background::Default,
        };
        for (name, [x, y]) in &w.icons {
            layout.icons.push(IconPosition {
                name: name.clone(),
                x: *x,
                y: *y,
            });
        }
        for p in &args.position {
            let (name, xy) = p
                .rsplit_once('=')
                .with_context(|| format!("--position must be NAME=X,Y, got {p:?}"))?;
            let (x, y) = pair(xy, "--position")?;
            layout.icons.retain(|i| i.name != name);
            layout.icons.push(IconPosition {
                name: name.to_owned(),
                x,
                y,
            });
        }
        b.window(layout);
    }

    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    if show_progress {
        b.progress(|done, total| {
            let pct = (done * 100).checked_div(total).unwrap_or(100);
            eprint!("\rwriting {pct:3}%");
        });
    }
    let start = std::time::Instant::now();
    let summary = b
        .write(&output)
        .with_context(|| format!("creating {}", output.display()))?;
    if show_progress {
        eprint!("\r            \r");
    }
    if !args.quiet {
        let virt = summary.koly.sector_count * 512;
        eprintln!(
            "created {} ({}, {} volume, {} image) in {:.1}s",
            output.display(),
            b_format(&args, &cfg),
            inspect::human(virt),
            inspect::human(summary.image_size),
            start.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

fn b_format(args: &CreateArgs, cfg: &Config) -> String {
    args.format
        .as_ref()
        .or(cfg.format.as_ref())
        .and_then(|f| f.parse::<Format>().ok())
        .unwrap_or_default()
        .to_string()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Create(a) => create(*a),
        Command::Info(a) => inspect::info(a),
        Command::Ls(a) => inspect::ls(a),
        Command::Cat(a) => inspect::cat(a),
        Command::Extract(a) => inspect::extract(a),
        Command::Verify(a) => inspect::verify(a),
        Command::Convert(a) => inspect::convert(a),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dmgr: {e:#}");
            ExitCode::FAILURE
        }
    }
}
