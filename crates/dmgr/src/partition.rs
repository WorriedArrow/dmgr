//! Partition schemes: writing GPT images and reading GPT / APM / MBR maps.

use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

use crate::disk::VirtualDisk;
use crate::error::{Error, Result, corrupt, invalid};
use crate::io::ReadAt;
use crate::udif::{Region, SECTOR_SIZE};

/// GPT partition type GUID for HFS+ (`Apple_HFS`).
pub const APPLE_HFS_GUID: Uuid = uuid::uuid!("48465300-0000-11AA-AA11-00306543ECAC");

/// How the volume is placed in the disk image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
#[non_exhaustive]
pub enum PartitionScheme {
    /// A GUID partition table with one `Apple_HFS` partition (what `hdiutil` uses).
    #[default]
    Gpt,
    /// No partition map: the image is the bare volume (`hdiutil -layout NONE`).
    None,
}

impl fmt::Display for PartitionScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gpt => "GPT",
            Self::None => "NONE",
        })
    }
}

impl FromStr for PartitionScheme {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "gpt" | "guid" | "gpts" | "guid_partition_scheme" => Ok(Self::Gpt),
            "none" | "nopartition" | "bare" => Ok(Self::None),
            _ => Err(invalid(format!(
                "unknown partition scheme {s:?} (expected GPT or NONE)"
            ))),
        }
    }
}

/// A complete disk: contents plus the regions to describe in the image.
#[derive(Clone, Debug)]
pub struct DiskLayout {
    pub disk: VirtualDisk,
    pub regions: Vec<Region>,
}

/// Wraps a volume in the requested partition scheme.
pub fn layout(
    scheme: PartitionScheme,
    volume: &VirtualDisk,
    disk_guid: Uuid,
    partition_guid: Uuid,
) -> Result<DiskLayout> {
    if !volume.size().is_multiple_of(SECTOR_SIZE) {
        return Err(invalid("volume size must be a multiple of 512"));
    }
    match scheme {
        PartitionScheme::None => Ok(DiskLayout {
            disk: volume.clone(),
            regions: vec![Region {
                name: "whole disk (Apple_HFS : 0)".into(),
                id: -1,
                first_sector: 0,
                sector_count: volume.size() / SECTOR_SIZE,
                free: false,
            }],
        }),
        PartitionScheme::Gpt => gpt(volume, disk_guid, partition_guid),
    }
}

const GPT_ENTRIES: u64 = 128;
const GPT_ENTRY_SIZE: u64 = 128;
const GPT_TABLE_SECTORS: u64 = GPT_ENTRIES * GPT_ENTRY_SIZE / SECTOR_SIZE;
/// hdiutil starts the first partition at sector 40.
const FIRST_PARTITION_SECTOR: u64 = 40;

fn gpt(volume: &VirtualDisk, disk_guid: Uuid, part_guid: Uuid) -> Result<DiskLayout> {
    let part_sectors = volume.size() / SECTOR_SIZE;
    let part_start = FIRST_PARTITION_SECTOR;
    let part_end = part_start + part_sectors; // exclusive
    let backup_table = part_end;
    let backup_header = backup_table + GPT_TABLE_SECTORS;
    let total = backup_header + 1;
    let first_usable = 2 + GPT_TABLE_SECTORS;
    let last_usable = backup_table - 1;

    // Partition entry array.
    let mut entries = vec![0u8; (GPT_ENTRIES * GPT_ENTRY_SIZE) as usize];
    entries[0..16].copy_from_slice(&APPLE_HFS_GUID.to_bytes_le());
    entries[16..32].copy_from_slice(&part_guid.to_bytes_le());
    entries[32..40].copy_from_slice(&part_start.to_le_bytes());
    entries[40..48].copy_from_slice(&(part_end - 1).to_le_bytes());
    for (i, u) in "disk image".encode_utf16().enumerate() {
        entries[56 + i * 2..58 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    let entries_crc = crc32fast::hash(&entries);

    let header = |current: u64, backup: u64, table: u64| -> Vec<u8> {
        let mut h = vec![0u8; SECTOR_SIZE as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&current.to_le_bytes());
        h[32..40].copy_from_slice(&backup.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid.to_bytes_le());
        h[72..80].copy_from_slice(&table.to_le_bytes());
        h[80..84].copy_from_slice(&(GPT_ENTRIES as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(GPT_ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };

    // Protective MBR.
    let mut mbr = vec![0u8; SECTOR_SIZE as usize];
    let p = &mut mbr[446..462];
    p[0] = 0x00;
    p[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    p[4] = 0xEE;
    p[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    p[8..12].copy_from_slice(&1u32.to_le_bytes());
    p[12..16].copy_from_slice(&u32::try_from(total - 1).unwrap_or(u32::MAX).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let mut disk = VirtualDisk::new(total * SECTOR_SIZE);
    disk.put(0, mbr)?;
    disk.put(SECTOR_SIZE, header(1, backup_header, 2))?;
    disk.put(2 * SECTOR_SIZE, entries.clone())?;
    disk.embed(part_start * SECTOR_SIZE, volume)?;
    disk.put(backup_table * SECTOR_SIZE, entries)?;
    disk.put(
        backup_header * SECTOR_SIZE,
        header(backup_header, 1, backup_table),
    )?;

    let mut regions = Vec::new();
    let mut add = |name: &str, first: u64, count: u64, free: bool| {
        if count > 0 {
            let n = regions.len();
            regions.push(Region {
                name: format!("{name} : {n})"),
                id: n as i32 - 1,
                first_sector: first,
                sector_count: count,
                free,
            });
        }
    };
    add("Protective Master Boot Record (MBR", 0, 1, false);
    add("GPT Header (Primary GPT Header", 1, 1, false);
    add(
        "GPT Partition Data (Primary GPT Table",
        2,
        GPT_TABLE_SECTORS,
        false,
    );
    add(
        " (Apple_Free",
        first_usable,
        part_start - first_usable,
        true,
    );
    add("disk image (Apple_HFS", part_start, part_sectors, false);
    add(
        "GPT Partition Data (Backup GPT Table",
        backup_table,
        GPT_TABLE_SECTORS,
        false,
    );
    add("GPT Header (Backup GPT Header", backup_header, 1, false);
    Ok(DiskLayout { disk, regions })
}

/// A partition found on a disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionInfo {
    /// Partition name (GPT/APM) or a description.
    pub name: String,
    /// Partition type (`Apple_HFS`, a GUID, or an MBR type byte).
    pub kind: String,
    /// Byte offset from the start of the disk.
    pub offset: u64,
    /// Size in bytes.
    pub size: u64,
    /// Whether this looks like an HFS+ partition.
    pub is_hfs: bool,
}

/// The partition map type found on a disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapKind {
    Gpt,
    Apm,
    Mbr,
    /// No partition map; the disk is a single volume.
    None,
}

/// Reads the partition map of a disk.
pub fn read_partitions<R: ReadAt>(dev: &mut R) -> Result<(MapKind, Vec<PartitionInfo>)> {
    let size = dev.size();
    if size >= 2 * SECTOR_SIZE {
        let hdr = dev.read_vec(SECTOR_SIZE, SECTOR_SIZE as usize)?;
        if &hdr[0..8] == b"EFI PART" {
            return read_gpt(dev, &hdr).map(|p| (MapKind::Gpt, p));
        }
        let b0 = dev.read_vec(0, SECTOR_SIZE as usize)?;
        if &b0[0..2] == b"ER"
            && let Ok(parts) = read_apm(dev)
            && !parts.is_empty()
        {
            return Ok((MapKind::Apm, parts));
        }
        if b0[510] == 0x55 && b0[511] == 0xAA && !looks_like_hfs(dev)? {
            let mut parts = Vec::new();
            for i in 0..4 {
                let e = &b0[446 + i * 16..462 + i * 16];
                let ty = e[4];
                let start = u64::from(u32::from_le_bytes(e[8..12].try_into().unwrap()));
                let count = u64::from(u32::from_le_bytes(e[12..16].try_into().unwrap()));
                if ty != 0 && count > 0 {
                    parts.push(PartitionInfo {
                        name: format!("MBR partition {}", i + 1),
                        kind: format!("{ty:#04x}"),
                        offset: start * SECTOR_SIZE,
                        size: count * SECTOR_SIZE,
                        is_hfs: ty == 0xAF,
                    });
                }
            }
            if !parts.is_empty() {
                return Ok((MapKind::Mbr, parts));
            }
        }
    }
    Ok((
        MapKind::None,
        vec![PartitionInfo {
            name: "whole disk".into(),
            kind: "Apple_HFS".into(),
            offset: 0,
            size,
            is_hfs: looks_like_hfs(dev)?,
        }],
    ))
}

fn looks_like_hfs<R: ReadAt>(dev: &mut R) -> Result<bool> {
    if dev.size() < 1026 {
        return Ok(false);
    }
    let sig = dev.read_vec(1024, 2)?;
    Ok(matches!(&sig[..], b"H+" | b"HX" | b"BD"))
}

fn read_gpt<R: ReadAt>(dev: &mut R, hdr: &[u8]) -> Result<Vec<PartitionInfo>> {
    let le64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let le32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let table = le64(hdr, 72);
    let count = le32(hdr, 80) as usize;
    let esize = le32(hdr, 84) as usize;
    if !(128..=4096).contains(&esize) || count > 4096 {
        return Err(corrupt("implausible GPT header"));
    }
    let data = dev.read_vec(table * SECTOR_SIZE, count * esize)?;
    let mut parts = Vec::new();
    for e in data.chunks_exact(esize) {
        let ty = Uuid::from_bytes_le(e[0..16].try_into().unwrap());
        if ty.is_nil() {
            continue;
        }
        let first = le64(e, 32);
        let last = le64(e, 40);
        let name: Vec<u16> = e[56..128]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        parts.push(PartitionInfo {
            name: String::from_utf16_lossy(&name),
            kind: if ty == APPLE_HFS_GUID {
                "Apple_HFS".into()
            } else {
                ty.to_string().to_uppercase()
            },
            offset: first * SECTOR_SIZE,
            size: (last + 1).saturating_sub(first) * SECTOR_SIZE,
            is_hfs: ty == APPLE_HFS_GUID,
        });
    }
    Ok(parts)
}

fn read_apm<R: ReadAt>(dev: &mut R) -> Result<Vec<PartitionInfo>> {
    let first = dev.read_vec(SECTOR_SIZE, SECTOR_SIZE as usize)?;
    if &first[0..2] != b"PM" {
        return Err(corrupt("no APM entries"));
    }
    let n = u32::from_be_bytes(first[4..8].try_into().unwrap()) as u64;
    let mut parts = Vec::new();
    for i in 1..=n.min(256) {
        let e = dev.read_vec(i * SECTOR_SIZE, SECTOR_SIZE as usize)?;
        if &e[0..2] != b"PM" {
            break;
        }
        let start = u64::from(u32::from_be_bytes(e[8..12].try_into().unwrap()));
        let count = u64::from(u32::from_be_bytes(e[12..16].try_into().unwrap()));
        let cstr = |b: &[u8]| {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        };
        let name = cstr(&e[16..48]);
        let kind = cstr(&e[48..80]);
        parts.push(PartitionInfo {
            is_hfs: kind == "Apple_HFS" || kind == "Apple_HFSX",
            name,
            kind,
            offset: start * SECTOR_SIZE,
            size: count * SECTOR_SIZE,
        });
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt_roundtrip() {
        let mut vol = VirtualDisk::new(1 << 20);
        vol.put(1024, b"H+".to_vec()).unwrap();
        let l = layout(PartitionScheme::Gpt, &vol, Uuid::nil(), Uuid::nil()).unwrap();
        let total: u64 = l.regions.iter().map(|r| r.sector_count).sum();
        assert_eq!(total * 512, l.disk.size());
        let mut r = l.disk.reader();
        let (kind, parts) = read_partitions(&mut r).unwrap();
        assert_eq!(kind, MapKind::Gpt);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].offset, 40 * 512);
        assert_eq!(parts[0].size, 1 << 20);
        assert!(parts[0].is_hfs);
        assert_eq!(parts[0].name, "disk image");
        assert_eq!(l.regions[4].name, "disk image (Apple_HFS : 4)");
        assert_eq!(l.regions[0].id, -1);

        let l = layout(PartitionScheme::None, &vol, Uuid::nil(), Uuid::nil()).unwrap();
        let mut r = l.disk.reader();
        let (kind, parts) = read_partitions(&mut r).unwrap();
        assert_eq!(kind, MapKind::None);
        assert!(parts[0].is_hfs);
    }
}
