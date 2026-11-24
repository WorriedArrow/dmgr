//! Positional reads over images, partitions and files.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::disk::{DiskReader, VirtualDisk};
use crate::error::{Result, corrupt};

/// Random-access reads from a fixed-size device.
pub trait ReadAt {
    /// Size of the device in bytes.
    fn size(&self) -> u64;

    /// Reads exactly `buf.len()` bytes at `offset`.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Reads `len` bytes at `offset` into a new vector.
    fn read_vec(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read_at(offset, &mut v)?;
        Ok(v)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &mut T {
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Box<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
}

/// [`ReadAt`] over a [`File`] (or any `Read + Seek`).
#[derive(Debug)]
pub struct SeekReader<R> {
    inner: R,
    size: u64,
}

impl<R: Read + Seek> SeekReader<R> {
    pub fn new(mut inner: R) -> Result<Self> {
        let size = inner.seek(SeekFrom::End(0))?;
        Ok(Self { inner, size })
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl SeekReader<File> {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        use crate::error::PathContext;
        let path = path.as_ref();
        Self::new(File::open(path).with_path(path)?)
    }
}

impl<R: Read + Seek> ReadAt for SeekReader<R> {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(corrupt("read beyond end of file"));
        }
        self.inner.seek(SeekFrom::Start(offset))?;
        self.inner.read_exact(buf)?;
        Ok(())
    }
}

impl ReadAt for DiskReader<'_> {
    fn size(&self) -> u64 {
        self.disk().size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        DiskReader::read_at(self, offset, buf)
    }
}

impl ReadAt for &[u8] {
    fn size(&self) -> u64 {
        self.len() as u64
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let start = usize::try_from(offset).map_err(|_| corrupt("offset overflow"))?;
        let src = self
            .get(start..start + buf.len())
            .ok_or_else(|| corrupt("read beyond end of buffer"))?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

/// A window `[offset, offset + len)` of another device (e.g. a partition).
#[derive(Debug)]
pub struct Slice<R> {
    inner: R,
    offset: u64,
    len: u64,
}

impl<R: ReadAt> Slice<R> {
    pub fn new(inner: R, offset: u64, len: u64) -> Result<Self> {
        if offset.checked_add(len).is_none_or(|e| e > inner.size()) {
            return Err(corrupt("partition extends beyond end of image"));
        }
        Ok(Self { inner, offset, len })
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: ReadAt> ReadAt for Slice<R> {
    fn size(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.len {
            return Err(corrupt("read beyond end of partition"));
        }
        self.inner.read_at(self.offset + offset, buf)
    }
}

/// Helper so a [`VirtualDisk`] can be read through the [`ReadAt`] trait.
pub fn disk_reader(disk: &VirtualDisk) -> DiskReader<'_> {
    disk.reader()
}
