//! Classic Mac OS alias records (version 2), as stored in `.DS_Store`
//! `icvp.backgroundImageAlias` to reference a window background picture.

use crate::bytes::PutBe;

/// Disk type: ejectable (what Finder records for mounted disk images).
const DISK_EJECTABLE: u16 = 5;

/// Description of the alias target.
#[derive(Clone, Debug)]
pub struct AliasTarget {
    pub volume_name: String,
    /// Volume creation date (HFS seconds).
    pub volume_created: u32,
    /// Whether the target is a folder.
    pub is_folder: bool,
    /// CNID of the target's parent folder.
    pub parent_cnid: u32,
    /// CNID of the target.
    pub cnid: u32,
    /// Target creation date (HFS seconds).
    pub created: u32,
    /// Path components from the volume root to the target (inclusive).
    pub components: Vec<String>,
    /// CNIDs of the folders containing the target, innermost first, excluding the root.
    pub ancestor_cnids: Vec<u32>,
}

/// Encodes `s` as a Mac Roman-ish byte string (non-ASCII becomes `?`), with
/// `:` replaced by `/` as the Carbon File Manager expects.
fn carbon(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| match c {
            ':' => b'/',
            c if c.is_ascii() => c as u8,
            _ => b'?',
        })
        .collect()
}

fn pascal(out: &mut Vec<u8>, s: &[u8], field: usize) {
    let n = s.len().min(field - 1);
    out.push(n as u8);
    out.extend_from_slice(&s[..n]);
    out.put_zeros(field - 1 - n);
}

fn tag(out: &mut Vec<u8>, t: i16, data: &[u8]) {
    out.put_i16(t);
    out.put_u16(data.len() as u16);
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

fn utf16_counted(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut v = Vec::with_capacity(2 + units.len() * 2);
    v.put_u16(units.len() as u16);
    for u in units {
        v.put_u16(u);
    }
    v
}

/// Builds a version 2 alias record pointing at `t`.
pub fn alias_record(t: &AliasTarget) -> Vec<u8> {
    let filename = t.components.last().map(String::as_str).unwrap_or("");
    let mut o = Vec::with_capacity(512);
    o.put_u32(0); // application-specific
    o.put_u16(0); // record size, patched below
    o.put_u16(2); // version
    o.put_u16(u16::from(t.is_folder));
    pascal(&mut o, &carbon(&t.volume_name), 28);
    o.put_u32(t.volume_created);
    o.extend_from_slice(b"H+");
    o.put_u16(DISK_EJECTABLE);
    o.put_u32(t.parent_cnid);
    pascal(&mut o, &carbon(filename), 64);
    o.put_u32(t.cnid);
    o.put_u32(t.created);
    o.put_u32(0); // file type
    o.put_u32(0); // creator
    o.put_i16(-1); // nlvlFrom
    o.put_i16(-1); // nlvlTo
    o.put_u32(0); // volume attributes
    o.put_u16(0); // volume file system ID
    o.put_zeros(10);
    debug_assert_eq!(o.len(), 150);

    if t.components.len() >= 2 {
        let folder = &t.components[t.components.len() - 2];
        tag(&mut o, 0, &carbon(folder));
    }
    if !t.ancestor_cnids.is_empty() {
        let mut ids = Vec::new();
        for c in &t.ancestor_cnids {
            ids.put_u32(*c);
        }
        tag(&mut o, 1, &ids);
    }
    let mut carbon_path = carbon(&t.volume_name);
    for c in &t.components {
        carbon_path.push(b':');
        carbon_path.extend(carbon(c));
    }
    tag(&mut o, 2, &carbon_path);
    tag(&mut o, 14, &utf16_counted(filename));
    tag(&mut o, 15, &utf16_counted(&t.volume_name));
    let mut hi = Vec::new();
    hi.put_u64(u64::from(t.volume_created) << 16);
    tag(&mut o, 16, &hi);
    let mut hi = Vec::new();
    hi.put_u64(u64::from(t.created) << 16);
    tag(&mut o, 17, &hi);
    let posix = format!("/{}", t.components.join("/"));
    tag(&mut o, 18, posix.as_bytes());
    let mount = format!("/Volumes/{}", t.volume_name);
    tag(&mut o, 19, mount.as_bytes());
    o.put_i16(-1);
    o.put_u16(0);

    let len = o.len() as u16;
    o[4..6].copy_from_slice(&len.to_be_bytes());
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let a = alias_record(&AliasTarget {
            volume_name: "My App".into(),
            volume_created: 0x1234_5678,
            is_folder: false,
            parent_cnid: 17,
            cnid: 18,
            created: 0x1234_5678,
            components: vec![".background".into(), "bg.png".into()],
            ancestor_cnids: vec![17],
        });
        assert_eq!(u16::from_be_bytes([a[4], a[5]]) as usize, a.len());
        assert_eq!(&a[10..17], b"\x06My App");
        assert_eq!(&a[42..44], b"H+");
        assert_eq!(u32::from_be_bytes(a[46..50].try_into().unwrap()), 17);
        assert_eq!(&a[50..57], b"\x06bg.png");
        assert_eq!(u32::from_be_bytes(a[114..118].try_into().unwrap()), 18);
        assert_eq!(&a[a.len() - 4..], &[0xFF, 0xFF, 0, 0]);
        let s = String::from_utf8_lossy(&a);
        assert!(s.contains("My App:.background:bg.png"));
        assert!(s.contains("/.background/bg.png"));
        assert!(s.contains("/Volumes/My App"));
    }
}
