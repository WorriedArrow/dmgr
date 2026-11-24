//! UDIF on-disk structures: the `koly` trailer and `mish` block tables.

use std::fmt;
use std::str::FromStr;

use crate::bytes::{PutBe, Reader};
use crate::error::{Error, Result, corrupt};

/// Size of a UDIF sector.
pub const SECTOR_SIZE: u64 = 512;

/// Block chunk types used in `mish` tables.
pub mod chunk {
    pub const ZERO_FILL: u32 = 0x0000_0000;
    pub const RAW: u32 = 0x0000_0001;
    pub const IGNORE: u32 = 0x0000_0002;
    pub const ADC: u32 = 0x8000_0004;
    pub const ZLIB: u32 = 0x8000_0005;
    pub const BZIP2: u32 = 0x8000_0006;
    pub const LZFSE: u32 = 0x8000_0007;
    pub const LZMA: u32 = 0x8000_0008;
    pub const COMMENT: u32 = 0x7FFF_FFFE;
    pub const TERMINATOR: u32 = 0xFFFF_FFFF;

    /// A short human-readable name for a chunk type.
    pub fn name(kind: u32) -> &'static str {
        match kind {
            ZERO_FILL => "zero",
            RAW => "raw",
            IGNORE => "ignore",
            ADC => "adc",
            ZLIB => "zlib",
            BZIP2 => "bzip2",
            LZFSE => "lzfse",
            LZMA => "lzma",
            COMMENT => "comment",
            TERMINATOR => "end",
            _ => "unknown",
        }
    }
}

/// Checksum type for CRC-32 (`kUDIFChecksumTypeCRC32`).
pub const CHECKSUM_CRC32: u32 = 2;

/// Disk image formats `dmgr` can produce (names follow `hdiutil`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "UPPERCASE"))]
#[non_exhaustive]
pub enum Format {
    /// UDIF read/write image (uncompressed, every sector stored).
    Udrw,
    /// UDIF read-only image (uncompressed, zero runs elided).
    Udro,
    /// UDIF zlib-compressed image. Readable by every macOS version.
    #[default]
    Udzo,
    /// UDIF bzip2-compressed image (macOS 10.4+, deprecated by Apple).
    Udbz,
    /// UDIF LZFSE-compressed image (macOS 10.11+).
    Ulfo,
    /// UDIF LZMA-compressed image (macOS 10.15+).
    Ulmo,
}

impl Format {
    /// All formats, in rough order of popularity.
    pub const ALL: [Format; 6] = [
        Format::Udzo,
        Format::Ulfo,
        Format::Ulmo,
        Format::Udbz,
        Format::Udro,
        Format::Udrw,
    ];

    /// The `hdiutil` name of the format.
    pub fn name(self) -> &'static str {
        match self {
            Self::Udrw => "UDRW",
            Self::Udro => "UDRO",
            Self::Udzo => "UDZO",
            Self::Udbz => "UDBZ",
            Self::Ulfo => "ULFO",
            Self::Ulmo => "ULMO",
        }
    }

    /// The chunk type for compressed chunks, if the format compresses.
    pub fn chunk_type(self) -> Option<u32> {
        match self {
            Self::Udrw | Self::Udro => None,
            Self::Udzo => Some(chunk::ZLIB),
            Self::Udbz => Some(chunk::BZIP2),
            Self::Ulfo => Some(chunk::LZFSE),
            Self::Ulmo => Some(chunk::LZMA),
        }
    }

    /// Whether this build of `dmgr` can write the format.
    pub fn is_supported(self) -> bool {
        #[allow(clippy::match_like_matches_macro)]
        match self {
            Self::Udbz => cfg!(feature = "bzip2"),
            Self::Ulfo => cfg!(feature = "lzfse"),
            Self::Ulmo => cfg!(feature = "lzma"),
            _ => true,
        }
    }

    /// Default compression level for the format.
    pub fn default_level(self) -> u32 {
        match self {
            Self::Udzo => 9,
            Self::Udbz => 9,
            Self::Ulmo => 6,
            _ => 0,
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Format {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "udrw" | "rw" | "readwrite" => Self::Udrw,
            "udro" | "ro" | "readonly" | "raw" | "none" => Self::Udro,
            "udzo" | "zlib" | "deflate" => Self::Udzo,
            "udbz" | "bzip2" | "bz2" => Self::Udbz,
            "ulfo" | "lzfse" => Self::Ulfo,
            "ulmo" | "lzma" | "xz" => Self::Ulmo,
            _ => {
                return Err(Error::InvalidInput(format!(
                    "unknown image format {s:?} (expected UDZO, ULFO, ULMO, UDBZ, UDRO or UDRW)"
                )));
            }
        })
    }
}

/// A UDIF checksum (`UDIFChecksum`).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Checksum {
    pub kind: u32,
    pub bits: u32,
    pub data: [u32; 32],
}

impl fmt::Debug for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Checksum(type {}, {} bits, {:#010x})",
            self.kind, self.bits, self.data[0]
        )
    }
}

impl Checksum {
    pub fn crc32(value: u32) -> Self {
        let mut data = [0; 32];
        data[0] = value;
        Self {
            kind: CHECKSUM_CRC32,
            bits: 32,
            data,
        }
    }

    pub fn crc32_value(&self) -> Option<u32> {
        (self.kind == CHECKSUM_CRC32).then_some(self.data[0])
    }

    fn encode(&self, o: &mut Vec<u8>) {
        o.put_u32(self.kind);
        o.put_u32(self.bits);
        for v in self.data {
            o.put_u32(v);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut c = Self {
            kind: r.u32()?,
            bits: r.u32()?,
            data: [0; 32],
        };
        for v in &mut c.data {
            *v = r.u32()?;
        }
        Ok(c)
    }
}

/// The 512-byte `koly` trailer at the end of every UDIF image.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Koly {
    pub version: u32,
    pub header_size: u32,
    pub flags: u32,
    pub running_data_fork_offset: u64,
    pub data_fork_offset: u64,
    pub data_fork_length: u64,
    pub rsrc_fork_offset: u64,
    pub rsrc_fork_length: u64,
    pub segment_number: u32,
    pub segment_count: u32,
    pub segment_id: [u8; 16],
    pub data_fork_checksum: Checksum,
    pub xml_offset: u64,
    pub xml_length: u64,
    pub master_checksum: Checksum,
    pub image_variant: u32,
    pub sector_count: u64,
}

impl Koly {
    pub const SIZE: usize = 512;
    pub const MAGIC: &'static [u8; 4] = b"koly";

    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(Self::SIZE);
        o.extend_from_slice(Self::MAGIC);
        o.put_u32(self.version);
        o.put_u32(self.header_size);
        o.put_u32(self.flags);
        o.put_u64(self.running_data_fork_offset);
        o.put_u64(self.data_fork_offset);
        o.put_u64(self.data_fork_length);
        o.put_u64(self.rsrc_fork_offset);
        o.put_u64(self.rsrc_fork_length);
        o.put_u32(self.segment_number);
        o.put_u32(self.segment_count);
        o.extend_from_slice(&self.segment_id);
        self.data_fork_checksum.encode(&mut o);
        o.put_u64(self.xml_offset);
        o.put_u64(self.xml_length);
        o.put_zeros(120);
        self.master_checksum.encode(&mut o);
        o.put_u32(self.image_variant);
        o.put_u64(self.sector_count);
        o.put_zeros(12);
        debug_assert_eq!(o.len(), Self::SIZE);
        o
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        if r.bytes(4)? != Self::MAGIC {
            return Err(corrupt("missing koly trailer"));
        }
        let mut k = Self {
            version: r.u32()?,
            header_size: r.u32()?,
            flags: r.u32()?,
            running_data_fork_offset: r.u64()?,
            data_fork_offset: r.u64()?,
            data_fork_length: r.u64()?,
            rsrc_fork_offset: r.u64()?,
            rsrc_fork_length: r.u64()?,
            segment_number: r.u32()?,
            segment_count: r.u32()?,
            segment_id: r.array()?,
            ..Default::default()
        };
        k.data_fork_checksum = Checksum::decode(&mut r)?;
        k.xml_offset = r.u64()?;
        k.xml_length = r.u64()?;
        r.skip(120)?;
        k.master_checksum = Checksum::decode(&mut r)?;
        k.image_variant = r.u32()?;
        k.sector_count = r.u64()?;
        Ok(k)
    }
}

/// One entry of a block table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Chunk {
    pub kind: u32,
    pub comment: u32,
    /// First sector, relative to the table's first sector.
    pub sector: u64,
    pub sector_count: u64,
    /// Offset of the stored bytes, relative to the data fork.
    pub offset: u64,
    pub length: u64,
}

/// A `mish` block table describing one partition's sectors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockTable {
    pub version: u32,
    pub first_sector: u64,
    pub sector_count: u64,
    pub data_offset: u64,
    pub buffers_needed: u32,
    pub block_descriptors: u32,
    pub checksum: Checksum,
    pub chunks: Vec<Chunk>,
}

impl BlockTable {
    pub const MAGIC: &'static [u8; 4] = b"mish";

    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(204 + 40 * self.chunks.len());
        o.extend_from_slice(Self::MAGIC);
        o.put_u32(self.version);
        o.put_u64(self.first_sector);
        o.put_u64(self.sector_count);
        o.put_u64(self.data_offset);
        o.put_u32(self.buffers_needed);
        o.put_u32(self.block_descriptors);
        o.put_zeros(24);
        self.checksum.encode(&mut o);
        o.put_u32(self.chunks.len() as u32);
        for c in &self.chunks {
            o.put_u32(c.kind);
            o.put_u32(c.comment);
            o.put_u64(c.sector);
            o.put_u64(c.sector_count);
            o.put_u64(c.offset);
            o.put_u64(c.length);
        }
        o
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        if r.bytes(4)? != Self::MAGIC {
            return Err(corrupt("block table missing 'mish' signature"));
        }
        let mut t = Self {
            version: r.u32()?,
            first_sector: r.u64()?,
            sector_count: r.u64()?,
            data_offset: r.u64()?,
            buffers_needed: r.u32()?,
            block_descriptors: r.u32()?,
            ..Default::default()
        };
        r.skip(24)?;
        t.checksum = Checksum::decode(&mut r)?;
        let n = r.u32()? as usize;
        t.chunks.reserve(n.min(1 << 20));
        for _ in 0..n {
            t.chunks.push(Chunk {
                kind: r.u32()?,
                comment: r.u32()?,
                sector: r.u64()?,
                sector_count: r.u64()?,
                offset: r.u64()?,
                length: r.u64()?,
            });
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn koly_roundtrip() {
        let k = Koly {
            version: 4,
            header_size: 512,
            flags: 1,
            data_fork_length: 1234,
            segment_number: 1,
            segment_count: 1,
            segment_id: [7; 16],
            data_fork_checksum: Checksum::crc32(0xDEAD_BEEF),
            xml_offset: 1234,
            xml_length: 99,
            master_checksum: Checksum::crc32(0x1234_5678),
            image_variant: 1,
            sector_count: 2048,
            ..Default::default()
        };
        let b = k.encode();
        assert_eq!(b.len(), 512);
        assert_eq!(Koly::decode(&b).unwrap(), k);
    }

    #[test]
    fn mish_roundtrip() {
        let t = BlockTable {
            version: 1,
            first_sector: 40,
            sector_count: 100,
            buffers_needed: 0x808,
            checksum: Checksum::crc32(5),
            chunks: vec![Chunk {
                kind: chunk::ZLIB,
                sector_count: 100,
                length: 7,
                ..Default::default()
            }],
            ..Default::default()
        };
        let b = t.encode();
        assert_eq!(b.len(), 204 + 40);
        assert_eq!(BlockTable::decode(&b).unwrap(), t);
    }

    #[test]
    fn format_names() {
        for f in Format::ALL {
            assert_eq!(f.name().parse::<Format>().unwrap(), f);
        }
        assert_eq!("lzfse".parse::<Format>().unwrap(), Format::Ulfo);
        assert!("bogus".parse::<Format>().is_err());
    }
}
