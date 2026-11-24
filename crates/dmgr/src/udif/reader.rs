//! Reading UDIF images.

use plist::Value;

use super::compress::decompress;
use super::format::*;
use crate::error::{Error, Result, corrupt};
use crate::io::ReadAt;

/// A `blkx` resource: a named block table.
#[derive(Clone, Debug)]
pub struct BlkxEntry {
    pub name: String,
    pub id: String,
    pub attributes: String,
    pub table: BlockTable,
}

#[derive(Clone, Copy, Debug)]
struct FlatChunk {
    start: u64,
    sectors: u64,
    kind: u32,
    offset: u64,
    length: u64,
}

/// Results of [`UdifImage::verify`].
#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    /// `(blkx name, stored CRC, computed CRC)` for every table with a CRC-32.
    pub tables: Vec<(String, u32, u32)>,
    /// Stored and computed master checksum, when present.
    pub master: Option<(u32, u32)>,
    /// Stored and computed data fork checksum, when present.
    pub data_fork: Option<(u32, u32)>,
}

impl VerifyReport {
    /// Whether every checksum matched.
    pub fn ok(&self) -> bool {
        self.tables.iter().all(|(_, a, b)| a == b)
            && self.master.is_none_or(|(a, b)| a == b)
            && self.data_fork.is_none_or(|(a, b)| a == b)
    }
}

/// An opened UDIF image, readable as a flat disk through [`ReadAt`].
#[derive(Debug)]
pub struct UdifImage<R> {
    src: R,
    koly: Koly,
    blkx: Vec<BlkxEntry>,
    plist: Value,
    chunks: Vec<FlatChunk>,
    size: u64,
    cache: Option<(usize, Vec<u8>)>,
}

/// Whether `src` ends with a UDIF `koly` trailer.
pub fn is_udif<R: ReadAt>(src: &mut R) -> Result<bool> {
    let size = src.size();
    if size < Koly::SIZE as u64 {
        return Ok(false);
    }
    let mut magic = [0u8; 4];
    src.read_at(size - Koly::SIZE as u64, &mut magic)?;
    Ok(&magic == Koly::MAGIC)
}

impl<R: ReadAt> UdifImage<R> {
    /// Parses the trailer and block tables of a UDIF image.
    pub fn open(mut src: R) -> Result<Self> {
        let size = src.size();
        if size < Koly::SIZE as u64 {
            return Err(corrupt("file too small to be a disk image"));
        }
        let koly = Koly::decode(&src.read_vec(size - Koly::SIZE as u64, Koly::SIZE)?)?;
        if koly.segment_count > 1 {
            return Err(Error::Unsupported("segmented disk images".into()));
        }
        if koly.xml_length == 0 {
            return Err(Error::Unsupported(
                "images without an XML property list (resource-fork only)".into(),
            ));
        }
        if koly.xml_offset + koly.xml_length > size {
            return Err(corrupt("property list extends beyond end of file"));
        }
        let xml = src.read_vec(koly.xml_offset, koly.xml_length as usize)?;
        let plist = Value::from_reader_xml(&xml[..])?;
        let blkx_arr = plist
            .as_dictionary()
            .and_then(|d| d.get("resource-fork"))
            .and_then(Value::as_dictionary)
            .and_then(|d| d.get("blkx"))
            .and_then(Value::as_array)
            .ok_or_else(|| corrupt("property list has no blkx resources"))?;
        let mut blkx = Vec::with_capacity(blkx_arr.len());
        for v in blkx_arr {
            let d = v
                .as_dictionary()
                .ok_or_else(|| corrupt("blkx entry is not a dictionary"))?;
            let s = |k: &str| d.get(k).and_then(Value::as_string).unwrap_or("").to_owned();
            let data = d
                .get("Data")
                .and_then(Value::as_data)
                .ok_or_else(|| corrupt("blkx entry without Data"))?;
            blkx.push(BlkxEntry {
                name: s("Name"),
                id: s("ID"),
                attributes: s("Attributes"),
                table: BlockTable::decode(data)?,
            });
        }

        let base = koly.data_fork_offset;
        let mut chunks = Vec::new();
        for b in &blkx {
            for c in &b.table.chunks {
                if matches!(c.kind, chunk::COMMENT | chunk::TERMINATOR) || c.sector_count == 0 {
                    continue;
                }
                chunks.push(FlatChunk {
                    start: b.table.first_sector + c.sector,
                    sectors: c.sector_count,
                    kind: c.kind,
                    offset: base + b.table.data_offset + c.offset,
                    length: c.length,
                });
            }
        }
        chunks.sort_by_key(|c| c.start);
        let sectors = chunks
            .iter()
            .map(|c| c.start + c.sectors)
            .max()
            .unwrap_or(0)
            .max(koly.sector_count);
        Ok(Self {
            src,
            koly,
            blkx,
            plist,
            chunks,
            size: sectors * SECTOR_SIZE,
            cache: None,
        })
    }

    pub fn koly(&self) -> &Koly {
        &self.koly
    }

    pub fn blkx(&self) -> &[BlkxEntry] {
        &self.blkx
    }

    /// The full property list.
    pub fn plist(&self) -> &Value {
        &self.plist
    }

    /// The image format inferred from the chunk types present.
    pub fn format_guess(&self) -> &'static str {
        let has = |k: u32| self.chunks.iter().any(|c| c.kind == k);
        if has(chunk::LZMA) {
            "ULMO"
        } else if has(chunk::LZFSE) {
            "ULFO"
        } else if has(chunk::BZIP2) {
            "UDBZ"
        } else if has(chunk::ZLIB) {
            "UDZO"
        } else if has(chunk::ADC) {
            "UDCO"
        } else if self.chunks.iter().all(|c| c.kind == chunk::RAW) {
            "UDRW"
        } else {
            "UDRO"
        }
    }

    fn load_chunk(&mut self, idx: usize) -> Result<&[u8]> {
        if self.cache.as_ref().is_none_or(|(i, _)| *i != idx) {
            let c = self.chunks[idx];
            let out_len =
                usize::try_from(c.sectors * SECTOR_SIZE).map_err(|_| corrupt("chunk too large"))?;
            let data = if matches!(c.kind, chunk::ZERO_FILL | chunk::IGNORE) {
                vec![0u8; out_len]
            } else {
                let len = usize::try_from(c.length).map_err(|_| corrupt("chunk too large"))?;
                let raw = self.src.read_vec(c.offset, len)?;
                decompress(c.kind, &raw, out_len)?
            };
            self.cache = Some((idx, data));
        }
        Ok(&self.cache.as_ref().expect("cached").1)
    }

    /// Recomputes every checksum stored in the image.
    pub fn verify(&mut self) -> Result<VerifyReport> {
        let mut report = VerifyReport::default();
        let mut master = crc32fast::Hasher::new();
        let blkx = self.blkx.clone();
        let base = self.koly.data_fork_offset;
        for b in &blkx {
            let mut h = crc32fast::Hasher::new();
            let mut covered = 0u64;
            for c in &b.table.chunks {
                if matches!(c.kind, chunk::COMMENT | chunk::TERMINATOR) {
                    continue;
                }
                if c.sector > covered {
                    // gaps read as zeros
                    let zeros = vec![0u8; ((c.sector - covered) * SECTOR_SIZE) as usize];
                    h.update(&zeros);
                }
                let out_len = (c.sector_count * SECTOR_SIZE) as usize;
                let data = if matches!(c.kind, chunk::ZERO_FILL | chunk::IGNORE) {
                    vec![0u8; out_len]
                } else {
                    let raw = self
                        .src
                        .read_vec(base + b.table.data_offset + c.offset, c.length as usize)?;
                    decompress(c.kind, &raw, out_len)?
                };
                h.update(&data);
                covered = c.sector + c.sector_count;
            }
            let computed = h.finalize();
            if let Some(stored) = b.table.checksum.crc32_value() {
                report.tables.push((b.name.clone(), stored, computed));
                master.update(&stored.to_be_bytes());
            }
        }
        if let Some(stored) = self.koly.master_checksum.crc32_value() {
            report.master = Some((stored, master.finalize()));
        }
        if let Some(stored) = self.koly.data_fork_checksum.crc32_value() {
            let mut h = crc32fast::Hasher::new();
            let mut off = 0u64;
            let mut buf = vec![0u8; 1 << 20];
            while off < self.koly.data_fork_length {
                let n = ((self.koly.data_fork_length - off) as usize).min(buf.len());
                self.src.read_at(base + off, &mut buf[..n])?;
                h.update(&buf[..n]);
                off += n as u64;
            }
            report.data_fork = Some((stored, h.finalize()));
        }
        Ok(report)
    }

    pub fn into_inner(self) -> R {
        self.src
    }
}

impl<R: ReadAt> ReadAt for UdifImage<R> {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(corrupt("read beyond end of image"));
        }
        let mut pos = offset;
        let end = offset + buf.len() as u64;
        while pos < end {
            let sector = pos / SECTOR_SIZE;
            // Last chunk starting at or before `sector`.
            let idx = self.chunks.partition_point(|c| c.start <= sector);
            let hit = idx
                .checked_sub(1)
                .filter(|&i| sector < self.chunks[i].start + self.chunks[i].sectors);
            let dst_off = (pos - offset) as usize;
            match hit {
                Some(i) => {
                    let c = self.chunks[i];
                    let chunk_start = c.start * SECTOR_SIZE;
                    let chunk_end = chunk_start + c.sectors * SECTOR_SIZE;
                    let n = (end.min(chunk_end) - pos) as usize;
                    let within = (pos - chunk_start) as usize;
                    let data = self.load_chunk(i)?;
                    buf[dst_off..dst_off + n].copy_from_slice(&data[within..within + n]);
                    pos += n as u64;
                }
                None => {
                    // Not covered by any chunk: zeros up to the next chunk.
                    let next = self
                        .chunks
                        .get(idx)
                        .map_or(end, |c| (c.start * SECTOR_SIZE).min(end));
                    let next = next.max(pos + 1).min(end);
                    buf[dst_off..dst_off + (next - pos) as usize].fill(0);
                    pos = next;
                }
            }
        }
        Ok(())
    }
}
