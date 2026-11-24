//! Reading HFS+ / HFSX volumes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::SystemTime;

use super::btree::BTreeFile;
use super::format::*;
use super::time::from_hfs;
use super::unicode::from_hfs_name;
use crate::error::{Result, corrupt};
use crate::io::ReadAt;

/// `st_flags` bit marking a file as transparently compressed (decmpfs).
const UF_COMPRESSED: u8 = 0x20;
const HARD_LINK_TYPE: [u8; 4] = *b"hlnk";
const HARD_LINK_CREATOR: [u8; 4] = *b"hfs+";
const DIR_LINK_TYPE: [u8; 4] = *b"fdrp";
const DIR_LINK_CREATOR: [u8; 4] = *b"MACS";
const PRIVATE_DATA_DIR: &str = "\0\0\0\0HFS+ Private Data";
const PRIVATE_DIR_DATA_DIR: &str = ".HFS+ Private Directory Data\r";

/// The type of a catalog entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    /// A file hard link; the payload is the indirect node number.
    HardLink(u32),
    /// A directory hard link; the payload is the indirect node number.
    DirHardLink(u32),
    Fifo,
    CharDevice,
    BlockDevice,
    Socket,
}

/// A file or folder found in the catalog.
#[derive(Clone, Debug)]
pub struct Entry {
    pub cnid: u32,
    pub parent: u32,
    /// POSIX name (`:` and `/` swapped back).
    pub name: String,
    /// Raw catalog name.
    pub hfs_name: Vec<u16>,
    /// The folder or file record.
    pub record: CatalogRecord,
}

impl Entry {
    pub fn permissions(&self) -> &BsdInfo {
        match &self.record {
            CatalogRecord::Folder(f) => &f.permissions,
            CatalogRecord::File(f) => &f.permissions,
            _ => unreachable!("entries hold folder or file records"),
        }
    }

    /// Full `st_mode`, including the file type bits.
    pub fn mode(&self) -> u16 {
        let m = self.permissions().file_mode;
        if m & mode::S_IFMT != 0 {
            m
        } else if self.is_dir() {
            mode::S_IFDIR | 0o755
        } else {
            mode::S_IFREG | 0o644
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.record, CatalogRecord::Folder(_))
    }

    pub fn kind(&self) -> EntryKind {
        match &self.record {
            CatalogRecord::Folder(_) => EntryKind::Directory,
            CatalogRecord::File(f) => {
                let (ty, cr) = (f.file_type(), f.creator());
                if ty == HARD_LINK_TYPE && cr == HARD_LINK_CREATOR {
                    return EntryKind::HardLink(f.permissions.special);
                }
                if ty == DIR_LINK_TYPE && cr == DIR_LINK_CREATOR {
                    return EntryKind::DirHardLink(f.permissions.special);
                }
                match f.permissions.file_mode & mode::S_IFMT {
                    mode::S_IFLNK => EntryKind::Symlink,
                    mode::S_IFIFO => EntryKind::Fifo,
                    mode::S_IFCHR => EntryKind::CharDevice,
                    mode::S_IFBLK => EntryKind::BlockDevice,
                    mode::S_IFSOCK => EntryKind::Socket,
                    _ => EntryKind::File,
                }
            }
            _ => unreachable!("entries hold folder or file records"),
        }
    }

    /// Logical size of the data fork (0 for folders).
    pub fn data_size(&self) -> u64 {
        match &self.record {
            CatalogRecord::File(f) => f.data_fork.logical_size,
            _ => 0,
        }
    }

    /// Logical size of the resource fork (0 for folders).
    pub fn rsrc_size(&self) -> u64 {
        match &self.record {
            CatalogRecord::File(f) => f.resource_fork.logical_size,
            _ => 0,
        }
    }

    pub fn modified(&self) -> SystemTime {
        from_hfs(match &self.record {
            CatalogRecord::Folder(f) => f.content_mod_date,
            CatalogRecord::File(f) => f.content_mod_date,
            _ => 0,
        })
    }

    pub fn created(&self) -> SystemTime {
        from_hfs(match &self.record {
            CatalogRecord::Folder(f) => f.create_date,
            CatalogRecord::File(f) => f.create_date,
            _ => 0,
        })
    }

    /// The 32 bytes of Finder info (user info followed by extended info).
    pub fn finder_info(&self) -> [u8; 32] {
        let (a, b) = match &self.record {
            CatalogRecord::Folder(f) => (f.user_info, f.finder_info),
            CatalogRecord::File(f) => (f.user_info, f.finder_info),
            _ => ([0; 16], [0; 16]),
        };
        let mut out = [0u8; 32];
        out[..16].copy_from_slice(&a);
        out[16..].copy_from_slice(&b);
        out
    }

    pub fn finder_flags(&self) -> u16 {
        let fi = self.finder_info();
        u16::from_be_bytes([fi[8], fi[9]])
    }

    fn is_compressed(&self) -> bool {
        self.permissions().owner_flags & UF_COMPRESSED != 0
    }
}

/// A mounted (read-only, in-process) HFS+ volume.
#[derive(Debug)]
pub struct Volume<R> {
    dev: R,
    base: u64,
    header: VolumeHeader,
    overflow: Vec<(ExtentKey, ExtentRecord)>,
    catalog: BTreeFile,
    entries: HashMap<u32, Entry>,
    children: HashMap<u32, Vec<u32>>,
    threads: HashMap<u32, (bool, ThreadRecord)>,
    attrs: HashMap<u32, Vec<(String, AttrRecord, Vec<ExtentRecord>)>>,
    attributes: Option<BTreeFile>,
    extents: Option<BTreeFile>,
}

impl<R: ReadAt> Volume<R> {
    /// Opens the HFS+ volume stored on `dev` (a partition or bare volume).
    pub fn open(mut dev: R) -> Result<Self> {
        let mut base = 0u64;
        let mut vh_buf = dev.read_vec(VOLUME_HEADER_OFFSET, 512)?;
        let sig = u16::from_be_bytes([vh_buf[0], vh_buf[1]]);
        if sig == SIG_HFS {
            // Classic HFS wrapper around an embedded HFS+ volume.
            let mdb = &vh_buf;
            let embed_sig = u16::from_be_bytes([mdb[0x7C], mdb[0x7D]]);
            if embed_sig != SIG_HFSPLUS {
                return Err(crate::Error::Unsupported("classic HFS volumes".into()));
            }
            let al_blk_siz = u64::from(u32::from_be_bytes(mdb[0x14..0x18].try_into().unwrap()));
            let al_bl_st = u64::from(u16::from_be_bytes([mdb[0x1C], mdb[0x1D]]));
            let start = u64::from(u16::from_be_bytes([mdb[0x7E], mdb[0x7F]]));
            base = al_bl_st * 512 + start * al_blk_siz;
            vh_buf = dev.read_vec(base + VOLUME_HEADER_OFFSET, 512)?;
        }
        let header = VolumeHeader::decode(&vh_buf)?;
        if header.signature != SIG_HFSPLUS && header.signature != SIG_HFSX {
            return Err(corrupt(format!(
                "no HFS+ volume header (signature {:#06x})",
                header.signature
            )));
        }
        if !header.block_size.is_power_of_two() || header.block_size < 512 {
            return Err(corrupt(format!("invalid block size {}", header.block_size)));
        }

        let mut vol = Self {
            dev,
            base,
            header,
            overflow: Vec::new(),
            catalog: empty_tree(),
            entries: HashMap::new(),
            children: HashMap::new(),
            threads: HashMap::new(),
            attrs: HashMap::new(),
            attributes: None,
            extents: None,
        };

        // Extents overflow file (its own extents never overflow).
        let ext_fork = vol.header.extents_file.clone();
        if ext_fork.logical_size > 0 {
            let data = vol.read_fork_all(cnid::EXTENTS_FILE, 0, &ext_fork)?;
            let tree = BTreeFile::new(data)?;
            for (k, d) in tree.leaf_records()? {
                let key = ExtentKey::decode_body(&k[2..])?;
                let ext = decode_extents(&mut crate::bytes::Reader::new(d))?;
                vol.overflow.push((key, ext));
            }
            vol.overflow.sort_by_key(|(k, _)| k.sort_key());
            vol.extents = Some(tree);
        }

        let cat_fork = vol.header.catalog_file.clone();
        vol.catalog = BTreeFile::new(vol.read_fork_all(cnid::CATALOG_FILE, 0, &cat_fork)?)?;
        vol.index_catalog()?;

        let attr_fork = vol.header.attributes_file.clone();
        if attr_fork.logical_size > 0 {
            let tree = BTreeFile::new(vol.read_fork_all(cnid::ATTRIBUTES_FILE, 0, &attr_fork)?)?;
            vol.index_attributes(&tree)?;
            vol.attributes = Some(tree);
        }
        Ok(vol)
    }

    fn index_catalog(&mut self) -> Result<()> {
        for (k, d) in self.catalog.leaf_records()? {
            let key = CatalogKey::decode_body(&k[2..])?;
            let rec = CatalogRecord::decode(d)?;
            let cnid = match &rec {
                CatalogRecord::Folder(f) => f.folder_id,
                CatalogRecord::File(f) => f.file_id,
                CatalogRecord::FolderThread(t) | CatalogRecord::FileThread(t) => {
                    let is_folder = matches!(rec, CatalogRecord::FolderThread(_));
                    self.threads.insert(key.parent_id, (is_folder, t.clone()));
                    continue;
                }
            };
            self.children.entry(key.parent_id).or_default().push(cnid);
            self.entries.insert(
                cnid,
                Entry {
                    cnid,
                    parent: key.parent_id,
                    name: from_hfs_name(&key.name),
                    hfs_name: key.name,
                    record: rec,
                },
            );
        }
        if !self.entries.contains_key(&cnid::ROOT_FOLDER) {
            return Err(corrupt("catalog has no root folder"));
        }
        Ok(())
    }

    fn index_attributes(&mut self, tree: &BTreeFile) -> Result<()> {
        for (k, d) in tree.leaf_records()? {
            let key = AttrKey::decode_body(&k[2..])?;
            let rec = AttrRecord::decode(d)?;
            let name = String::from_utf16_lossy(&key.name);
            let list = self.attrs.entry(key.file_id).or_default();
            match rec {
                AttrRecord::Extents(ext) => {
                    if let Some(last) = list.iter_mut().rev().find(|(n, _, _)| *n == name) {
                        last.2.push(ext);
                    }
                }
                rec => list.push((name, rec, Vec::new())),
            }
        }
        Ok(())
    }

    /// The volume header.
    pub fn header(&self) -> &VolumeHeader {
        &self.header
    }

    /// The catalog B-tree.
    pub fn catalog(&self) -> &BTreeFile {
        &self.catalog
    }

    /// The attributes B-tree, if the volume has one.
    pub fn attributes_tree(&self) -> Option<&BTreeFile> {
        self.attributes.as_ref()
    }

    /// The extents overflow B-tree, if the volume has one.
    pub fn extents_tree(&self) -> Option<&BTreeFile> {
        self.extents.as_ref()
    }

    /// Records of the extents overflow file.
    pub fn overflow_extents(&self) -> &[(ExtentKey, ExtentRecord)] {
        &self.overflow
    }

    /// Thread records keyed by CNID (`true` for folder threads).
    pub fn threads(&self) -> &HashMap<u32, (bool, ThreadRecord)> {
        &self.threads
    }

    /// The volume name.
    pub fn name(&self) -> String {
        self.entries[&cnid::ROOT_FOLDER].name.clone()
    }

    pub fn block_size(&self) -> u64 {
        u64::from(self.header.block_size)
    }

    /// The root folder.
    pub fn root(&self) -> &Entry {
        &self.entries[&cnid::ROOT_FOLDER]
    }

    /// Looks up an entry by CNID.
    pub fn entry(&self, cnid: u32) -> Option<&Entry> {
        self.entries.get(&cnid)
    }

    /// All catalog entries (unordered).
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// Children of a folder in catalog order.
    pub fn children(&self, cnid: u32) -> Vec<&Entry> {
        self.children
            .get(&cnid)
            .map(|v| v.iter().filter_map(|c| self.entries.get(c)).collect())
            .unwrap_or_default()
    }

    /// Children of a folder, hiding HFS+ private metadata folders at the root.
    pub fn list(&self, cnid: u32) -> Vec<&Entry> {
        self.children(cnid)
            .into_iter()
            .filter(|e| {
                !(e.parent == cnid::ROOT_FOLDER
                    && (e.name == PRIVATE_DATA_DIR || e.name == PRIVATE_DIR_DATA_DIR))
            })
            .collect()
    }

    /// Resolves a `/`-separated path (case handling follows the volume).
    pub fn lookup(&self, path: &str) -> Option<&Entry> {
        let mut cur = self.root();
        for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            let want = super::unicode::to_hfs_name(comp).ok()?;
            cur = self.children(cur.cnid).into_iter().find(|e| {
                if self.header.is_hfsx() {
                    e.hfs_name == want
                } else {
                    super::unicode::fast_unicode_compare(&e.hfs_name, &want).is_eq()
                }
            })?;
        }
        Some(cur)
    }

    /// Path of an entry from the root, starting with `/`.
    pub fn path_of(&self, cnid: u32) -> String {
        let mut parts = Vec::new();
        let mut cur = cnid;
        while cur != cnid::ROOT_FOLDER {
            match self.entries.get(&cur) {
                Some(e) => {
                    parts.push(e.name.as_str());
                    cur = e.parent;
                }
                None => break,
            }
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    /// Resolves the extents of a fork, consulting the overflow file.
    pub fn fork_extents(
        &self,
        file_id: u32,
        fork_type: u8,
        fork: &ForkData,
    ) -> Result<Vec<ExtentDescriptor>> {
        let mut out: Vec<ExtentDescriptor> = fork
            .extents
            .iter()
            .copied()
            .filter(|e| e.block_count > 0)
            .collect();
        let mut have: u64 = out.iter().map(|e| u64::from(e.block_count)).sum();
        while have < u64::from(fork.total_blocks) {
            let key = (file_id, fork_type, have as u32);
            let rec = self
                .overflow
                .binary_search_by_key(&key, |(k, _)| k.sort_key())
                .map(|i| &self.overflow[i].1)
                .map_err(|_| corrupt(format!("missing overflow extents for file {file_id}")))?;
            let before = have;
            for e in rec.iter().filter(|e| e.block_count > 0) {
                out.push(*e);
                have += u64::from(e.block_count);
            }
            if have == before {
                return Err(corrupt(format!(
                    "empty overflow extent record for file {file_id}"
                )));
            }
        }
        Ok(out)
    }

    /// Reads `buf.len()` bytes of a fork at `offset`.
    pub fn read_fork(
        &mut self,
        extents: &[ExtentDescriptor],
        offset: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        let bs = self.block_size();
        let mut pos = offset;
        let mut done = 0usize;
        let mut ext_start = 0u64; // logical byte offset of current extent
        for e in extents {
            if done == buf.len() {
                break;
            }
            let ext_len = u64::from(e.block_count) * bs;
            if pos < ext_start + ext_len {
                let within = pos - ext_start;
                let n = ((ext_len - within) as usize).min(buf.len() - done);
                let phys = self.base + u64::from(e.start_block) * bs + within;
                self.dev.read_at(phys, &mut buf[done..done + n])?;
                done += n;
                pos += n as u64;
            }
            ext_start += ext_len;
        }
        if done != buf.len() {
            return Err(corrupt("read beyond end of fork"));
        }
        Ok(())
    }

    fn read_fork_all(&mut self, file_id: u32, fork_type: u8, fork: &ForkData) -> Result<Vec<u8>> {
        let extents = self.fork_extents(file_id, fork_type, fork)?;
        let len = usize::try_from(fork.logical_size).map_err(|_| corrupt("fork too large"))?;
        let mut buf = vec![0u8; len];
        self.read_fork(&extents, 0, &mut buf)?;
        Ok(buf)
    }

    fn stream_fork<W: Write + ?Sized>(
        &mut self,
        file_id: u32,
        fork_type: u8,
        fork: &ForkData,
        out: &mut W,
    ) -> Result<u64> {
        let extents = self.fork_extents(file_id, fork_type, fork)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut off = 0u64;
        while off < fork.logical_size {
            let n = ((fork.logical_size - off) as usize).min(buf.len());
            self.read_fork(&extents, off, &mut buf[..n])?;
            out.write_all(&buf[..n])?;
            off += n as u64;
        }
        Ok(off)
    }

    /// Follows hard links to the entry that holds the file's data.
    pub fn resolve_link<'a>(&'a self, entry: &'a Entry) -> Result<&'a Entry> {
        let (dir, prefix, inode) = match entry.kind() {
            EntryKind::HardLink(n) => (PRIVATE_DATA_DIR, "iNode", n),
            EntryKind::DirHardLink(n) => (PRIVATE_DIR_DATA_DIR, "dir_", n),
            _ => return Ok(entry),
        };
        let private = self
            .children(cnid::ROOT_FOLDER)
            .into_iter()
            .find(|e| e.name == dir)
            .ok_or_else(|| corrupt("hard link without private data folder"))?;
        let name = format!("{prefix}{inode}");
        self.children(private.cnid)
            .into_iter()
            .find(|e| e.name == name)
            .ok_or_else(|| corrupt(format!("hard link target {name} missing")))
    }

    /// Extended attributes of an entry (excluding `com.apple.decmpfs` and
    /// resource-fork data, which are exposed separately).
    pub fn xattrs(&mut self, cnid: u32) -> Result<Vec<(String, Vec<u8>)>> {
        let list = self.attrs.get(&cnid).cloned().unwrap_or_default();
        let mut out = Vec::with_capacity(list.len());
        for (name, rec, more) in list {
            let value = match rec {
                AttrRecord::Inline(v) => v,
                AttrRecord::Fork(fork) => {
                    let mut ext: Vec<ExtentDescriptor> = fork
                        .extents
                        .iter()
                        .copied()
                        .filter(|e| e.block_count > 0)
                        .collect();
                    for r in &more {
                        ext.extend(r.iter().copied().filter(|e| e.block_count > 0));
                    }
                    let mut v = vec![0u8; fork.logical_size as usize];
                    self.read_fork(&ext, 0, &mut v)?;
                    v
                }
                AttrRecord::Extents(_) => continue,
            };
            out.push((name, value));
        }
        Ok(out)
    }

    /// Names of the extended attributes of an entry.
    pub fn xattr_names(&self, cnid: u32) -> Vec<String> {
        self.attrs
            .get(&cnid)
            .map(|l| l.iter().map(|(n, _, _)| n.clone()).collect())
            .unwrap_or_default()
    }

    /// Target of a symbolic link.
    pub fn read_link(&mut self, entry: &Entry) -> Result<String> {
        if entry.kind() != EntryKind::Symlink {
            return Err(crate::error::invalid(format!(
                "{} is not a symlink",
                entry.name
            )));
        }
        let data = self.read_file(entry)?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    /// Reads the whole data fork of a file (decompressing decmpfs files).
    pub fn read_file(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let mut v = Vec::with_capacity(entry.data_size() as usize);
        self.copy_file(entry, &mut v)?;
        Ok(v)
    }

    /// Streams a file's data fork to `out`, following hard links and
    /// decompressing decmpfs-compressed files.
    pub fn copy_file<W: Write + ?Sized>(&mut self, entry: &Entry, out: &mut W) -> Result<u64> {
        let entry = self.resolve_link(entry)?.clone();
        let CatalogRecord::File(f) = &entry.record else {
            return Err(crate::error::invalid(format!(
                "{} is not a file",
                entry.name
            )));
        };
        if entry.is_compressed()
            && let Some(n) = self.decmpfs(&entry, out)?
        {
            return Ok(n);
        }
        self.stream_fork(f.file_id, 0, &f.data_fork, out)
    }

    /// Reads a file's resource fork.
    pub fn read_resource_fork(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let entry = self.resolve_link(entry)?.clone();
        match &entry.record {
            CatalogRecord::File(f) => self.read_fork_all(f.file_id, 0xFF, &f.resource_fork),
            _ => Ok(Vec::new()),
        }
    }

    fn decmpfs<W: Write + ?Sized>(&mut self, entry: &Entry, out: &mut W) -> Result<Option<u64>> {
        let xattr = self
            .xattrs(entry.cnid)?
            .into_iter()
            .find(|(n, _)| n == "com.apple.decmpfs")
            .map(|(_, v)| v);
        let Some(x) = xattr else { return Ok(None) };
        if x.len() < 16 || &x[0..4] != b"fpmc" {
            return Err(corrupt("bad decmpfs header"));
        }
        let ty = u32::from_le_bytes(x[4..8].try_into().unwrap());
        let size = u64::from_le_bytes(x[8..16].try_into().unwrap());
        let inline = &x[16..];
        let data = match ty {
            1 => inline.to_vec(),
            3 => decmpfs_block(inline, Codec::Zlib, size as usize)?,
            7 => decmpfs_block(inline, Codec::Lzvn, size as usize)?,
            11 => decmpfs_block(inline, Codec::Lzfse, size as usize)?,
            4 => {
                let rsrc = self.read_resource_fork(entry)?;
                decmpfs_zlib_rsrc(&rsrc, size)?
            }
            8 | 12 => {
                let rsrc = self.read_resource_fork(entry)?;
                let codec = if ty == 8 { Codec::Lzvn } else { Codec::Lzfse };
                decmpfs_table_rsrc(&rsrc, codec, size)?
            }
            other => {
                return Err(crate::Error::Unsupported(format!(
                    "decmpfs compression type {other}"
                )));
            }
        };
        if data.len() as u64 != size {
            return Err(corrupt("decmpfs size mismatch"));
        }
        out.write_all(&data)?;
        Ok(Some(size))
    }
}

fn empty_tree() -> BTreeFile {
    let params = super::btree::TreeParams {
        node_size: 512,
        max_key_length: 10,
        key_compare_type: 0,
        attributes: bt_attr::BIG_KEYS,
    };
    BTreeFile::new(super::btree::build(&params, &[], 1, 512).expect("empty tree"))
        .expect("valid empty tree")
}

#[derive(Clone, Copy)]
enum Codec {
    Zlib,
    Lzvn,
    Lzfse,
}

const DECMPFS_BLOCK: usize = 65536;

fn decmpfs_block(data: &[u8], codec: Codec, expected: usize) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let raw_marker = match codec {
        Codec::Zlib => (data[0] & 0x0F) == 0x0F,
        Codec::Lzvn => data[0] == 0x06,
        Codec::Lzfse => data[0] == 0xFF,
    };
    if raw_marker {
        return Ok(data[1..].to_vec());
    }
    match codec {
        Codec::Zlib => {
            let mut out = Vec::with_capacity(expected);
            flate2::read::ZlibDecoder::new(data).read_to_end(&mut out)?;
            Ok(out)
        }
        Codec::Lzvn => {
            // Wrap the raw LZVN stream in an LZFSE "bvxn" block.
            let mut wrapped = Vec::with_capacity(data.len() + 16);
            wrapped.extend_from_slice(b"bvxn");
            wrapped.extend_from_slice(&(expected as u32).to_le_bytes());
            wrapped.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wrapped.extend_from_slice(data);
            wrapped.extend_from_slice(b"bvx$");
            crate::udif::compress::lzfse_decode(&wrapped, expected)
        }
        Codec::Lzfse => crate::udif::compress::lzfse_decode(data, expected),
    }
}

fn decmpfs_zlib_rsrc(rsrc: &[u8], size: u64) -> Result<Vec<u8>> {
    // Resource fork: 0x100-byte resource header, then a 'cmpf' resource
    // starting with a little-endian block table.
    let base = 0x104usize;
    let get_le = |off: usize| -> Result<u32> {
        rsrc.get(off..off + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(|| corrupt("truncated decmpfs resource fork"))
    };
    let count = get_le(base)? as usize;
    let mut out = Vec::with_capacity(size as usize);
    for i in 0..count {
        let off = get_le(base + 4 + i * 8)? as usize;
        let len = get_le(base + 8 + i * 8)? as usize;
        let block = rsrc
            .get(base + off..base + off + len)
            .ok_or_else(|| corrupt("decmpfs block out of range"))?;
        let want = (size as usize - out.len()).min(DECMPFS_BLOCK);
        out.extend(decmpfs_block(block, Codec::Zlib, want)?);
    }
    Ok(out)
}

fn decmpfs_table_rsrc(rsrc: &[u8], codec: Codec, size: u64) -> Result<Vec<u8>> {
    let get_le = |off: usize| -> Result<usize> {
        rsrc.get(off..off + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            .ok_or_else(|| corrupt("truncated decmpfs resource fork"))
    };
    let first = get_le(0)?;
    if first % 4 != 0 || first < 8 {
        return Err(corrupt("bad decmpfs offset table"));
    }
    let n = first / 4 - 1;
    let mut out = Vec::with_capacity(size as usize);
    for i in 0..n {
        let (s, e) = (get_le(i * 4)?, get_le(i * 4 + 4)?);
        let block = rsrc
            .get(s..e)
            .ok_or_else(|| corrupt("decmpfs block out of range"))?;
        let want = (size as usize - out.len()).min(DECMPFS_BLOCK);
        out.extend(decmpfs_block(block, codec, want)?);
    }
    Ok(out)
}
