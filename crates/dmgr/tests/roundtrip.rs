//! End-to-end tests: build images, read them back, and check them.

use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dmgr::finder::{Background, IconPosition, WindowLayout, dsstore};
use dmgr::hfs::format::finder_flags;
use dmgr::hfs::{EntryKind, Metadata, Volume, VolumeBuilder, VolumeOptions, check};
use dmgr::io::{ReadAt, SeekReader, Slice};
use dmgr::udif::UdifImage;
use dmgr::{DiskImage, DmgBuilder, Format, PartitionScheme};

fn epoch() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// Builds a fixture tree and returns it.
fn fixture(dir: &Path) {
    let app = dir.join("Demo.app/Contents");
    fs::create_dir_all(app.join("MacOS")).unwrap();
    fs::create_dir_all(app.join("Resources/en.lproj")).unwrap();
    fs::write(app.join("Info.plist"), b"<plist><dict/></plist>").unwrap();
    // Mach-O magic so non-unix hosts mark it executable.
    let mut bin = vec![0xCF, 0xFA, 0xED, 0xFE];
    bin.extend((0..300_000u32).flat_map(|i| (i.wrapping_mul(2_654_435_761)).to_le_bytes()));
    fs::write(app.join("MacOS/Demo"), &bin).unwrap();
    fs::write(
        app.join("Resources/en.lproj/Localizable.strings"),
        "\"a\" = \"b\";",
    )
    .unwrap();
    fs::write(dir.join("README.txt"), "hello\n".repeat(1000)).unwrap();
    fs::write(dir.join("empty"), b"").unwrap();
    fs::write(dir.join("Ünïcödé ✓ 日本語.txt"), b"unicode").unwrap();
    // Windows cannot create ':' in names.
    let _ = fs::write(dir.join("colon:name"), b"colon");
    fs::create_dir_all(dir.join("nested/a/b/c/d")).unwrap();
    fs::write(dir.join("nested/a/b/c/d/deep.txt"), b"deep").unwrap();
    fs::write(dir.join("zeros.bin"), vec![0u8; 3 << 20]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(app.join("MacOS/Demo"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("Demo.app/Contents/Info.plist", dir.join("link")).unwrap();
    }
}

/// Collects `path -> (kind, contents or link target)` from a volume.
fn snapshot<R: ReadAt>(vol: &mut Volume<R>) -> BTreeMap<String, (String, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(vol.root().cnid, String::new())];
    while let Some((cnid, prefix)) = stack.pop() {
        let children: Vec<_> = vol.list(cnid).into_iter().cloned().collect();
        for e in children {
            let path = format!("{prefix}/{}", e.name);
            match e.kind() {
                EntryKind::Directory => {
                    out.insert(path.clone(), ("dir".into(), Vec::new()));
                    stack.push((e.cnid, path));
                }
                EntryKind::Symlink => {
                    let t = vol.read_link(&e).unwrap();
                    out.insert(path, ("link".into(), t.into_bytes()));
                }
                _ => {
                    let data = vol.read_file(&e).unwrap();
                    assert_eq!(data.len() as u64, e.data_size());
                    out.insert(path, ("file".into(), data));
                }
            }
        }
    }
    out
}

fn host_snapshot(dir: &Path) -> BTreeMap<String, (String, Vec<u8>)> {
    let mut out = BTreeMap::new();
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, (String, Vec<u8>)>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            let name = dmgr::hfs::unicode::decompose(&name);
            let path = format!("{prefix}/{name}");
            let ft = e.file_type().unwrap();
            if ft.is_symlink() {
                let t = fs::read_link(e.path()).unwrap();
                out.insert(
                    path,
                    ("link".into(), t.to_str().unwrap().as_bytes().to_vec()),
                );
            } else if ft.is_dir() {
                out.insert(path.clone(), ("dir".into(), Vec::new()));
                walk(&e.path(), &path, out);
            } else {
                out.insert(path, ("file".into(), fs::read(e.path()).unwrap()));
            }
        }
    }
    walk(dir, "", &mut out);
    out
}

fn open(bytes: Vec<u8>) -> DiskImage {
    let src = SeekReader::new(Cursor::new(bytes)).unwrap();
    // DiskImage expects a file-backed source; go through a temp file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.dmg");
    fs::write(&path, src.into_inner().into_inner()).unwrap();
    let img = DiskImage::open(&path).unwrap();
    std::mem::forget(dir);
    img
}

fn check_image(path: &Path) -> Volume<Slice<dmgr::ImageSource>> {
    let mut img = DiskImage::open(path).unwrap();
    let report = img.verify_checksums().unwrap().expect("UDIF");
    assert!(report.ok(), "checksum mismatch: {report:?}");
    let part = img.hfs_partition().unwrap().clone();
    let mut vol = img.into_volume().unwrap();
    let errs = check::check(&mut vol, part.size).unwrap();
    assert!(errs.is_empty(), "fsck errors: {errs:#?}");
    vol
}

#[test]
fn all_formats_roundtrip() {
    let src = tempfile::tempdir().unwrap();
    fixture(src.path());
    let expected = host_snapshot(src.path());
    let out = tempfile::tempdir().unwrap();
    for format in Format::ALL {
        for scheme in [PartitionScheme::Gpt, PartitionScheme::None] {
            for cs in [false, true] {
                let path = out.path().join(format!("{format}-{scheme}-{cs}.dmg"));
                let mut b = DmgBuilder::new("Demo Volume");
                b.format(format)
                    .partition_scheme(scheme)
                    .case_sensitive(cs)
                    .add_contents(src.path());
                b.write(&path).unwrap();
                let mut vol = check_image(&path);
                assert_eq!(vol.name(), "Demo Volume");
                assert_eq!(vol.header().is_hfsx(), cs);
                let got = snapshot(&mut vol);
                assert_eq!(
                    got.keys().collect::<Vec<_>>(),
                    expected.keys().collect::<Vec<_>>()
                );
                assert_eq!(got, expected, "{format} {scheme} cs={cs}");
                let demo = vol.lookup("Demo.app/Contents/MacOS/Demo").unwrap();
                assert_eq!(demo.mode() & 0o777, 0o755, "executable bit");
                let readme = vol.lookup("readme.TXT");
                assert_eq!(readme.is_some(), !cs, "case sensitivity lookup");
            }
        }
    }
}

#[test]
fn compressed_images_are_small() {
    let src = tempfile::tempdir().unwrap();
    fs::write(
        src.path().join("big.txt"),
        "the quick brown fox ".repeat(500_000),
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    for format in [Format::Udzo, Format::Ulfo, Format::Ulmo, Format::Udbz] {
        let path = out.path().join(format!("{format}.dmg"));
        DmgBuilder::new("Small")
            .format(format)
            .add_contents(src.path())
            .write(&path)
            .unwrap();
        let len = fs::metadata(&path).unwrap().len();
        assert!(len < 1_000_000, "{format} image is {len} bytes");
        check_image(&path);
    }
}

#[test]
fn reproducible_builds_are_identical() {
    let src = tempfile::tempdir().unwrap();
    fixture(src.path());
    let build = || {
        let mut v = Vec::new();
        DmgBuilder::new("Repro")
            .reproducible(epoch())
            .add_contents(src.path())
            .add_applications_link()
            .write_to(&mut v)
            .unwrap();
        v
    };
    let a = build();
    let b = build();
    assert_eq!(a.len(), b.len());
    assert!(a == b, "reproducible builds differ");
    let img = open(a);
    let vol = img.into_volume().unwrap();
    assert_eq!(vol.header().create_date, dmgr::hfs::time::to_hfs(epoch()));
}

#[test]
fn many_files_multi_level_catalog() {
    let mut vb = VolumeBuilder::new(VolumeOptions::new("Many")).unwrap();
    let root = vb.root();
    let mut names = Vec::new();
    for d in 0..20 {
        let dir = vb
            .add_dir(root, &format!("dir {d}"), Metadata::dir(epoch()))
            .unwrap();
        for f in 0..400 {
            let name = format!("File_{f:04}_with_a_reasonably_long_name_{d}.dat");
            vb.add_file(
                dir,
                &name,
                format!("{d}/{f}").into_bytes(),
                Metadata::file(epoch()),
            )
            .unwrap();
            names.push((d, name));
        }
    }
    let built = vb.build().unwrap();
    let mut r = built.disk.reader();
    let mut vol = Volume::open(&mut r).unwrap();
    assert!(vol.catalog().header().tree_depth >= 2);
    let size = built.disk.size();
    let errs = check::check(&mut vol, size).unwrap();
    assert!(errs.is_empty(), "{errs:#?}");
    for (d, name) in names.iter().step_by(97) {
        let e = vol.lookup(&format!("dir {d}/{name}")).unwrap().clone();
        let data = vol.read_file(&e).unwrap();
        let f: u32 = name[5..9].parse().unwrap();
        assert_eq!(data, format!("{d}/{f}").into_bytes());
    }
}

#[test]
fn case_conflicts_are_rejected_on_hfsplus() {
    let mut vb = VolumeBuilder::new(VolumeOptions::new("Case")).unwrap();
    let root = vb.root();
    vb.add_file(root, "README", b"a".to_vec(), Metadata::file(epoch()))
        .unwrap();
    let err = vb
        .add_file(root, "readme", b"b".to_vec(), Metadata::file(epoch()))
        .unwrap_err();
    assert!(err.to_string().contains("case-insensitive"), "{err}");
    // Composed vs decomposed é are the same name on HFS+.
    vb.add_file(root, "caf\u{e9}", b"a".to_vec(), Metadata::file(epoch()))
        .unwrap();
    assert!(
        vb.add_file(root, "cafe\u{301}", b"a".to_vec(), Metadata::file(epoch()))
            .is_err()
    );

    let mut opts = VolumeOptions::new("Case");
    opts.case_sensitive = true;
    let mut vb = VolumeBuilder::new(opts).unwrap();
    let root = vb.root();
    vb.add_file(root, "README", b"a".to_vec(), Metadata::file(epoch()))
        .unwrap();
    vb.add_file(root, "readme", b"b".to_vec(), Metadata::file(epoch()))
        .unwrap();
    let built = vb.build().unwrap();
    let mut r = built.disk.reader();
    let mut vol = Volume::open(&mut r).unwrap();
    assert!(
        check::check(&mut vol, built.disk.size())
            .unwrap()
            .is_empty()
    );
    assert_eq!(vol.list(2).len(), 2);
}

#[test]
fn xattrs_resource_forks_and_finder_info() {
    let mut vb = VolumeBuilder::new(VolumeOptions::new("Attrs")).unwrap();
    let root = vb.root();
    let mut m = Metadata::file(epoch());
    m.xattrs = vec![
        ("com.example.small".into(), b"value".to_vec()),
        ("com.example.big".into(), vec![7u8; 100_000]),
        ("com.apple.ResourceFork".into(), b"rsrc data".to_vec()),
        ("com.apple.FinderInfo".into(), {
            let mut fi = vec![0u8; 32];
            fi[0..4].copy_from_slice(b"TEXT");
            fi[4..8].copy_from_slice(b"ttxt");
            fi
        }),
    ];
    vb.add_file(root, "file.txt", b"data".to_vec(), m).unwrap();
    let mut dm = Metadata::dir(epoch());
    dm.xattrs = vec![("com.example.dir".into(), b"d".to_vec())];
    vb.add_dir(root, "folder", dm).unwrap();
    let built = vb.build().unwrap();
    let mut r = built.disk.reader();
    let mut vol = Volume::open(&mut r).unwrap();
    let errs = check::check(&mut vol, built.disk.size()).unwrap();
    assert!(errs.is_empty(), "{errs:#?}");
    let f = vol.lookup("file.txt").unwrap().clone();
    let mut x = vol.xattrs(f.cnid).unwrap();
    x.sort();
    assert_eq!(x.len(), 2);
    assert_eq!(x[0], ("com.example.big".into(), vec![7u8; 100_000]));
    assert_eq!(x[1], ("com.example.small".into(), b"value".to_vec()));
    assert_eq!(vol.read_resource_fork(&f).unwrap(), b"rsrc data");
    assert_eq!(&f.finder_info()[0..8], b"TEXTttxt");
    let d = vol.lookup("folder").unwrap().clone();
    assert_eq!(vol.xattrs(d.cnid).unwrap().len(), 1);
}

#[test]
fn finder_layout_background_and_icon() {
    let src = tempfile::tempdir().unwrap();
    fixture(src.path());
    let bg = src.path().join("bg.png");
    fs::write(&bg, b"\x89PNG fake").unwrap();
    let icns = src.path().join("vol.icns");
    fs::write(&icns, b"icns fake").unwrap();
    let out = tempfile::tempdir().unwrap();
    let path = out.path().join("styled.dmg");
    DmgBuilder::new("Styled")
        .add_path(src.path().join("Demo.app"))
        .add_applications_link()
        .volume_icon(&icns)
        .open_folder(true)
        .hide_extension("Demo.app")
        .window(WindowLayout {
            width: 600,
            height: 380,
            background: Background::Image(bg.clone()),
            icons: vec![
                IconPosition {
                    name: "Demo.app".into(),
                    x: 150,
                    y: 190,
                },
                IconPosition {
                    name: "Applications".into(),
                    x: 450,
                    y: 190,
                },
            ],
            ..Default::default()
        })
        .write(&path)
        .unwrap();
    let mut vol = check_image(&path);
    assert_eq!(vol.header().finder_info[2], 2, "open folder");
    assert!(vol.root().finder_flags() & finder_flags::HAS_CUSTOM_ICON != 0);
    let bgdir = vol.lookup(".background").unwrap().clone();
    assert!(bgdir.finder_flags() & finder_flags::IS_INVISIBLE != 0);
    let img = vol.lookup(".background/bg.png").unwrap().clone();
    let app = vol.lookup("Demo.app").unwrap().clone();
    assert!(app.finder_flags() & finder_flags::EXTENSION_HIDDEN != 0);
    assert_eq!(
        vol.read_link(&vol.lookup("Applications").unwrap().clone())
            .unwrap(),
        "/Applications"
    );
    let icon = vol.lookup(".VolumeIcon.icns").unwrap().clone();
    assert_eq!(vol.read_file(&icon).unwrap(), b"icns fake");

    let ds = vol.lookup(".DS_Store").unwrap().clone();
    let recs = dsstore::read(&vol.read_file(&ds).unwrap()).unwrap();
    let iloc: Vec<_> = recs.iter().filter(|r| &r.code == b"Iloc").collect();
    assert_eq!(iloc.len(), 2);
    let icvp = recs.iter().find(|r| &r.code == b"icvp").unwrap();
    let dsstore::Value::Blob(blob) = &icvp.value else {
        panic!()
    };
    let pl = plist::Value::from_reader(Cursor::new(blob)).unwrap();
    let d = pl.as_dictionary().unwrap();
    assert_eq!(d["backgroundType"].as_signed_integer(), Some(2));
    let alias = d["backgroundImageAlias"].as_data().unwrap();
    assert_eq!(
        u32::from_be_bytes(alias[46..50].try_into().unwrap()),
        bgdir.cnid
    );
    assert_eq!(
        u32::from_be_bytes(alias[114..118].try_into().unwrap()),
        img.cnid
    );
}

#[test]
fn explicit_size_and_free_space() {
    let mut b = DmgBuilder::new("Sized");
    b.format(Format::Udro)
        .add_file("a.txt", b"x".to_vec(), 0o644)
        .size(64 << 20);
    let mut v = Vec::new();
    b.write_to(&mut v).unwrap();
    let img = open(v);
    let part = img.hfs_partition().unwrap().clone();
    assert_eq!(part.size, 64 << 20);
    let vol = img.into_volume().unwrap();
    assert!(u64::from(vol.header().free_blocks) * 4096 > 60 << 20);

    let mut b = DmgBuilder::new("Tiny");
    b.add_file("a.bin", vec![1u8; 2 << 20], 0o644).size(1 << 20);
    assert!(b.write_to(&mut Vec::new()).is_err());
}

#[test]
fn convert_between_formats() {
    let src = tempfile::tempdir().unwrap();
    fixture(src.path());
    let out = tempfile::tempdir().unwrap();
    let a = out.path().join("a.dmg");
    DmgBuilder::new("Conv")
        .add_contents(src.path())
        .write(&a)
        .unwrap();
    let expected = snapshot(&mut check_image(&a));
    for format in [Format::Ulfo, Format::Udrw, Format::Udzo] {
        let b = out.path().join(format!("b-{format}.dmg"));
        dmgr::convert(&a, &b, &dmgr::udif::WriteOptions::new(format)).unwrap();
        let mut vol = check_image(&b);
        assert_eq!(snapshot(&mut vol), expected);
        let img = DiskImage::open(&b).unwrap();
        assert_eq!(img.udif().unwrap().blkx().len(), 7, "GPT regions preserved");
    }
}

#[test]
fn udrw_stores_every_sector() {
    let mut v = Vec::new();
    DmgBuilder::new("RW")
        .format(Format::Udrw)
        .add_file("a", b"x".to_vec(), 0o644)
        .write_to(&mut v)
        .unwrap();
    let src = SeekReader::new(Cursor::new(v)).unwrap();
    let img = UdifImage::open(src).unwrap();
    let k = img.koly().clone();
    assert_eq!(k.data_fork_length, k.sector_count * 512);
    for b in img.blkx() {
        for c in &b.table.chunks {
            assert!(c.kind == dmgr::udif::chunk::RAW || c.kind == dmgr::udif::chunk::TERMINATOR);
        }
    }
}
