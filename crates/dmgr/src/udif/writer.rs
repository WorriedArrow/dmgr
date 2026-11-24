//! Streaming UDIF writer.

use std::io::Write;

use plist::{Dictionary, Value};

use super::compress::compress;
use super::format::*;
use crate::disk::VirtualDisk;
use crate::error::{Result, invalid};

/// A region of the disk described by its own block table (`blkx` entry),
/// normally one per partition-map element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// Display name, e.g. `disk image (Apple_HFS : 4)`.
    pub name: String,
    /// The `ID` attribute of the `blkx` resource.
    pub id: i32,
    pub first_sector: u64,
    pub sector_count: u64,
    /// Unallocated space; stored as "ignore" chunks.
    pub free: bool,
}

/// Options for [`write()`].
#[derive(Clone, Debug)]
pub struct WriteOptions {
    pub format: Format,
    /// Compression level; the format default when `None`.
    pub level: Option<u32>,
    /// Sectors per chunk (default 2048 = 1 MiB).
    pub chunk_sectors: u32,
    /// Segment ID stored in the trailer.
    pub segment_id: [u8; 16],
}

impl WriteOptions {
    pub fn new(format: Format) -> Self {
        Self {
            format,
            level: None,
            chunk_sectors: 2048,
            segment_id: *uuid::Uuid::new_v4().as_bytes(),
        }
    }
}

/// Information about a written image.
#[derive(Clone, Debug)]
pub struct WriteSummary {
    /// The UDIF trailer (`None` for UDRW, which is a raw image).
    pub koly: Option<Koly>,
    pub tables: Vec<(Region, BlockTable)>,
    /// Size of the uncompressed disk in bytes.
    pub disk_size: u64,
    /// Total bytes written, including the property list and trailer.
    pub image_size: u64,
}

/// Progress callback: `(bytes of the virtual disk processed, total bytes)`.
pub type Progress<'a> = &'a (dyn Fn(u64, u64) + Sync);

const BATCH: usize = 32;

struct Pending {
    sector: u64,
    sectors: u64,
    data: Option<Vec<u8>>,
}

fn update_crc_zeros(h: &mut crc32fast::Hasher, mut n: u64) {
    static ZEROS: [u8; 65536] = [0; 65536];
    while n > 0 {
        let k = n.min(ZEROS.len() as u64) as usize;
        h.update(&ZEROS[..k]);
        n -= k as u64;
    }
}

/// Writes `disk` as a UDIF image to `out`, one block table per region.
///
/// Regions must cover the disk contiguously in order. [`Format::Udrw`]
/// images are written as plain raw disk images, which is what `hdiutil`
/// produces for that format.
pub fn write<W: Write>(
    disk: &VirtualDisk,
    regions: &[Region],
    opts: &WriteOptions,
    mut out: W,
    progress: Option<Progress<'_>>,
) -> Result<WriteSummary> {
    if !opts.format.is_supported() {
        return Err(crate::Error::Unsupported(format!(
            "{} support was not compiled into this build of dmgr",
            opts.format
        )));
    }
    if opts.chunk_sectors == 0 || opts.chunk_sectors > 1 << 16 {
        return Err(invalid("chunk size must be between 1 and 65536 sectors"));
    }
    if !disk.size().is_multiple_of(SECTOR_SIZE) {
        return Err(invalid("disk size is not a multiple of 512"));
    }
    let total_sectors = disk.size() / SECTOR_SIZE;
    let mut expect = 0u64;
    for r in regions {
        if r.first_sector != expect {
            return Err(invalid("regions must be contiguous"));
        }
        expect += r.sector_count;
    }
    if expect != total_sectors {
        return Err(invalid("regions do not cover the whole disk"));
    }

    if opts.format == Format::Udrw {
        return write_raw(disk, out, progress);
    }
    let level = opts.level.unwrap_or(opts.format.default_level());
    let mut reader = disk.reader();
    let mut data_crc = crc32fast::Hasher::new();
    let mut data_off = 0u64;
    let mut tables = Vec::with_capacity(regions.len());
    let chunk_bytes = u64::from(opts.chunk_sectors) * SECTOR_SIZE;

    for (index, region) in regions.iter().enumerate() {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut crc = crc32fast::Hasher::new();
        let mut sector = 0u64;
        while sector < region.sector_count {
            // Gather a batch of chunks.
            let mut batch: Vec<Pending> = Vec::with_capacity(BATCH);
            while batch.len() < BATCH && sector < region.sector_count {
                let n = (region.sector_count - sector).min(u64::from(opts.chunk_sectors));
                let off = (region.first_sector + sector) * SECTOR_SIZE;
                let len = n * SECTOR_SIZE;
                let data = if region.free || disk.is_sparse(off, len) {
                    None
                } else {
                    let mut buf = vec![0u8; len as usize];
                    reader.read_at(off, &mut buf)?;
                    if buf.iter().all(|&b| b == 0) {
                        None
                    } else {
                        Some(buf)
                    }
                };
                batch.push(Pending {
                    sector,
                    sectors: n,
                    data,
                });
                sector += n;
            }

            let compressed = compress_batch(&batch, opts.format, level)?;

            for (p, c) in batch.iter().zip(compressed) {
                match (&p.data, c) {
                    (None, _) => {
                        // hdiutil leaves "ignore" chunks out of the checksum.
                        if !region.free {
                            update_crc_zeros(&mut crc, p.sectors * SECTOR_SIZE);
                        }
                        let kind = if region.free {
                            chunk::IGNORE
                        } else {
                            chunk::ZERO_FILL
                        };
                        match chunks.last_mut() {
                            Some(last)
                                if last.kind == kind
                                    && last.sector + last.sector_count == p.sector =>
                            {
                                last.sector_count += p.sectors;
                            }
                            _ => chunks.push(Chunk {
                                kind,
                                comment: 0,
                                sector: p.sector,
                                sector_count: p.sectors,
                                offset: data_off,
                                length: 0,
                            }),
                        }
                    }
                    (Some(raw), c) => {
                        crc.update(raw);
                        let (kind, bytes) = match (opts.format.chunk_type(), c) {
                            (Some(kind), Some(c)) if c.len() < raw.len() => (kind, c),
                            _ => (chunk::RAW, raw.clone()),
                        };
                        out.write_all(&bytes)?;
                        data_crc.update(&bytes);
                        chunks.push(Chunk {
                            kind,
                            comment: 0,
                            sector: p.sector,
                            sector_count: p.sectors,
                            offset: data_off,
                            length: bytes.len() as u64,
                        });
                        data_off += bytes.len() as u64;
                    }
                }
            }
            if let Some(cb) = progress {
                cb((region.first_sector + sector) * SECTOR_SIZE, disk.size());
            }
        }
        chunks.push(Chunk {
            kind: chunk::TERMINATOR,
            comment: 0,
            sector: region.sector_count,
            sector_count: 0,
            offset: data_off,
            length: 0,
        });
        let table = BlockTable {
            version: 1,
            first_sector: region.first_sector,
            sector_count: region.sector_count,
            data_offset: 0,
            buffers_needed: buffers_needed(opts.format, opts.chunk_sectors),
            block_descriptors: index as u32,
            checksum: Checksum::crc32(crc.finalize()),
            chunks,
        };
        tables.push((region.clone(), table));
    }
    let _ = chunk_bytes;

    // Property list with the blkx resources.
    let blkx: Vec<Value> = tables
        .iter()
        .map(|(r, t)| {
            let mut d = Dictionary::new();
            d.insert("Attributes".into(), Value::String("0x0050".into()));
            d.insert("CFName".into(), Value::String(r.name.clone()));
            d.insert("Data".into(), Value::Data(t.encode()));
            d.insert("ID".into(), Value::String(r.id.to_string()));
            d.insert("Name".into(), Value::String(r.name.clone()));
            Value::Dictionary(d)
        })
        .collect();
    let mut rsrc = Dictionary::new();
    rsrc.insert("blkx".into(), Value::Array(blkx));
    let mut root = Dictionary::new();
    root.insert("resource-fork".into(), Value::Dictionary(rsrc));
    let mut xml = Vec::new();
    Value::Dictionary(root).to_writer_xml(&mut xml)?;
    xml.push(b'\n');
    out.write_all(&xml)?;

    let mut master = crc32fast::Hasher::new();
    for (_, t) in &tables {
        master.update(&t.checksum.data[0].to_be_bytes());
    }
    let koly = Koly {
        version: 4,
        header_size: Koly::SIZE as u32,
        flags: 1,
        running_data_fork_offset: 0,
        data_fork_offset: 0,
        data_fork_length: data_off,
        rsrc_fork_offset: 0,
        rsrc_fork_length: 0,
        segment_number: 1,
        segment_count: 1,
        segment_id: opts.segment_id,
        data_fork_checksum: Checksum::crc32(data_crc.finalize()),
        xml_offset: data_off,
        xml_length: xml.len() as u64,
        master_checksum: Checksum::crc32(master.finalize()),
        image_variant: 1,
        sector_count: total_sectors,
    };
    out.write_all(&koly.encode())?;
    out.flush()?;
    Ok(WriteSummary {
        image_size: data_off + xml.len() as u64 + Koly::SIZE as u64,
        koly: Some(koly),
        tables,
        disk_size: disk.size(),
    })
}

/// Decompression buffer size (in sectors) that `hdiutil` records for each
/// codec. Intel builds of DiskImages size their buffers from it.
fn buffers_needed(format: Format, chunk_sectors: u32) -> u32 {
    match format {
        Format::Udbz => chunk_sectors * 2 + 11,
        Format::Ulfo => chunk_sectors * 2 + 21,
        Format::Ulmo => chunk_sectors * 2 + 1,
        _ => chunk_sectors + 8,
    }
}

/// Writes the disk verbatim (UDRW / raw image).
fn write_raw<W: Write>(
    disk: &VirtualDisk,
    mut out: W,
    progress: Option<Progress<'_>>,
) -> Result<WriteSummary> {
    let mut reader = disk.reader();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < disk.size() {
        let n = ((disk.size() - off) as usize).min(buf.len());
        reader.read_at(off, &mut buf[..n])?;
        out.write_all(&buf[..n])?;
        off += n as u64;
        if let Some(cb) = progress {
            cb(off, disk.size());
        }
    }
    out.flush()?;
    Ok(WriteSummary {
        koly: None,
        tables: Vec::new(),
        disk_size: disk.size(),
        image_size: disk.size(),
    })
}

fn compress_batch(batch: &[Pending], format: Format, level: u32) -> Result<Vec<Option<Vec<u8>>>> {
    if format.chunk_type().is_none() {
        return Ok(batch.iter().map(|_| None).collect());
    }
    let work = |p: &Pending| -> Result<Option<Vec<u8>>> {
        p.data
            .as_deref()
            .map(|d| compress(format, level, d))
            .transpose()
    };
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        batch.par_iter().map(work).collect()
    }
    #[cfg(not(feature = "parallel"))]
    {
        batch.iter().map(work).collect()
    }
}
