//! Opening existing disk images for inspection, extraction and conversion.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::disk::{Source, VirtualDisk};
use crate::error::{PathContext, Result, invalid};
use crate::hfs::Volume;
use crate::io::{ReadAt, SeekReader, Slice};
use crate::partition::{MapKind, PartitionInfo, read_partitions};
use crate::udif::{
    self, Region, SECTOR_SIZE, UdifImage, VerifyReport, WriteOptions, WriteSummary, chunk,
};

/// The backing store of an opened image.
#[derive(Debug)]
pub enum ImageSource {
    /// A UDIF (`.dmg`) image.
    Udif(Box<UdifImage<SeekReader<File>>>),
    /// A raw disk or volume image (`.img`, `.cdr`, `.iso`).
    Raw(SeekReader<File>),
}

impl ReadAt for ImageSource {
    fn size(&self) -> u64 {
        match self {
            Self::Udif(u) => u.size(),
            Self::Raw(r) => r.size(),
        }
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Udif(u) => u.read_at(offset, buf),
            Self::Raw(r) => r.read_at(offset, buf),
        }
    }
}

/// An opened disk image.
#[derive(Debug)]
pub struct DiskImage {
    source: ImageSource,
    map: MapKind,
    partitions: Vec<PartitionInfo>,
}

impl DiskImage {
    /// Opens a UDIF or raw image.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut raw = SeekReader::open(path)?;
        let source = if udif::is_udif(&mut raw)? {
            ImageSource::Udif(Box::new(UdifImage::open(raw)?))
        } else {
            ImageSource::Raw(raw)
        };
        Self::from_source(source)
    }

    /// Wraps an already opened source.
    pub fn from_source(mut source: ImageSource) -> Result<Self> {
        let (map, partitions) = read_partitions(&mut source)?;
        Ok(Self {
            source,
            map,
            partitions,
        })
    }

    pub fn source(&self) -> &ImageSource {
        &self.source
    }

    pub fn source_mut(&mut self) -> &mut ImageSource {
        &mut self.source
    }

    /// The UDIF structures, if this is a UDIF image.
    pub fn udif(&self) -> Option<&UdifImage<SeekReader<File>>> {
        match &self.source {
            ImageSource::Udif(u) => Some(u),
            ImageSource::Raw(_) => None,
        }
    }

    /// Size of the (uncompressed) disk in bytes.
    pub fn size(&self) -> u64 {
        self.source.size()
    }

    pub fn partition_map(&self) -> MapKind {
        self.map
    }

    pub fn partitions(&self) -> &[PartitionInfo] {
        &self.partitions
    }

    /// Verifies UDIF checksums (`None` for raw images).
    pub fn verify_checksums(&mut self) -> Result<Option<VerifyReport>> {
        match &mut self.source {
            ImageSource::Udif(u) => u.verify().map(Some),
            ImageSource::Raw(_) => Ok(None),
        }
    }

    /// The first HFS+ partition.
    pub fn hfs_partition(&self) -> Option<&PartitionInfo> {
        self.partitions.iter().find(|p| p.is_hfs)
    }

    /// Opens the HFS+ volume of the image.
    pub fn into_volume(self) -> Result<Volume<Slice<ImageSource>>> {
        let p = self.hfs_partition().cloned().ok_or_else(|| {
            crate::Error::Unsupported(
                "image contains no HFS+ volume (APFS images are not supported)".into(),
            )
        })?;
        Volume::open(Slice::new(self.source, p.offset, p.size)?)
    }

    /// The image's regions as they would be described when rewriting it.
    pub fn regions(&self) -> Vec<Region> {
        let whole = || {
            vec![Region {
                name: "whole disk (Apple_HFS : 0)".into(),
                id: -1,
                first_sector: 0,
                sector_count: self.size() / SECTOR_SIZE,
                free: false,
            }]
        };
        let Some(u) = self.udif() else { return whole() };
        let mut regions: Vec<Region> =
            u.blkx()
                .iter()
                .enumerate()
                .map(|(i, b)| Region {
                    name: b.name.clone(),
                    id: b.id.parse().unwrap_or(i as i32 - 1),
                    first_sector: b.table.first_sector,
                    sector_count: b.table.sector_count,
                    free: b.table.chunks.iter().all(|c| {
                        matches!(c.kind, chunk::IGNORE | chunk::TERMINATOR | chunk::COMMENT)
                    }),
                })
                .collect();
        regions.sort_by_key(|r| r.first_sector);
        let mut expect = 0;
        for r in &regions {
            if r.first_sector != expect {
                return whole();
            }
            expect += r.sector_count;
        }
        if expect * SECTOR_SIZE != self.size() {
            return whole();
        }
        regions
    }

    /// Re-encodes the image in another format.
    pub fn convert(self, out: &mut dyn Write, opts: &WriteOptions) -> Result<WriteSummary> {
        let regions = self.regions();
        let size = self.size();
        let dev: Arc<Mutex<dyn ReadAt + Send>> = Arc::new(Mutex::new(self.source));
        let mut disk = VirtualDisk::new(size);
        disk.put(
            0,
            Source::Device {
                dev,
                offset: 0,
                len: size,
            },
        )?;
        udif::write(&disk, &regions, opts, out, None)
    }
}

/// Converts the image at `input` to `output` in a new format.
pub fn convert(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    opts: &WriteOptions,
) -> Result<WriteSummary> {
    let (input, output) = (input.as_ref(), output.as_ref());
    if input == output {
        return Err(invalid("input and output must differ"));
    }
    let img = DiskImage::open(input)?;
    let mut tmp = output.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp = PathBuf::from(tmp);
    let res = (|| {
        let f = File::create(&tmp).with_path(&tmp)?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        let s = img.convert(&mut w, opts)?;
        w.flush()?;
        drop(w);
        if output.exists() {
            fs::remove_file(output).with_path(output)?;
        }
        fs::rename(&tmp, output).with_path(output)?;
        Ok(s)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}
