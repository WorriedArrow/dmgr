//! Building HFS+ / HFSX volumes from an in-memory file tree.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use super::btree::{self, Record, TreeParams};
use super::format::*;
use super::time::to_hfs;
use super::unicode::{binary_compare, fast_unicode_compare, fold_key, to_hfs_name};
use crate::disk::{Source, VirtualDisk};
use crate::error::{Result, invalid};

/// Largest extended attribute stored inline in the attributes B-tree.
pub const MAX_INLINE_ATTR_SIZE: usize = 3802;

/// Identifies an entry added to a [`VolumeBuilder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(usize);

/// Metadata attached to each file system entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// Permission bits (`0o7777` mask); the file type bits are derived from the entry kind.
    pub permissions: u16,
    pub uid: u32,
    pub gid: u32,
    pub created: SystemTime,
    pub modified: SystemTime,
    /// Classic Mac OS Finder info: 16 bytes `FileInfo`/`FolderInfo` followed
    /// by 16 bytes `ExtendedFileInfo`/`ExtendedFolderInfo`.
    pub finder_info: [u8; 32],
    /// Extended attributes. `com.apple.FinderInfo` is merged into
    /// [`finder_info`](Self::finder_info) and `com.apple.ResourceFork` becomes
    /// the resource fork.
    pub xattrs: Vec<(String, Vec<u8>)>,
}

impl Metadata {
    /// Default metadata for a directory with the given timestamp.
    pub fn dir(time: SystemTime) -> Self {
        Self::with_permissions(0o755, time)
    }

    /// Default metadata for a regular file with the given timestamp.
    pub fn file(time: SystemTime) -> Self {
        Self::with_permissions(0o644, time)
    }

    pub fn with_permissions(permissions: u16, time: SystemTime) -> Self {
        Self {
            permissions,
            uid: 99,
            gid: 99,
            created: time,
            modified: time,
            finder_info: [0; 32],
            xattrs: Vec::new(),
        }
    }

    /// Finder flags (`finderFlags` in `FileInfo`/`FolderInfo`).
    pub fn finder_flags(&self) -> u16 {
        u16::from_be_bytes([self.finder_info[8], self.finder_info[9]])
    }

    pub fn set_finder_flags(&mut self, flags: u16) {
        self.finder_info[8..10].copy_from_slice(&flags.to_be_bytes());
    }

    /// Sets the classic file type and creator codes.
    pub fn set_type_creator(&mut self, file_type: [u8; 4], creator: [u8; 4]) {
        self.finder_info[0..4].copy_from_slice(&file_type);
        self.finder_info[4..8].copy_from_slice(&creator);
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Dir {
        children: Vec<usize>,
    },
    File {
        data: Option<Source>,
        rsrc: Option<Source>,
    },
    Symlink {
        target: String,
    },
}

#[derive(Clone, Debug)]
struct Entry {
    parent: usize,
    name: String,
    hfs_name: Vec<u16>,
    cnid: u32,
    kind: Kind,
    meta: Metadata,
}

/// Options controlling the generated volume.
#[derive(Clone, Debug)]
pub struct VolumeOptions {
    /// Volume name (shown in Finder and used as the mount point name).
    pub name: String,
    /// Create a case-sensitive HFSX volume instead of case-insensitive HFS+.
    pub case_sensitive: bool,
    /// Allocation block size in bytes (power of two, 512..=65536).
    pub block_size: u32,
    /// Volume creation/modification time.
    pub created: SystemTime,
    /// 64-bit volume identifier stored in the Finder info (used for the volume UUID).
    pub volume_id: u64,
    /// Ask Finder to open a window on the root folder when mounted.
    pub open_folder: bool,
    /// Exact volume size in bytes; computed from the contents when `None`.
    pub size: Option<u64>,
    /// Free space to reserve beyond the contents when `size` is `None`.
    pub free_space: Option<u64>,
}

impl VolumeOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            case_sensitive: false,
            block_size: 4096,
            created: SystemTime::now(),
            volume_id: 0,
            open_folder: false,
            size: None,
            free_space: None,
        }
    }
}

/// The result of [`VolumeBuilder::build`].
#[derive(Debug, Clone)]
pub struct BuiltVolume {
    /// The volume's bytes.
    pub disk: VirtualDisk,
    /// The primary volume header that was written.
    pub header: VolumeHeader,
}

/// Incrementally describes a volume's file tree, then lays it out.
#[derive(Debug, Clone)]
pub struct VolumeBuilder {
    opts: VolumeOptions,
    entries: Vec<Entry>,
    lookup: HashMap<(usize, Vec<u16>), usize>,
    next_cnid: u32,
}

const CATALOG_NODE_SIZE: usize = 8192;
const EXTENTS_NODE_SIZE: usize = 4096;
const ATTRIBUTES_NODE_SIZE: usize = 8192;
const SYMLINK_TYPE: [u8; 4] = *b"slnk";
const SYMLINK_CREATOR: [u8; 4] = *b"rhap";

impl VolumeBuilder {
    /// Starts a volume containing only the root folder.
    pub fn new(opts: VolumeOptions) -> Result<Self> {
        if !opts.block_size.is_power_of_two() || !(512..=65536).contains(&opts.block_size) {
            return Err(invalid(format!("invalid block size {}", opts.block_size)));
        }
        let hfs_name = to_hfs_name(&opts.name)?;
        let root = Entry {
            parent: 0,
            name: opts.name.clone(),
            hfs_name,
            cnid: cnid::ROOT_FOLDER,
            kind: Kind::Dir {
                children: Vec::new(),
            },
            meta: Metadata::dir(opts.created),
        };
        Ok(Self {
            opts,
            entries: vec![root],
            lookup: HashMap::new(),
            next_cnid: cnid::FIRST_USER,
        })
    }

    pub fn options(&self) -> &VolumeOptions {
        &self.opts
    }

    /// The root folder.
    pub fn root(&self) -> EntryId {
        EntryId(0)
    }

    fn compare(&self, a: &[u16], b: &[u16]) -> Ordering {
        if self.opts.case_sensitive {
            binary_compare(a, b)
        } else {
            fast_unicode_compare(a, b)
        }
    }

    fn name_key(&self, hfs_name: &[u16]) -> Vec<u16> {
        if self.opts.case_sensitive {
            hfs_name.to_vec()
        } else {
            fold_key(hfs_name)
        }
    }

    /// Finds a child of `parent` by name, using the volume's case sensitivity.
    pub fn lookup(&self, parent: EntryId, name: &str) -> Option<EntryId> {
        let hfs = to_hfs_name(name).ok()?;
        self.lookup
            .get(&(parent.0, self.name_key(&hfs)))
            .map(|&i| EntryId(i))
    }

    /// Resolves a `/`-separated path relative to the root.
    pub fn lookup_path(&self, path: &str) -> Option<EntryId> {
        let mut cur = self.root();
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = self.lookup(cur, comp)?;
        }
        Some(cur)
    }

    fn add(&mut self, parent: EntryId, name: &str, kind: Kind, meta: Metadata) -> Result<EntryId> {
        if name == "." || name == ".." || name.contains('/') {
            return Err(invalid(format!("invalid file name {name:?}")));
        }
        if !matches!(
            self.entries.get(parent.0).map(|e| &e.kind),
            Some(Kind::Dir { .. })
        ) {
            return Err(invalid(format!("parent of {name:?} is not a directory")));
        }
        let hfs_name = to_hfs_name(name)?;
        let key = (parent.0, self.name_key(&hfs_name));
        if let Some(&existing) = self.lookup.get(&key) {
            return Err(invalid(format!(
                "{:?} conflicts with existing entry {:?}{}",
                self.path(EntryId(parent.0)) + "/" + name,
                self.path(EntryId(existing)),
                if self.opts.case_sensitive {
                    ""
                } else {
                    " (HFS+ names are case-insensitive; use a case-sensitive file system to keep both)"
                }
            )));
        }
        let cnid = self.next_cnid;
        self.next_cnid = cnid
            .checked_add(1)
            .ok_or_else(|| invalid("too many files"))?;
        let idx = self.entries.len();
        self.entries.push(Entry {
            parent: parent.0,
            name: name.to_owned(),
            hfs_name,
            cnid,
            kind,
            meta,
        });
        self.lookup.insert(key, idx);
        if let Kind::Dir { children } = &mut self.entries[parent.0].kind {
            children.push(idx);
        }
        Ok(EntryId(idx))
    }

    /// Adds a directory.
    pub fn add_dir(&mut self, parent: EntryId, name: &str, meta: Metadata) -> Result<EntryId> {
        self.add(
            parent,
            name,
            Kind::Dir {
                children: Vec::new(),
            },
            meta,
        )
    }

    /// Adds a regular file whose data fork comes from `data`.
    pub fn add_file(
        &mut self,
        parent: EntryId,
        name: &str,
        data: impl Into<Source>,
        meta: Metadata,
    ) -> Result<EntryId> {
        let data = data.into();
        self.add(
            parent,
            name,
            Kind::File {
                data: (!data.is_empty()).then_some(data),
                rsrc: None,
            },
            meta,
        )
    }

    /// Adds a symbolic link.
    pub fn add_symlink(
        &mut self,
        parent: EntryId,
        name: &str,
        target: &str,
        meta: Metadata,
    ) -> Result<EntryId> {
        if target.is_empty() {
            return Err(invalid(format!("symlink {name:?} has an empty target")));
        }
        self.add(
            parent,
            name,
            Kind::Symlink {
                target: target.to_owned(),
            },
            meta,
        )
    }

    /// Replaces the data fork of an existing regular file.
    pub fn set_file_data(&mut self, id: EntryId, data: impl Into<Source>) -> Result<()> {
        let data = data.into();
        match &mut self.entries[id.0].kind {
            Kind::File { data: d, .. } => {
                *d = (!data.is_empty()).then_some(data);
                Ok(())
            }
            _ => Err(invalid("not a regular file")),
        }
    }

    /// Sets the resource fork of a regular file.
    pub fn set_resource_fork(&mut self, id: EntryId, rsrc: impl Into<Source>) -> Result<()> {
        let rsrc = rsrc.into();
        match &mut self.entries[id.0].kind {
            Kind::File { rsrc: r, .. } => {
                *r = (!rsrc.is_empty()).then_some(rsrc);
                Ok(())
            }
            _ => Err(invalid("only regular files have resource forks")),
        }
    }

    pub fn metadata(&self, id: EntryId) -> &Metadata {
        &self.entries[id.0].meta
    }

    pub fn metadata_mut(&mut self, id: EntryId) -> &mut Metadata {
        &mut self.entries[id.0].meta
    }

    /// The catalog node ID assigned to an entry.
    pub fn cnid(&self, id: EntryId) -> u32 {
        self.entries[id.0].cnid
    }

    pub fn is_dir(&self, id: EntryId) -> bool {
        matches!(self.entries[id.0].kind, Kind::Dir { .. })
    }

    /// The POSIX name of an entry (the volume name for the root).
    pub fn name(&self, id: EntryId) -> &str {
        &self.entries[id.0].name
    }

    /// Parent of an entry (the root is its own parent).
    pub fn parent(&self, id: EntryId) -> EntryId {
        EntryId(self.entries[id.0].parent)
    }

    /// Path of an entry relative to the root, starting with `/`.
    pub fn path(&self, id: EntryId) -> String {
        let mut parts = Vec::new();
        let mut cur = id.0;
        while cur != 0 {
            parts.push(self.entries[cur].name.as_str());
            cur = self.entries[cur].parent;
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    /// Children of a directory in insertion order.
    pub fn children(&self, id: EntryId) -> Vec<EntryId> {
        match &self.entries[id.0].kind {
            Kind::Dir { children } => children.iter().map(|&i| EntryId(i)).collect(),
            _ => Vec::new(),
        }
    }

    /// Lays out the volume and produces its bytes.
    pub fn build(self) -> Result<BuiltVolume> {
        Layout::new(self)?.finish()
    }
}

/// A fork waiting for allocation.
struct PendingFork {
    source: Source,
    start: u32,
    blocks: u32,
}

impl PendingFork {
    fn new(source: Source) -> Self {
        Self {
            source,
            start: 0,
            blocks: 0,
        }
    }
}

#[derive(Default)]
struct EntryForks {
    data: Option<usize>,
    rsrc: Option<usize>,
    flags: u16,
}

/// An extended attribute awaiting layout: inline data or an index into the forks.
type PendingAttr = (u32, Vec<u16>, Result<Vec<u8>, usize>);

struct Layout {
    b: VolumeBuilder,
    forks: Vec<PendingFork>,
    entry_forks: Vec<EntryForks>,
    /// (file id, name, inline data or fork index)
    attrs: Vec<PendingAttr>,
}

impl Layout {
    fn new(mut b: VolumeBuilder) -> Result<Self> {
        let mut forks = Vec::new();
        let mut entry_forks: Vec<EntryForks> = Vec::with_capacity(b.entries.len());
        let mut attrs = Vec::new();

        for entry in &mut b.entries {
            let mut ef = EntryForks::default();
            // Special extended attributes.
            let xattrs = std::mem::take(&mut entry.meta.xattrs);
            for (name, value) in xattrs {
                match name.as_str() {
                    "com.apple.FinderInfo" if value.len() == 32 => {
                        entry.meta.finder_info.copy_from_slice(&value);
                    }
                    "com.apple.ResourceFork" => {
                        if let Kind::File { rsrc, .. } = &mut entry.kind {
                            if !value.is_empty() {
                                *rsrc = Some(value.into());
                            }
                        }
                    }
                    _ => {
                        let uname: Vec<u16> = name.encode_utf16().collect();
                        if uname.is_empty() || uname.len() > 127 {
                            return Err(invalid(format!(
                                "extended attribute name {name:?} on {:?} must be 1..=127 UTF-16 units",
                                entry.name
                            )));
                        }
                        if name == "com.apple.system.Security" {
                            ef.flags |= rec_flags::HAS_SECURITY;
                        }
                        ef.flags |= rec_flags::HAS_ATTRIBUTES;
                        let payload = if value.len() <= MAX_INLINE_ATTR_SIZE {
                            Ok(value)
                        } else {
                            forks.push(PendingFork::new(value.into()));
                            Err(forks.len() - 1)
                        };
                        attrs.push((entry.cnid, uname, payload));
                    }
                }
            }
            match &entry.kind {
                Kind::File { data, rsrc } => {
                    if let Some(d) = data {
                        forks.push(PendingFork::new(d.clone()));
                        ef.data = Some(forks.len() - 1);
                    }
                    if let Some(r) = rsrc {
                        forks.push(PendingFork::new(r.clone()));
                        ef.rsrc = Some(forks.len() - 1);
                    }
                }
                Kind::Symlink { target } => {
                    forks.push(PendingFork::new(target.as_bytes().to_vec().into()));
                    ef.data = Some(forks.len() - 1);
                }
                Kind::Dir { .. } => {}
            }
            entry_forks.push(ef);
        }
        attrs.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        for w in attrs.windows(2) {
            if w[0].0 == w[1].0 && w[0].1 == w[1].1 {
                return Err(invalid("duplicate extended attribute"));
            }
        }
        Ok(Self {
            b,
            forks,
            entry_forks,
            attrs,
        })
    }

    fn catalog_params(&self) -> TreeParams {
        TreeParams {
            node_size: CATALOG_NODE_SIZE,
            max_key_length: 516,
            key_compare_type: if self.b.opts.case_sensitive {
                key_compare::BINARY
            } else {
                key_compare::CASE_FOLDING
            },
            attributes: bt_attr::BIG_KEYS | bt_attr::VARIABLE_INDEX_KEYS,
        }
    }

    fn extents_params() -> TreeParams {
        TreeParams {
            node_size: EXTENTS_NODE_SIZE,
            max_key_length: ExtentKey::KEY_LENGTH,
            key_compare_type: 0,
            attributes: bt_attr::BIG_KEYS,
        }
    }

    fn attributes_params() -> TreeParams {
        TreeParams {
            node_size: ATTRIBUTES_NODE_SIZE,
            max_key_length: AttrKey::MAX_KEY_LENGTH,
            key_compare_type: 0,
            attributes: bt_attr::BIG_KEYS | bt_attr::VARIABLE_INDEX_KEYS,
        }
    }

    fn fork_data(&self, idx: Option<usize>) -> ForkData {
        let bs = self.b.opts.block_size;
        match idx {
            None => ForkData::default(),
            Some(i) => {
                let f = &self.forks[i];
                ForkData::contiguous(f.source.len(), f.start, f.blocks, bs)
            }
        }
    }

    fn catalog_records(&self) -> Vec<Record> {
        let b = &self.b;
        let mut recs: Vec<(CatalogKey, CatalogRecord)> = Vec::with_capacity(b.entries.len() * 2);
        for (i, e) in b.entries.iter().enumerate() {
            let parent_cnid = if i == 0 {
                cnid::ROOT_PARENT
            } else {
                b.entries[e.parent].cnid
            };
            let m = &e.meta;
            let ef = &self.entry_forks[i];
            let created = to_hfs(m.created);
            let modified = to_hfs(m.modified);
            let user_info: [u8; 16] = m.finder_info[..16].try_into().expect("16 bytes");
            let finder_info: [u8; 16] = m.finder_info[16..].try_into().expect("16 bytes");
            let key = CatalogKey {
                parent_id: parent_cnid,
                name: e.hfs_name.clone(),
            };
            let thread_key = CatalogKey {
                parent_id: e.cnid,
                name: Vec::new(),
            };
            let thread = ThreadRecord {
                parent_id: parent_cnid,
                name: e.hfs_name.clone(),
            };
            let mut perms = BsdInfo {
                owner_id: m.uid,
                group_id: m.gid,
                admin_flags: 0,
                owner_flags: 0,
                file_mode: m.permissions & 0o7777,
                special: 0,
            };
            match &e.kind {
                Kind::Dir { children } => {
                    perms.file_mode |= mode::S_IFDIR;
                    let subdirs = children
                        .iter()
                        .filter(|&&c| matches!(b.entries[c].kind, Kind::Dir { .. }))
                        .count() as u32;
                    recs.push((
                        key,
                        CatalogRecord::Folder(FolderRecord {
                            flags: ef.flags | rec_flags::HAS_FOLDER_COUNT,
                            valence: children.len() as u32,
                            folder_id: e.cnid,
                            create_date: created,
                            content_mod_date: modified,
                            attribute_mod_date: modified,
                            access_date: modified,
                            backup_date: 0,
                            permissions: perms,
                            user_info,
                            finder_info,
                            text_encoding: 0,
                            folder_count: subdirs,
                        }),
                    ));
                    recs.push((thread_key, CatalogRecord::FolderThread(thread)));
                }
                Kind::File { .. } | Kind::Symlink { .. } => {
                    let mut user_info = user_info;
                    if matches!(e.kind, Kind::Symlink { .. }) {
                        perms.file_mode = mode::S_IFLNK | 0o755;
                        user_info[0..4].copy_from_slice(&SYMLINK_TYPE);
                        user_info[4..8].copy_from_slice(&SYMLINK_CREATOR);
                    } else {
                        perms.file_mode |= mode::S_IFREG;
                    }
                    recs.push((
                        key,
                        CatalogRecord::File(FileRecord {
                            flags: ef.flags | rec_flags::THREAD_EXISTS,
                            file_id: e.cnid,
                            create_date: created,
                            content_mod_date: modified,
                            attribute_mod_date: modified,
                            access_date: modified,
                            backup_date: 0,
                            permissions: perms,
                            user_info,
                            finder_info,
                            text_encoding: 0,
                            data_fork: self.fork_data(ef.data),
                            resource_fork: self.fork_data(ef.rsrc),
                        }),
                    ));
                    recs.push((thread_key, CatalogRecord::FileThread(thread)));
                }
            }
        }
        recs.sort_by(|(a, _), (b2, _)| {
            a.parent_id
                .cmp(&b2.parent_id)
                .then_with(|| b.compare(&a.name, &b2.name))
        });
        recs.into_iter()
            .map(|(k, r)| (k.encode(), r.encode()))
            .collect()
    }

    fn attribute_records(&self) -> Vec<Record> {
        self.attrs
            .iter()
            .map(|(file_id, name, payload)| {
                let key = AttrKey {
                    file_id: *file_id,
                    start_block: 0,
                    name: name.clone(),
                };
                let rec = match payload {
                    Ok(data) => AttrRecord::Inline(data.clone()),
                    Err(i) => AttrRecord::Fork(self.fork_data(Some(*i))),
                };
                (key.encode(), rec.encode())
            })
            .collect()
    }

    fn finish(mut self) -> Result<BuiltVolume> {
        let opts = self.b.opts.clone();
        let bs = u64::from(opts.block_size);
        let blocks_for = |bytes: u64| -> u64 { bytes.div_ceil(bs) };

        // Size the B-trees (record sizes do not depend on block placement).
        let cat_p = self.catalog_params();
        let ext_p = Self::extents_params();
        let attr_p = Self::attributes_params();
        let cat_records = self.catalog_records();
        let attr_records = self.attribute_records();
        let round_nodes = |p: &TreeParams, n: u32| -> u32 {
            // Make the file a whole number of allocation blocks.
            let per_block = (bs as usize / p.node_size).max(1) as u32;
            let bytes = u64::from(n) * p.node_size as u64;
            let blocks = blocks_for(bytes);
            let n2 = (blocks * bs / p.node_size as u64) as u32;
            n2.max(n).next_multiple_of(per_block)
        };
        let cat_used = btree::nodes_required(&cat_p, &cat_records)?;
        let cat_nodes = round_nodes(
            &cat_p,
            btree::total_nodes_for(&cat_p, cat_used, (cat_used / 4).max(8)),
        );
        let ext_nodes = round_nodes(&ext_p, btree::total_nodes_for(&ext_p, 1, 7));
        let attr_used = btree::nodes_required(&attr_p, &attr_records)?;
        let attr_nodes = round_nodes(
            &attr_p,
            btree::total_nodes_for(&attr_p, attr_used, (attr_used / 4).max(4)),
        );
        let tree_blocks = |p: &TreeParams, n: u32| blocks_for(u64::from(n) * p.node_size as u64);
        let cat_blocks = tree_blocks(&cat_p, cat_nodes);
        let ext_blocks = tree_blocks(&ext_p, ext_nodes);
        let attr_blocks = tree_blocks(&attr_p, attr_nodes);

        // Blocks reserved at the head (boot blocks + volume header) and tail
        // (alternate volume header + reserved sector).
        let head = blocks_for(VOLUME_HEADER_OFFSET + VolumeHeader::SIZE as u64);
        let tail = blocks_for(1024);
        let data_blocks: u64 = self.forks.iter().map(|f| blocks_for(f.source.len())).sum();
        let fixed = head + ext_blocks + cat_blocks + attr_blocks + data_blocks + tail;

        let bitmap_blocks_for = |total: u64| blocks_for(total.div_ceil(8));
        let total_blocks = match opts.size {
            Some(size) => {
                let total = size / bs;
                let needed = fixed + bitmap_blocks_for(total);
                if total < needed {
                    return Err(invalid(format!(
                        "volume size {size} is too small; at least {} bytes are needed",
                        needed * bs
                    )));
                }
                total
            }
            None => {
                let used_estimate = fixed + bitmap_blocks_for(fixed * 2);
                let free = match opts.free_space {
                    Some(f) => blocks_for(f),
                    None => (used_estimate / 50).max(blocks_for(256 * 1024)),
                };
                let min_total = blocks_for(1024 * 1024);
                let mut total = (fixed + free).max(min_total);
                loop {
                    let t = (fixed + bitmap_blocks_for(total) + free).max(min_total);
                    if t == total {
                        break total;
                    }
                    total = t;
                }
            }
        };
        if total_blocks > u64::from(u32::MAX) {
            return Err(invalid("volume too large for its block size"));
        }
        let bitmap_blocks = bitmap_blocks_for(total_blocks);

        // Place everything contiguously after the header.
        let mut next = head;
        let alloc_start = next;
        next += bitmap_blocks;
        let ext_start = next;
        next += ext_blocks;
        let cat_start = next;
        next += cat_blocks;
        let attr_start = next;
        next += attr_blocks;
        for f in &mut self.forks {
            f.blocks = blocks_for(f.source.len()) as u32;
            if f.blocks > 0 {
                f.start = next as u32;
                next += u64::from(f.blocks);
            }
        }
        let data_end = next;
        let tail_start = total_blocks - tail;
        debug_assert!(data_end <= tail_start);

        // Rebuild records now that fork extents are known.
        let cat_records = self.catalog_records();
        let attr_records = self.attribute_records();
        let cat_clump = (cat_blocks * bs) as u32;
        let catalog = btree::build(&cat_p, &cat_records, cat_nodes, cat_clump)?;
        let extents = btree::build(&ext_p, &[], ext_nodes, (ext_blocks * bs) as u32)?;
        let attributes = btree::build(
            &attr_p,
            &attr_records,
            attr_nodes,
            (attr_blocks * bs) as u32,
        )?;

        // Allocation bitmap.
        let mut bitmap = vec![0u8; (bitmap_blocks * bs) as usize];
        let mut mark = |range: std::ops::Range<u64>| {
            for blk in range {
                bitmap[(blk / 8) as usize] |= 0x80 >> (blk % 8);
            }
        };
        mark(0..data_end);
        mark(tail_start..total_blocks);
        let used_blocks = data_end + tail;

        let b = &self.b;
        let file_count = b
            .entries
            .iter()
            .filter(|e| !matches!(e.kind, Kind::Dir { .. }))
            .count() as u32;
        let folder_count = (b.entries.len() as u32) - file_count - 1;
        let now = to_hfs(opts.created);
        let fork = |start: u64, blocks: u64, clump: u32| {
            ForkData::contiguous(blocks * bs, start as u32, blocks as u32, clump)
        };
        let header = VolumeHeader {
            signature: if opts.case_sensitive {
                SIG_HFSX
            } else {
                SIG_HFSPLUS
            },
            version: if opts.case_sensitive { 5 } else { 4 },
            attributes: vol_attr::UNMOUNTED,
            last_mounted_version: u32::from_be_bytes(*b"10.0"),
            journal_info_block: 0,
            create_date: now,
            modify_date: now,
            backup_date: 0,
            checked_date: now,
            file_count,
            folder_count,
            block_size: opts.block_size,
            total_blocks: total_blocks as u32,
            free_blocks: (total_blocks - used_blocks) as u32,
            next_allocation: data_end as u32,
            rsrc_clump_size: 65536,
            data_clump_size: 65536,
            next_catalog_id: b.next_cnid,
            write_count: 1,
            encodings_bitmap: 1,
            finder_info: [
                0,
                0,
                if opts.open_folder {
                    cnid::ROOT_FOLDER
                } else {
                    0
                },
                0,
                0,
                0,
                (opts.volume_id >> 32) as u32,
                opts.volume_id as u32,
            ],
            allocation_file: fork(alloc_start, bitmap_blocks, opts.block_size),
            extents_file: fork(ext_start, ext_blocks, (ext_blocks * bs) as u32),
            catalog_file: fork(cat_start, cat_blocks, cat_clump),
            attributes_file: fork(attr_start, attr_blocks, (attr_blocks * bs) as u32),
            startup_file: ForkData::default(),
        };

        let size = total_blocks * bs;
        let mut disk = VirtualDisk::new(size);
        let vh = header.encode();
        disk.put(VOLUME_HEADER_OFFSET, vh.clone())?;
        disk.put(alloc_start * bs, bitmap)?;
        disk.put(ext_start * bs, extents)?;
        disk.put(cat_start * bs, catalog)?;
        disk.put(attr_start * bs, attributes)?;
        for f in &self.forks {
            if f.blocks > 0 {
                disk.put(u64::from(f.start) * bs, f.source.clone())?;
            }
        }
        disk.put(size - 1024, vh)?;
        Ok(BuiltVolume { disk, header })
    }
}

/// Convenience: an `Arc<[u8]>`-backed source.
pub fn bytes(data: impl Into<Arc<[u8]>>) -> Source {
    Source::Bytes(data.into())
}
