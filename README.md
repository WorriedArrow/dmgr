# dmgr

[![CI](https://github.com/worriedarrow/dmgr/actions/workflows/ci.yml/badge.svg)](https://github.com/worriedarrow/dmgr/actions/workflows/ci.yml)

Create, inspect, verify and extract macOS disk images (`.dmg`) on Linux, Windows and macOS. dmgr is written in Rust with no C dependencies and does not need `hdiutil`.

```sh
dmgr create -o "My App.dmg" "dist/My App.app" --applications-link
```

dmgr writes UDIF images that contain an HFS+ volume inside a GUID partition table, the same layout `hdiutil create -srcfolder` produces. Every push is checked on GitHub's macOS runners: `hdiutil imageinfo`, `hdiutil verify`, mounting, `fsck_hfs`, a byte-for-byte comparison of the mounted contents, and a Finder check of the window layout. Images built on the Linux and Windows runners go through the same checks.

## Install

```sh
cargo install --locked --git https://github.com/worriedarrow/dmgr dmgr-cli
```

Tagged versions also get prebuilt binaries for Linux, macOS and Windows (x86_64 and arm64) on the [releases page](https://github.com/worriedarrow/dmgr/releases).

## What it supports

| | |
|---|---|
| Image formats | UDZO (zlib), ULFO (LZFSE), ULMO (LZMA), UDBZ (bzip2), UDRO, UDRW |
| File systems | HFS+ (case-insensitive), HFSX (case-sensitive) |
| Partition layouts | GPT (default, like hdiutil), none (bare volume) |
| Contents | files, folders, symlinks, permissions, owners, timestamps, extended attributes, resource forks, Finder info |
| Finder window | size and position, icon size, icon positions, background picture or color, volume icon, hidden extensions, auto-open |
| Reading | UDIF images from hdiutil (all formats above plus ADC), raw images, GPT/APM/MBR, HFS+ hard links and decmpfs-compressed files |
| Output | reproducible byte-for-byte when `--reproducible` or `SOURCE_DATE_EPOCH` is used |

Not supported: APFS volumes, encrypted images, segmented images, license agreements (SLA) and code signing. To sign a DMG on any platform, run [`rcodesign`](https://github.com/indygreg/apple-platform-rs) on the image dmgr wrote.

## Usage

### Create

```sh
# Put a folder's contents at the root of the volume (like hdiutil -srcfolder)
dmgr create -o out.dmg -V "My Volume" -s build/stage

# A drag-to-install image with a styled window
dmgr create -o "My App.dmg" -V "My App" "dist/My App.app" \
  --applications-link \
  --icon assets/volume.icns \
  --background assets/background.png \
  --window-pos 200,120 --window-size 640,400 --icon-size 128 \
  --position "My App.app=160,190" --position "Applications=480,190" \
  --format ULFO
```

`dmgr create --help` lists every option. Useful ones:

- `-f, --format` picks UDZO (default, readable by every macOS), ULFO (macOS 10.11+, faster to open), ULMO (macOS 10.15+, smallest), UDBZ, UDRO or UDRW (mountable read-write).
- `--fs hfsx` makes the volume case-sensitive. On the default HFS+, two names that differ only in case are an error, because macOS could not tell them apart.
- `--size 200m` and `--free-space 50m` control the volume size. By default the volume fits its contents with a little slack.
- `--copy DEST=SRC` places a file or folder at a specific path, `--symlink NAME=TARGET` adds any symlink, and `--exclude "*.dSYM"` skips matching names.
- `--owner 0:80` forces ownership. Without it, Unix hosts keep source ownership and Windows uses 99:99, which macOS treats as "current user" on disk images.
- `--reproducible` fixes timestamps and identifiers so the same input gives the same bytes. It uses `SOURCE_DATE_EPOCH` when that is set.

On Windows there are no Unix permission bits. dmgr marks a file executable when it starts with a Mach-O or universal-binary header or a `#!` line, or when it sits in a `Contents/MacOS` folder.

### Configuration file

Everything above can also go in a TOML file, with paths relative to the file:

```toml
output = "dist/My App.dmg"
volume-name = "My App"
format = "ULFO"
files = ["dist/My App.app"]
applications-link = true
icon = "assets/volume.icns"
exclude = ["*.dSYM"]

[window]
x = 200
y = 120
width = 640
height = 400
icon-size = 128
background = "assets/background.png"

[window.icons]
"My App.app" = [160, 190]
"Applications" = [480, 190]
```

```sh
dmgr create -c dmgr.toml
```

### Inspect and extract

```sh
dmgr info app.dmg            # format, block tables, partitions, volume details
dmgr info --chunks app.dmg   # every compressed chunk
dmgr ls -lR app.dmg          # list files with modes, owners and sizes
dmgr cat app.dmg "My App.app/Contents/Info.plist"
dmgr extract app.dmg out/    # extract everything (add --all for Finder metadata)
dmgr verify app.dmg          # checksums, file system consistency, every file readable
dmgr convert app.dmg -o app-lzfse.dmg -f ULFO
```

`dmgr verify` recomputes the UDIF checksums and runs dmgr's own HFS+ checker. The checker covers B-tree ordering and linkage, index keys, node bitmaps, thread records, folder valences and the allocation bitmap, so images can be validated without macOS.

## Library

The `dmgr` crate exposes the same functionality:

```rust
use dmgr::{DmgBuilder, Format};
use dmgr::finder::{IconPosition, WindowLayout};

DmgBuilder::new("My App")
    .format(Format::Ulfo)
    .add_path("dist/My App.app")
    .add_applications_link()
    .window(WindowLayout {
        icons: vec![
            IconPosition { name: "My App.app".into(), x: 160, y: 190 },
            IconPosition { name: "Applications".into(), x: 480, y: 190 },
        ],
        ..Default::default()
    })
    .write("My App.dmg")?;
```

Reading an image:

```rust
let volume = dmgr::DiskImage::open("My App.dmg")?.into_volume()?;
for entry in volume.list(volume.root().cnid) {
    println!("{} ({} bytes)", entry.name, entry.data_size());
}
```

The lower layers are public too: `hfs::VolumeBuilder` builds an HFS+ volume from an in-memory tree, `udif::write` turns any `disk::VirtualDisk` into a UDIF image, `partition` lays out GPT, and `finder::dsstore` reads and writes `.DS_Store` files.

Cargo features: `parallel` (compress chunks on all cores), `bzip2`, `lzfse` and `lzma` are on by default. `serde` derives (de)serialization for the layout types.

## How images are built

The builder assigns catalog node IDs, sorts catalog keys with Apple's `FastUnicodeCompare` case-folding table (or binary order for HFSX), and packs the catalog, extents and attributes B-trees. It then places every file contiguously, so no file needs overflow extents. The result is a sparse virtual disk where file contents point back at the source files, so a multi-gigabyte image never sits in memory. The UDIF writer streams that disk in 1 MiB chunks, compresses them in parallel, stores all-zero chunks as zero-fill entries, and writes the `blkx` property list and `koly` trailer with CRC-32 checksums.

## Testing

`cargo test --workspace` runs unit tests and round-trip tests: build images in every format, layout and case mode, read them back, compare contents, and run the checker. CI runs the tests on Linux (x86_64 and arm64), Windows and macOS, then runs [`scripts/conformance.sh`](scripts/conformance.sh) on macOS 26, macOS 15 (arm64) and macOS 15 (Intel). That script:

- builds a fixture with an app bundle, Unicode names, symlinks, 3000 files, deep nesting, sparse and incompressible data, xattrs and a resource fork;
- creates images in every format, file system and layout, then checks each with `hdiutil imageinfo`, `hdiutil verify`, a mount and content comparison, and `fsck_hfs`;
- writes to a mounted UDRW image on macOS and checks it again with `fsck_hfs` and `dmgr verify`;
- checks the Finder layout with the `ds_store`/`mac_alias` Python packages and asks Finder for the window bounds and icon positions;
- runs `dmgr verify`, `extract` and `convert` on images made by `hdiutil create`, including HFS-compressed files;
- runs the same checks on images built by dmgr on the Linux and Windows runners.

## License

MIT or Apache-2.0, at your option.

The HFS+ case-folding tables in `crates/dmgr/src/hfs/case_table.rs` are data from Apple's open-source HFS implementation (APSL 2.0).
