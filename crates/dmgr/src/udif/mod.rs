//! The Universal Disk Image Format (UDIF) container used by `.dmg` files.
//!
//! A UDIF image is a *data fork* of (optionally compressed) chunks, an XML
//! property list whose `blkx` resources map sectors to chunks, and a 512-byte
//! `koly` trailer pointing at both.

pub(crate) mod compress;
pub mod format;
pub mod reader;
pub mod writer;

pub use format::{BlockTable, Checksum, Chunk, Format, Koly, SECTOR_SIZE, chunk};
pub use reader::{BlkxEntry, UdifImage, VerifyReport, is_udif};
pub use writer::{Region, WriteOptions, WriteSummary, write};
