//! A lazily materialized, sparse disk image.
//!
//! Building a volume produces a [`VirtualDisk`]: a list of non-overlapping
//! extents backed either by in-memory buffers (file system metadata) or by
//! ranges of source files (file contents). Everything else reads as zeros.
//! This lets multi-gigabyte images be produced without holding them in memory.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::{Error, PathContext, Result, invalid};
use crate::io::ReadAt;

/// A shareable device used as a [`Source`].
pub type SharedDevice = Arc<Mutex<dyn ReadAt + Send>>;

/// Where the bytes of an extent come from.
#[derive(Clone)]
pub enum Source {
    /// In-memory bytes.
    Bytes(Arc<[u8]>),
    /// `len` bytes of a file starting at `offset`.
    File {
        path: Arc<Path>,
        offset: u64,
        len: u64,
    },
    /// `len` bytes of another device starting at `offset`.
    Device {
        dev: SharedDevice,
        offset: u64,
        len: u64,
    },
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Self::File { path, offset, len } => {
                write!(f, "File({}, {offset}+{len})", path.display())
            }
            Self::Device { offset, len, .. } => write!(f, "Device({offset}+{len})"),
        }
    }
}

impl Source {
    pub fn len(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::File { len, .. } | Self::Device { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn file(path: impl Into<PathBuf>, len: u64) -> Self {
        Self::File {
            path: Arc::from(path.into().into_boxed_path()),
            offset: 0,
            len,
        }
    }
}

impl From<Vec<u8>> for Source {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v.into())
    }
}

/// A sparse virtual disk built from [`Source`] extents.
#[derive(Clone, Debug, Default)]
pub struct VirtualDisk {
    size: u64,
    extents: Vec<(u64, Source)>,
}

impl VirtualDisk {
    /// Creates an all-zero disk of `size` bytes.
    pub fn new(size: u64) -> Self {
        Self {
            size,
            extents: Vec::new(),
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Places `src` at `offset`. Extents must not overlap.
    pub fn put(&mut self, offset: u64, src: impl Into<Source>) -> Result<()> {
        let src = src.into();
        if src.is_empty() {
            return Ok(());
        }
        let end = offset + src.len();
        if end > self.size {
            return Err(invalid(format!(
                "extent {offset}..{end} beyond disk size {}",
                self.size
            )));
        }
        let idx = self.extents.partition_point(|(o, _)| *o < offset);
        if let Some((prev_off, prev)) = idx.checked_sub(1).map(|i| &self.extents[i])
            && prev_off + prev.len() > offset
        {
            return Err(invalid("overlapping disk extents"));
        }
        if let Some((next_off, _)) = self.extents.get(idx)
            && end > *next_off
        {
            return Err(invalid("overlapping disk extents"));
        }
        self.extents.insert(idx, (offset, src));
        Ok(())
    }

    /// Copies all extents of `other` into this disk at `offset`.
    pub fn embed(&mut self, offset: u64, other: &VirtualDisk) -> Result<()> {
        if offset + other.size > self.size {
            return Err(invalid("embedded disk does not fit"));
        }
        for (o, src) in &other.extents {
            self.put(offset + o, src.clone())?;
        }
        Ok(())
    }

    /// Whether `off..off+len` contains no data extents (reads as zeros).
    pub fn is_sparse(&self, off: u64, len: u64) -> bool {
        let idx = self.extents.partition_point(|(o, s)| o + s.len() <= off);
        match self.extents.get(idx) {
            Some((o, _)) => *o >= off + len,
            None => true,
        }
    }

    /// Opens a reader over this disk.
    pub fn reader(&self) -> DiskReader<'_> {
        DiskReader {
            disk: self,
            open: None,
        }
    }
}

/// Sequential/random reader over a [`VirtualDisk`]; caches one open file.
#[derive(Debug)]
pub struct DiskReader<'a> {
    disk: &'a VirtualDisk,
    open: Option<(Arc<Path>, File)>,
}

impl DiskReader<'_> {
    /// The disk being read.
    pub fn disk(&self) -> &VirtualDisk {
        self.disk
    }

    /// Fills `buf` with the disk contents at `off`.
    pub fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        let end = off + buf.len() as u64;
        if end > self.disk.size {
            return Err(invalid("read beyond end of virtual disk"));
        }
        buf.fill(0);
        let extents = &self.disk.extents;
        let mut idx = extents.partition_point(|(o, s)| o + s.len() <= off);
        while let Some((eo, src)) = extents.get(idx) {
            if *eo >= end {
                break;
            }
            let s = off.max(*eo);
            let e = end.min(eo + src.len());
            let dst = &mut buf[(s - off) as usize..(e - off) as usize];
            let src_off = s - eo;
            match src {
                Source::Bytes(b) => {
                    dst.copy_from_slice(&b[src_off as usize..(src_off as usize + dst.len())]);
                }
                Source::File { path, offset, .. } => {
                    let f = self.file(path)?;
                    f.seek(SeekFrom::Start(offset + src_off))
                        .with_path(path.to_path_buf())?;
                    f.read_exact(dst).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::UnexpectedEof {
                            Error::InvalidInput(format!(
                                "{} shrank while the image was being built",
                                path.display()
                            ))
                        } else {
                            Error::PathIo {
                                path: path.to_path_buf(),
                                source: e,
                            }
                        }
                    })?;
                }
                Source::Device { dev, offset, .. } => {
                    let mut d = dev.lock().map_err(|_| invalid("device lock poisoned"))?;
                    d.read_at(offset + src_off, dst)?;
                }
            }
            idx += 1;
        }
        Ok(())
    }

    fn file(&mut self, path: &Arc<Path>) -> Result<&mut File> {
        let reuse = matches!(&self.open, Some((p, _)) if Arc::ptr_eq(p, path) || **p == **path);
        if !reuse {
            let f = File::open(path).with_path(path.to_path_buf())?;
            self.open = Some((path.clone(), f));
        }
        Ok(&mut self.open.as_mut().expect("just opened").1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_reads() {
        let mut d = VirtualDisk::new(100);
        d.put(10, vec![1u8; 5]).unwrap();
        d.put(50, vec![2u8; 10]).unwrap();
        assert!(d.put(12, vec![3u8; 2]).is_err());
        assert!(d.put(95, vec![3u8; 10]).is_err());
        let mut buf = [9u8; 20];
        d.reader().read_at(5, &mut buf).unwrap();
        assert_eq!(&buf[..5], &[0; 5]);
        assert_eq!(&buf[5..10], &[1; 5]);
        assert_eq!(&buf[10..], &[0; 10]);
        assert!(d.is_sparse(0, 10));
        assert!(!d.is_sparse(0, 11));
        assert!(d.is_sparse(15, 35));
        assert!(!d.is_sparse(59, 1));
        assert!(d.is_sparse(60, 40));
        let mut outer = VirtualDisk::new(300);
        outer.embed(100, &d).unwrap();
        let mut b = [0u8; 1];
        outer.reader().read_at(150, &mut b).unwrap();
        assert_eq!(b[0], 2);
    }
}
