//! HFS+ on-disk structures (Apple Technical Note TN1150).

use crate::bytes::{PutBe, Reader};
use crate::error::Result;

/// `H+` volume signature.
pub const SIG_HFSPLUS: u16 = 0x482B;
/// `HX` (case-sensitive HFS+) volume signature.
pub const SIG_HFSX: u16 = 0x4858;
/// `BD` signature of a classic HFS volume (possibly wrapping HFS+).
pub const SIG_HFS: u16 = 0x4244;

/// Byte offset of the volume header from the start of the volume.
pub const VOLUME_HEADER_OFFSET: u64 = 1024;

/// Reserved catalog node IDs.
pub mod cnid {
    pub const ROOT_PARENT: u32 = 1;
    pub const ROOT_FOLDER: u32 = 2;
    pub const EXTENTS_FILE: u32 = 3;
    pub const CATALOG_FILE: u32 = 4;
    pub const BAD_BLOCKS_FILE: u32 = 5;
    pub const ALLOCATION_FILE: u32 = 6;
    pub const STARTUP_FILE: u32 = 7;
    pub const ATTRIBUTES_FILE: u32 = 8;
    pub const FIRST_USER: u32 = 16;
}

/// Volume attribute bits.
pub mod vol_attr {
    pub const UNMOUNTED: u32 = 1 << 8;
    pub const SPARED_BLOCKS: u32 = 1 << 9;
    pub const NO_CACHE_REQUIRED: u32 = 1 << 10;
    pub const BOOT_VOLUME_INCONSISTENT: u32 = 1 << 11;
    pub const CATALOG_NODE_IDS_REUSED: u32 = 1 << 12;
    pub const JOURNALED: u32 = 1 << 13;
    pub const SOFTWARE_LOCK: u32 = 1 << 15;
}

/// Catalog record flags.
pub mod rec_flags {
    pub const FILE_LOCKED: u16 = 0x0001;
    pub const THREAD_EXISTS: u16 = 0x0002;
    pub const HAS_ATTRIBUTES: u16 = 0x0004;
    pub const HAS_SECURITY: u16 = 0x0008;
    pub const HAS_FOLDER_COUNT: u16 = 0x0010;
    pub const HAS_LINK_CHAIN: u16 = 0x0020;
    pub const HAS_CHILD_LINK: u16 = 0x0040;
    pub const HAS_DATE_ADDED: u16 = 0x0080;
}

/// Finder flags (`FileInfo.finderFlags` / `FolderInfo.finderFlags`).
pub mod finder_flags {
    pub const IS_ON_DESK: u16 = 0x0001;
    pub const EXTENSION_HIDDEN: u16 = 0x0010;
    pub const IS_SHARED: u16 = 0x0040;
    pub const HAS_NO_INITS: u16 = 0x0080;
    pub const HAS_BEEN_INITED: u16 = 0x0100;
    pub const HAS_CUSTOM_ICON: u16 = 0x0400;
    pub const IS_STATIONERY: u16 = 0x0800;
    pub const NAME_LOCKED: u16 = 0x1000;
    pub const HAS_BUNDLE: u16 = 0x2000;
    pub const IS_INVISIBLE: u16 = 0x4000;
    pub const IS_ALIAS: u16 = 0x8000;
}

/// POSIX file type bits as stored in `HFSPlusBSDInfo.fileMode`.
pub mod mode {
    pub const S_IFMT: u16 = 0o170000;
    pub const S_IFIFO: u16 = 0o010000;
    pub const S_IFCHR: u16 = 0o020000;
    pub const S_IFDIR: u16 = 0o040000;
    pub const S_IFBLK: u16 = 0o060000;
    pub const S_IFREG: u16 = 0o100000;
    pub const S_IFLNK: u16 = 0o120000;
    pub const S_IFSOCK: u16 = 0o140000;
}

/// One contiguous run of allocation blocks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtentDescriptor {
    pub start_block: u32,
    pub block_count: u32,
}

/// Eight extents, the unit stored in fork data and extent records.
pub type ExtentRecord = [ExtentDescriptor; 8];

pub(crate) fn encode_extents(out: &mut Vec<u8>, ext: &ExtentRecord) {
    for e in ext {
        out.put_u32(e.start_block);
        out.put_u32(e.block_count);
    }
}

pub(crate) fn decode_extents(r: &mut Reader<'_>) -> Result<ExtentRecord> {
    let mut ext = ExtentRecord::default();
    for e in &mut ext {
        e.start_block = r.u32()?;
        e.block_count = r.u32()?;
    }
    Ok(ext)
}

/// `HFSPlusForkData`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForkData {
    pub logical_size: u64,
    pub clump_size: u32,
    pub total_blocks: u32,
    pub extents: ExtentRecord,
}

impl ForkData {
    pub const SIZE: usize = 80;

    /// A fork consisting of a single extent.
    pub fn contiguous(logical_size: u64, start_block: u32, block_count: u32, clump: u32) -> Self {
        let mut extents = ExtentRecord::default();
        if block_count > 0 {
            extents[0] = ExtentDescriptor {
                start_block,
                block_count,
            };
        }
        Self {
            logical_size,
            clump_size: clump,
            total_blocks: block_count,
            extents,
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.put_u64(self.logical_size);
        out.put_u32(self.clump_size);
        out.put_u32(self.total_blocks);
        encode_extents(out, &self.extents);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            logical_size: r.u64()?,
            clump_size: r.u32()?,
            total_blocks: r.u32()?,
            extents: decode_extents(r)?,
        })
    }

    /// Number of blocks described by the inline extents.
    pub fn inline_blocks(&self) -> u64 {
        self.extents.iter().map(|e| u64::from(e.block_count)).sum()
    }
}

/// `HFSPlusBSDInfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BsdInfo {
    pub owner_id: u32,
    pub group_id: u32,
    pub admin_flags: u8,
    pub owner_flags: u8,
    pub file_mode: u16,
    /// `iNodeNum`, `linkCount` or `rawDevice` depending on the file type.
    pub special: u32,
}

impl BsdInfo {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.put_u32(self.owner_id);
        out.put_u32(self.group_id);
        out.put_u8(self.admin_flags);
        out.put_u8(self.owner_flags);
        out.put_u16(self.file_mode);
        out.put_u32(self.special);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            owner_id: r.u32()?,
            group_id: r.u32()?,
            admin_flags: r.u8()?,
            owner_flags: r.u8()?,
            file_mode: r.u16()?,
            special: r.u32()?,
        })
    }
}

/// `HFSPlusVolumeHeader`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumeHeader {
    pub signature: u16,
    pub version: u16,
    pub attributes: u32,
    pub last_mounted_version: u32,
    pub journal_info_block: u32,
    pub create_date: u32,
    pub modify_date: u32,
    pub backup_date: u32,
    pub checked_date: u32,
    pub file_count: u32,
    pub folder_count: u32,
    pub block_size: u32,
    pub total_blocks: u32,
    pub free_blocks: u32,
    pub next_allocation: u32,
    pub rsrc_clump_size: u32,
    pub data_clump_size: u32,
    pub next_catalog_id: u32,
    pub write_count: u32,
    pub encodings_bitmap: u64,
    pub finder_info: [u32; 8],
    pub allocation_file: ForkData,
    pub extents_file: ForkData,
    pub catalog_file: ForkData,
    pub attributes_file: ForkData,
    pub startup_file: ForkData,
}

impl VolumeHeader {
    pub const SIZE: usize = 512;

    pub fn is_hfsx(&self) -> bool {
        self.signature == SIG_HFSX
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(Self::SIZE);
        o.put_u16(self.signature);
        o.put_u16(self.version);
        o.put_u32(self.attributes);
        o.put_u32(self.last_mounted_version);
        o.put_u32(self.journal_info_block);
        o.put_u32(self.create_date);
        o.put_u32(self.modify_date);
        o.put_u32(self.backup_date);
        o.put_u32(self.checked_date);
        o.put_u32(self.file_count);
        o.put_u32(self.folder_count);
        o.put_u32(self.block_size);
        o.put_u32(self.total_blocks);
        o.put_u32(self.free_blocks);
        o.put_u32(self.next_allocation);
        o.put_u32(self.rsrc_clump_size);
        o.put_u32(self.data_clump_size);
        o.put_u32(self.next_catalog_id);
        o.put_u32(self.write_count);
        o.put_u64(self.encodings_bitmap);
        for v in self.finder_info {
            o.put_u32(v);
        }
        self.allocation_file.encode(&mut o);
        self.extents_file.encode(&mut o);
        self.catalog_file.encode(&mut o);
        self.attributes_file.encode(&mut o);
        self.startup_file.encode(&mut o);
        debug_assert_eq!(o.len(), Self::SIZE);
        o
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let mut vh = Self {
            signature: r.u16()?,
            version: r.u16()?,
            attributes: r.u32()?,
            last_mounted_version: r.u32()?,
            journal_info_block: r.u32()?,
            create_date: r.u32()?,
            modify_date: r.u32()?,
            backup_date: r.u32()?,
            checked_date: r.u32()?,
            file_count: r.u32()?,
            folder_count: r.u32()?,
            block_size: r.u32()?,
            total_blocks: r.u32()?,
            free_blocks: r.u32()?,
            next_allocation: r.u32()?,
            rsrc_clump_size: r.u32()?,
            data_clump_size: r.u32()?,
            next_catalog_id: r.u32()?,
            write_count: r.u32()?,
            encodings_bitmap: r.u64()?,
            ..Default::default()
        };
        for v in &mut vh.finder_info {
            *v = r.u32()?;
        }
        vh.allocation_file = ForkData::decode(&mut r)?;
        vh.extents_file = ForkData::decode(&mut r)?;
        vh.catalog_file = ForkData::decode(&mut r)?;
        vh.attributes_file = ForkData::decode(&mut r)?;
        vh.startup_file = ForkData::decode(&mut r)?;
        Ok(vh)
    }
}

/// Catalog record types.
pub mod rec_type {
    pub const FOLDER: i16 = 1;
    pub const FILE: i16 = 2;
    pub const FOLDER_THREAD: i16 = 3;
    pub const FILE_THREAD: i16 = 4;
}

/// A catalog B-tree key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogKey {
    pub parent_id: u32,
    pub name: Vec<u16>,
}

impl CatalogKey {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(8 + self.name.len() * 2);
        o.put_u16((6 + self.name.len() * 2) as u16);
        o.put_u32(self.parent_id);
        o.put_u16(self.name.len() as u16);
        for &c in &self.name {
            o.put_u16(c);
        }
        o
    }

    /// Decodes a key whose `keyLength` field has already been consumed.
    pub(crate) fn decode_body(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let parent_id = r.u32()?;
        let len = r.u16()? as usize;
        let mut name = Vec::with_capacity(len);
        for _ in 0..len {
            name.push(r.u16()?);
        }
        Ok(Self { parent_id, name })
    }
}

/// `HFSPlusCatalogFolder`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderRecord {
    pub flags: u16,
    pub valence: u32,
    pub folder_id: u32,
    pub create_date: u32,
    pub content_mod_date: u32,
    pub attribute_mod_date: u32,
    pub access_date: u32,
    pub backup_date: u32,
    pub permissions: BsdInfo,
    /// `FolderInfo` (16 bytes).
    pub user_info: [u8; 16],
    /// `ExtendedFolderInfo` (16 bytes).
    pub finder_info: [u8; 16],
    pub text_encoding: u32,
    pub folder_count: u32,
}

/// `HFSPlusCatalogFile`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileRecord {
    pub flags: u16,
    pub file_id: u32,
    pub create_date: u32,
    pub content_mod_date: u32,
    pub attribute_mod_date: u32,
    pub access_date: u32,
    pub backup_date: u32,
    pub permissions: BsdInfo,
    /// `FileInfo` (16 bytes).
    pub user_info: [u8; 16],
    /// `ExtendedFileInfo` (16 bytes).
    pub finder_info: [u8; 16],
    pub text_encoding: u32,
    pub data_fork: ForkData,
    pub resource_fork: ForkData,
}

impl FileRecord {
    pub fn file_type(&self) -> [u8; 4] {
        self.user_info[0..4].try_into().expect("4 bytes")
    }
    pub fn creator(&self) -> [u8; 4] {
        self.user_info[4..8].try_into().expect("4 bytes")
    }
}

/// `HFSPlusCatalogThread`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadRecord {
    pub parent_id: u32,
    pub name: Vec<u16>,
}

/// Any catalog leaf record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogRecord {
    Folder(FolderRecord),
    File(FileRecord),
    FolderThread(ThreadRecord),
    FileThread(ThreadRecord),
}

impl CatalogRecord {
    pub const FOLDER_SIZE: usize = 88;
    pub const FILE_SIZE: usize = 248;

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(Self::FILE_SIZE);
        match self {
            Self::Folder(f) => {
                o.put_i16(rec_type::FOLDER);
                o.put_u16(f.flags);
                o.put_u32(f.valence);
                o.put_u32(f.folder_id);
                o.put_u32(f.create_date);
                o.put_u32(f.content_mod_date);
                o.put_u32(f.attribute_mod_date);
                o.put_u32(f.access_date);
                o.put_u32(f.backup_date);
                f.permissions.encode(&mut o);
                o.extend_from_slice(&f.user_info);
                o.extend_from_slice(&f.finder_info);
                o.put_u32(f.text_encoding);
                o.put_u32(f.folder_count);
            }
            Self::File(f) => {
                o.put_i16(rec_type::FILE);
                o.put_u16(f.flags);
                o.put_u32(0);
                o.put_u32(f.file_id);
                o.put_u32(f.create_date);
                o.put_u32(f.content_mod_date);
                o.put_u32(f.attribute_mod_date);
                o.put_u32(f.access_date);
                o.put_u32(f.backup_date);
                f.permissions.encode(&mut o);
                o.extend_from_slice(&f.user_info);
                o.extend_from_slice(&f.finder_info);
                o.put_u32(f.text_encoding);
                o.put_u32(0);
                f.data_fork.encode(&mut o);
                f.resource_fork.encode(&mut o);
            }
            Self::FolderThread(t) | Self::FileThread(t) => {
                o.put_i16(if matches!(self, Self::FolderThread(_)) {
                    rec_type::FOLDER_THREAD
                } else {
                    rec_type::FILE_THREAD
                });
                o.put_i16(0);
                o.put_u32(t.parent_id);
                o.put_u16(t.name.len() as u16);
                for &c in &t.name {
                    o.put_u16(c);
                }
            }
        }
        o
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let ty = r.i16()?;
        Ok(match ty {
            rec_type::FOLDER => {
                let flags = r.u16()?;
                Self::Folder(FolderRecord {
                    flags,
                    valence: r.u32()?,
                    folder_id: r.u32()?,
                    create_date: r.u32()?,
                    content_mod_date: r.u32()?,
                    attribute_mod_date: r.u32()?,
                    access_date: r.u32()?,
                    backup_date: r.u32()?,
                    permissions: BsdInfo::decode(&mut r)?,
                    user_info: r.array()?,
                    finder_info: r.array()?,
                    text_encoding: r.u32()?,
                    folder_count: r.u32()?,
                })
            }
            rec_type::FILE => {
                let flags = r.u16()?;
                r.skip(4)?;
                let mut f = FileRecord {
                    flags,
                    file_id: r.u32()?,
                    create_date: r.u32()?,
                    content_mod_date: r.u32()?,
                    attribute_mod_date: r.u32()?,
                    access_date: r.u32()?,
                    backup_date: r.u32()?,
                    permissions: BsdInfo::decode(&mut r)?,
                    user_info: r.array()?,
                    finder_info: r.array()?,
                    text_encoding: r.u32()?,
                    ..Default::default()
                };
                r.skip(4)?;
                f.data_fork = ForkData::decode(&mut r)?;
                f.resource_fork = ForkData::decode(&mut r)?;
                Self::File(f)
            }
            rec_type::FOLDER_THREAD | rec_type::FILE_THREAD => {
                r.skip(2)?;
                let parent_id = r.u32()?;
                let len = r.u16()? as usize;
                let mut name = Vec::with_capacity(len);
                for _ in 0..len {
                    name.push(r.u16()?);
                }
                let t = ThreadRecord { parent_id, name };
                if ty == rec_type::FOLDER_THREAD {
                    Self::FolderThread(t)
                } else {
                    Self::FileThread(t)
                }
            }
            other => {
                return Err(crate::error::corrupt(format!(
                    "unknown catalog record type {other}"
                )));
            }
        })
    }
}

/// Extents overflow key (`HFSPlusExtentKey`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExtentKey {
    pub file_id: u32,
    pub fork_type: u8,
    pub start_block: u32,
}

impl ExtentKey {
    pub const KEY_LENGTH: u16 = 10;

    #[allow(dead_code)]
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(12);
        o.put_u16(Self::KEY_LENGTH);
        o.put_u8(self.fork_type);
        o.put_u8(0);
        o.put_u32(self.file_id);
        o.put_u32(self.start_block);
        o
    }

    pub(crate) fn decode_body(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let fork_type = r.u8()?;
        r.skip(1)?;
        Ok(Self {
            fork_type,
            file_id: r.u32()?,
            start_block: r.u32()?,
        })
    }

    /// Ordering used by the extents overflow B-tree.
    pub fn sort_key(&self) -> (u32, u8, u32) {
        (self.file_id, self.fork_type, self.start_block)
    }
}

/// Attributes B-tree key (`HFSPlusAttrKey`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrKey {
    pub file_id: u32,
    pub start_block: u32,
    pub name: Vec<u16>,
}

impl AttrKey {
    pub const MAX_KEY_LENGTH: u16 = 266 - 2;

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(14 + self.name.len() * 2);
        o.put_u16((12 + self.name.len() * 2) as u16);
        o.put_u16(0);
        o.put_u32(self.file_id);
        o.put_u32(self.start_block);
        o.put_u16(self.name.len() as u16);
        for &c in &self.name {
            o.put_u16(c);
        }
        o
    }

    pub(crate) fn decode_body(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        r.skip(2)?;
        let file_id = r.u32()?;
        let start_block = r.u32()?;
        let len = r.u16()? as usize;
        let mut name = Vec::with_capacity(len);
        for _ in 0..len {
            name.push(r.u16()?);
        }
        Ok(Self {
            file_id,
            start_block,
            name,
        })
    }
}

/// Attribute record types.
pub mod attr_type {
    pub const INLINE_DATA: u32 = 0x10;
    pub const FORK_DATA: u32 = 0x20;
    pub const EXTENTS: u32 = 0x30;
}

/// An attributes B-tree record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrRecord {
    Inline(Vec<u8>),
    Fork(ForkData),
    Extents(ExtentRecord),
}

impl AttrRecord {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Self::Inline(data) => {
                o.put_u32(attr_type::INLINE_DATA);
                o.put_u32(0);
                o.put_u32(0);
                o.put_u32(data.len() as u32);
                o.extend_from_slice(data);
                if data.len() % 2 == 1 {
                    o.put_u8(0);
                }
            }
            Self::Fork(fork) => {
                o.put_u32(attr_type::FORK_DATA);
                o.put_u32(0);
                fork.encode(&mut o);
            }
            Self::Extents(ext) => {
                o.put_u32(attr_type::EXTENTS);
                o.put_u32(0);
                encode_extents(&mut o, ext);
            }
        }
        o
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        Ok(match r.u32()? {
            attr_type::INLINE_DATA => {
                r.skip(8)?;
                let len = r.u32()? as usize;
                Self::Inline(r.bytes(len)?.to_vec())
            }
            attr_type::FORK_DATA => {
                r.skip(4)?;
                Self::Fork(ForkData::decode(&mut r)?)
            }
            attr_type::EXTENTS => {
                r.skip(4)?;
                Self::Extents(decode_extents(&mut r)?)
            }
            other => {
                return Err(crate::error::corrupt(format!(
                    "unknown attribute record type {other:#x}"
                )));
            }
        })
    }
}

/// B-tree node kinds.
pub mod node_kind {
    pub const LEAF: i8 = -1;
    pub const INDEX: i8 = 0;
    pub const HEADER: i8 = 1;
    pub const MAP: i8 = 2;
}

/// B-tree header attribute bits.
pub mod bt_attr {
    pub const BAD_CLOSE: u32 = 0x0000_0001;
    pub const BIG_KEYS: u32 = 0x0000_0002;
    pub const VARIABLE_INDEX_KEYS: u32 = 0x0000_0004;
}

/// Catalog key comparison types stored in the B-tree header.
pub mod key_compare {
    pub const CASE_FOLDING: u8 = 0xCF;
    pub const BINARY: u8 = 0xBC;
}

/// `BTNodeDescriptor`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeDescriptor {
    pub flink: u32,
    pub blink: u32,
    pub kind: i8,
    pub height: u8,
    pub num_records: u16,
}

impl NodeDescriptor {
    pub const SIZE: usize = 14;

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.put_u32(self.flink);
        out.put_u32(self.blink);
        out.put_u8(self.kind as u8);
        out.put_u8(self.height);
        out.put_u16(self.num_records);
        out.put_u16(0);
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        Ok(Self {
            flink: r.u32()?,
            blink: r.u32()?,
            kind: r.u8()? as i8,
            height: r.u8()?,
            num_records: r.u16()?,
        })
    }
}

/// `BTHeaderRec`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeaderRecord {
    pub tree_depth: u16,
    pub root_node: u32,
    pub leaf_records: u32,
    pub first_leaf_node: u32,
    pub last_leaf_node: u32,
    pub node_size: u16,
    pub max_key_length: u16,
    pub total_nodes: u32,
    pub free_nodes: u32,
    pub clump_size: u32,
    pub btree_type: u8,
    pub key_compare_type: u8,
    pub attributes: u32,
}

impl HeaderRecord {
    pub const SIZE: usize = 106;

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.put_u16(self.tree_depth);
        out.put_u32(self.root_node);
        out.put_u32(self.leaf_records);
        out.put_u32(self.first_leaf_node);
        out.put_u32(self.last_leaf_node);
        out.put_u16(self.node_size);
        out.put_u16(self.max_key_length);
        out.put_u32(self.total_nodes);
        out.put_u32(self.free_nodes);
        out.put_u16(0);
        out.put_u32(self.clump_size);
        out.put_u8(self.btree_type);
        out.put_u8(self.key_compare_type);
        out.put_u32(self.attributes);
        out.put_zeros(64);
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let mut h = Self {
            tree_depth: r.u16()?,
            root_node: r.u32()?,
            leaf_records: r.u32()?,
            first_leaf_node: r.u32()?,
            last_leaf_node: r.u32()?,
            node_size: r.u16()?,
            max_key_length: r.u16()?,
            total_nodes: r.u32()?,
            free_nodes: r.u32()?,
            ..Default::default()
        };
        r.skip(2)?;
        h.clump_size = r.u32()?;
        h.btree_type = r.u8()?;
        h.key_compare_type = r.u8()?;
        h.attributes = r.u32()?;
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_sizes() {
        let folder = CatalogRecord::Folder(FolderRecord::default()).encode();
        assert_eq!(folder.len(), CatalogRecord::FOLDER_SIZE);
        let file = CatalogRecord::File(FileRecord::default()).encode();
        assert_eq!(file.len(), CatalogRecord::FILE_SIZE);
        assert_eq!(VolumeHeader::default().encode().len(), VolumeHeader::SIZE);
        let mut h = Vec::new();
        HeaderRecord::default().encode(&mut h);
        assert_eq!(h.len(), HeaderRecord::SIZE);
    }

    #[test]
    fn catalog_roundtrip() {
        let rec = CatalogRecord::File(FileRecord {
            flags: 2,
            file_id: 77,
            data_fork: ForkData::contiguous(5000, 10, 2, 4096),
            ..Default::default()
        });
        assert_eq!(CatalogRecord::decode(&rec.encode()).unwrap(), rec);
        let th = CatalogRecord::FolderThread(ThreadRecord {
            parent_id: 1,
            name: vec![0x41, 0x42],
        });
        assert_eq!(CatalogRecord::decode(&th.encode()).unwrap(), th);
        let vh = VolumeHeader {
            signature: SIG_HFSPLUS,
            total_blocks: 99,
            finder_info: [1, 2, 3, 4, 5, 6, 7, 8],
            ..Default::default()
        };
        assert_eq!(VolumeHeader::decode(&vh.encode()).unwrap(), vh);
    }
}
