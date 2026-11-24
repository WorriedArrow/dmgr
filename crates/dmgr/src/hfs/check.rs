//! A consistency checker for HFS+ volumes (a small `fsck_hfs`).
//!
//! It verifies B-tree structure (ordering, linkage, index keys, node
//! bitmaps), catalog invariants (threads, valences, counts) and the
//! allocation bitmap. It is used to validate images on hosts without macOS.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::btree::BTreeFile;
use super::format::*;
use super::reader::Volume;
use super::unicode::{binary_compare, fast_unicode_compare};
use crate::error::Result;
use crate::io::ReadAt;

type KeyCmp<'a> = &'a dyn Fn(&[u8], &[u8]) -> Result<Ordering>;

fn catalog_cmp(hfsx: bool) -> impl Fn(&[u8], &[u8]) -> Result<Ordering> {
    move |a, b| {
        let ka = CatalogKey::decode_body(&a[2..])?;
        let kb = CatalogKey::decode_body(&b[2..])?;
        Ok(ka.parent_id.cmp(&kb.parent_id).then_with(|| {
            if hfsx {
                binary_compare(&ka.name, &kb.name)
            } else {
                fast_unicode_compare(&ka.name, &kb.name)
            }
        }))
    }
}

fn extents_cmp(a: &[u8], b: &[u8]) -> Result<Ordering> {
    Ok(ExtentKey::decode_body(&a[2..])?
        .sort_key()
        .cmp(&ExtentKey::decode_body(&b[2..])?.sort_key()))
}

fn attr_cmp(a: &[u8], b: &[u8]) -> Result<Ordering> {
    let ka = AttrKey::decode_body(&a[2..])?;
    let kb = AttrKey::decode_body(&b[2..])?;
    Ok((ka.file_id, &ka.name, ka.start_block).cmp(&(kb.file_id, &kb.name, kb.start_block)))
}

/// Checks a B-tree's structure; returns problems found.
pub fn check_btree(name: &str, tree: &BTreeFile, cmp: KeyCmp<'_>) -> Result<Vec<String>> {
    let mut errs = Vec::new();
    let h = *tree.header();
    let mut used: HashSet<u32> = HashSet::new();
    used.insert(0);
    // Map nodes.
    let mut m = tree.node(0)?.desc.flink;
    while m != 0 {
        if !used.insert(m) {
            errs.push(format!("{name}: map node chain loops"));
            break;
        }
        let node = tree.node(m)?;
        if node.desc.kind != node_kind::MAP {
            errs.push(format!("{name}: node {m} in map chain is not a map node"));
            break;
        }
        m = node.desc.flink;
    }

    if h.root_node == 0 {
        if h.tree_depth != 0
            || h.leaf_records != 0
            || h.first_leaf_node != 0
            || h.last_leaf_node != 0
        {
            errs.push(format!("{name}: empty tree has non-zero header fields"));
        }
    } else {
        // Walk level by level from the root.
        let mut level = vec![(h.root_node, None::<Vec<u8>>)];
        let mut height = h.tree_depth;
        let mut leaf_count = 0u32;
        let mut leaves: Vec<u32> = Vec::new();
        while !level.is_empty() {
            let mut next = Vec::new();
            let mut prev_key: Option<Vec<u8>> = None;
            let mut prev_node = 0u32;
            for (i, (n, expect_first)) in level.iter().enumerate() {
                if !used.insert(*n) {
                    errs.push(format!("{name}: node {n} referenced twice"));
                    continue;
                }
                let node = tree.node(*n)?;
                let is_leaf = height == 1;
                let want_kind = if is_leaf {
                    node_kind::LEAF
                } else {
                    node_kind::INDEX
                };
                if node.desc.kind != want_kind {
                    errs.push(format!(
                        "{name}: node {n} has kind {} at height {height}",
                        node.desc.kind
                    ));
                    continue;
                }
                if u16::from(node.desc.height) != height {
                    errs.push(format!(
                        "{name}: node {n} height {} != {height}",
                        node.desc.height
                    ));
                }
                if node.desc.blink != prev_node {
                    errs.push(format!(
                        "{name}: node {n} blink {} != {prev_node}",
                        node.desc.blink
                    ));
                }
                let expected_flink = level.get(i + 1).map_or(0, |(x, _)| *x);
                if node.desc.flink != expected_flink {
                    errs.push(format!(
                        "{name}: node {n} flink {} != {expected_flink}",
                        node.desc.flink
                    ));
                }
                if node.records.is_empty() {
                    errs.push(format!("{name}: node {n} is empty"));
                    continue;
                }
                for (ri, rec) in node.records.iter().enumerate() {
                    let (key, data) = tree.split_record(rec, !is_leaf)?;
                    if ri == 0 {
                        if let Some(first) = expect_first {
                            if cmp(first, key)? != Ordering::Equal {
                                errs.push(format!(
                                    "{name}: index key for node {n} differs from its first key"
                                ));
                            }
                        }
                    }
                    if let Some(pk) = &prev_key {
                        if cmp(pk, key)? != Ordering::Less {
                            errs.push(format!("{name}: keys out of order in node {n} record {ri}"));
                        }
                    }
                    prev_key = Some(key.to_vec());
                    if is_leaf {
                        leaf_count += 1;
                    } else {
                        if data.len() < 4 {
                            errs.push(format!(
                                "{name}: index record without child pointer in node {n}"
                            ));
                            continue;
                        }
                        let child = u32::from_be_bytes(data[..4].try_into().unwrap());
                        next.push((child, Some(key.to_vec())));
                    }
                }
                if is_leaf {
                    leaves.push(*n);
                }
                prev_node = *n;
            }
            level = next;
            if height == 0 {
                break;
            }
            height -= 1;
        }
        if height != 0 {
            errs.push(format!(
                "{name}: tree depth {} does not match structure",
                h.tree_depth
            ));
        }
        if leaf_count != h.leaf_records {
            errs.push(format!(
                "{name}: header says {} leaf records, found {leaf_count}",
                h.leaf_records
            ));
        }
        if leaves.first().copied() != Some(h.first_leaf_node) {
            errs.push(format!("{name}: wrong first leaf node"));
        }
        if leaves.last().copied() != Some(h.last_leaf_node) {
            errs.push(format!("{name}: wrong last leaf node"));
        }
    }

    // Node bitmap must match exactly the nodes in use.
    let mut in_use = 0u32;
    for n in 0..h.total_nodes {
        let marked = tree.node_in_use(n)?;
        if marked {
            in_use += 1;
        }
        if used.contains(&n) && !marked {
            errs.push(format!("{name}: node {n} in use but free in bitmap"));
        }
        if !used.contains(&n) && marked {
            errs.push(format!("{name}: node {n} marked in use but unreachable"));
        }
    }
    if h.total_nodes - in_use != h.free_nodes {
        errs.push(format!(
            "{name}: header free node count {} != {}",
            h.free_nodes,
            h.total_nodes - in_use
        ));
    }
    Ok(errs)
}

/// Runs all checks against an open volume; returns a list of problems.
pub fn check<R: ReadAt>(vol: &mut Volume<R>, dev_size: u64) -> Result<Vec<String>> {
    let mut errs = Vec::new();
    let vh = vol.header().clone();
    let bs = u64::from(vh.block_size);
    let hfsx = vh.is_hfsx();

    if u64::from(vh.total_blocks) * bs > dev_size {
        errs.push("volume is larger than its partition".to_string());
    }

    // B-trees.
    let cat_cmp = catalog_cmp(hfsx);
    errs.extend(check_btree("catalog", vol.catalog(), &cat_cmp)?);
    let want_cmp = if hfsx {
        key_compare::BINARY
    } else {
        key_compare::CASE_FOLDING
    };
    let kct = vol.catalog().header().key_compare_type;
    if hfsx && kct != want_cmp {
        errs.push(format!("catalog key compare type {kct:#x} on HFSX volume"));
    }
    if let Some(attrs) = vol.attributes_tree() {
        errs.extend(check_btree("attributes", attrs, &attr_cmp)?);
    }
    if let Some(ext) = vol.extents_tree() {
        errs.extend(check_btree("extents", ext, &extents_cmp)?);
    }

    // Catalog semantics.
    let mut max_cnid = 0u32;
    let mut files = 0u32;
    let mut folders = 0u32;
    let mut valence: HashMap<u32, (u32, u32)> = HashMap::new();
    let entries: Vec<_> = vol.entries().cloned().collect();
    for e in &entries {
        max_cnid = max_cnid.max(e.cnid);
        let v = valence.entry(e.parent).or_default();
        v.0 += 1;
        if e.is_dir() {
            v.1 += 1;
            if e.cnid != cnid::ROOT_FOLDER {
                folders += 1;
            }
        } else {
            files += 1;
        }
        match vol.threads().get(&e.cnid) {
            None => {
                let needs = match &e.record {
                    CatalogRecord::File(f) => f.flags & rec_flags::THREAD_EXISTS != 0,
                    _ => true,
                };
                if needs {
                    errs.push(format!("missing thread record for {}", vol.path_of(e.cnid)));
                }
            }
            Some((is_folder, t)) => {
                if *is_folder != e.is_dir() {
                    errs.push(format!("thread type mismatch for {}", vol.path_of(e.cnid)));
                }
                if t.parent_id != e.parent || t.name != e.hfs_name {
                    errs.push(format!(
                        "thread record mismatch for {}",
                        vol.path_of(e.cnid)
                    ));
                }
            }
        }
        let has_attrs = !vol.xattr_names(e.cnid).is_empty();
        let flags = match &e.record {
            CatalogRecord::Folder(f) => f.flags,
            CatalogRecord::File(f) => f.flags,
            _ => 0,
        };
        if has_attrs != (flags & rec_flags::HAS_ATTRIBUTES != 0) {
            errs.push(format!(
                "attributes flag mismatch for {}",
                vol.path_of(e.cnid)
            ));
        }
    }
    for thread_cnid in vol.threads().keys() {
        if vol.entry(*thread_cnid).is_none() {
            errs.push(format!("orphan thread record for CNID {thread_cnid}"));
        }
    }
    for e in &entries {
        if let CatalogRecord::Folder(f) = &e.record {
            let (n, dirs) = valence.get(&e.cnid).copied().unwrap_or_default();
            if f.valence != n {
                errs.push(format!(
                    "valence of {} is {} but it has {n} entries",
                    vol.path_of(e.cnid),
                    f.valence
                ));
            }
            if f.flags & rec_flags::HAS_FOLDER_COUNT != 0 && f.folder_count != dirs {
                errs.push(format!(
                    "folder count of {} is {} but it has {dirs}",
                    vol.path_of(e.cnid),
                    f.folder_count
                ));
            }
        }
        if e.cnid != cnid::ROOT_FOLDER && vol.entry(e.parent).is_none_or(|p| !p.is_dir()) {
            errs.push(format!(
                "entry {} (CNID {}) has no parent folder",
                e.name, e.cnid
            ));
        }
    }
    if vh.file_count != files {
        errs.push(format!(
            "volume header file count {} != {files}",
            vh.file_count
        ));
    }
    if vh.folder_count != folders {
        errs.push(format!(
            "volume header folder count {} != {folders}",
            vh.folder_count
        ));
    }
    if vh.next_catalog_id <= max_cnid {
        errs.push(format!(
            "next catalog ID {} <= max CNID {max_cnid}",
            vh.next_catalog_id
        ));
    }

    // Allocation bitmap.
    let total = vh.total_blocks as usize;
    let mut owner: Vec<u32> = vec![0; total];
    let mut claim = |errs: &mut Vec<String>, who: u32, start: u32, count: u32| {
        for b in start as usize..start as usize + count as usize {
            match owner.get_mut(b) {
                None => {
                    errs.push(format!("CNID {who} uses block {b} beyond end of volume"));
                    return;
                }
                Some(o) if *o != 0 => {
                    errs.push(format!("block {b} allocated to both CNID {} and {who}", *o));
                    return;
                }
                Some(o) => *o = who,
            }
        }
    };
    let head = (VOLUME_HEADER_OFFSET + 512).div_ceil(bs) as u32;
    claim(&mut errs, u32::MAX, 0, head);
    let vol_bytes = u64::from(vh.total_blocks) * bs;
    let tail_start = ((vol_bytes - 1024) / bs) as u32;
    claim(
        &mut errs,
        u32::MAX - 1,
        tail_start,
        vh.total_blocks - tail_start,
    );
    let mut fork_check =
        |errs: &mut Vec<String>, vol: &Volume<R>, id: u32, ty: u8, fork: &ForkData| -> Result<()> {
            let ext = vol.fork_extents(id, ty, fork)?;
            let blocks: u64 = ext.iter().map(|e| u64::from(e.block_count)).sum();
            if blocks != u64::from(fork.total_blocks) {
                errs.push(format!("CNID {id} fork total blocks mismatch"));
            }
            if fork.logical_size > blocks * bs {
                errs.push(format!(
                    "CNID {id} fork logical size exceeds its allocation"
                ));
            }
            for e in ext {
                claim(errs, id, e.start_block, e.block_count);
            }
            Ok(())
        };
    fork_check(
        &mut errs,
        vol,
        cnid::ALLOCATION_FILE,
        0,
        &vh.allocation_file,
    )?;
    fork_check(&mut errs, vol, cnid::EXTENTS_FILE, 0, &vh.extents_file)?;
    fork_check(&mut errs, vol, cnid::CATALOG_FILE, 0, &vh.catalog_file)?;
    fork_check(
        &mut errs,
        vol,
        cnid::ATTRIBUTES_FILE,
        0,
        &vh.attributes_file,
    )?;
    fork_check(&mut errs, vol, cnid::STARTUP_FILE, 0, &vh.startup_file)?;
    for e in &entries {
        if let CatalogRecord::File(f) = &e.record {
            fork_check(&mut errs, vol, f.file_id, 0, &f.data_fork)?;
            fork_check(&mut errs, vol, f.file_id, 0xFF, &f.resource_fork)?;
        }
    }
    if let Some(attrs) = vol.attributes_tree() {
        for (k, d) in attrs.leaf_records()? {
            let key = AttrKey::decode_body(&k[2..])?;
            match AttrRecord::decode(d)? {
                AttrRecord::Fork(f) => {
                    for e in f.extents.iter().filter(|e| e.block_count > 0) {
                        claim(&mut errs, key.file_id, e.start_block, e.block_count);
                    }
                }
                AttrRecord::Extents(x) => {
                    for e in x.iter().filter(|e| e.block_count > 0) {
                        claim(&mut errs, key.file_id, e.start_block, e.block_count);
                    }
                }
                AttrRecord::Inline(_) => {}
            }
        }
    }

    let alloc_ext = vol.fork_extents(cnid::ALLOCATION_FILE, 0, &vh.allocation_file)?;
    let mut bitmap = vec![0u8; (vh.allocation_file.total_blocks as u64 * bs) as usize];
    vol.read_fork(&alloc_ext, 0, &mut bitmap)?;
    let mut used = 0u32;
    let mut mismatches = 0usize;
    for (b, o) in owner.iter().enumerate() {
        let bit = bitmap
            .get(b / 8)
            .is_some_and(|byte| byte & (0x80 >> (b % 8)) != 0);
        if bit {
            used += 1;
        }
        if bit != (*o != 0) {
            mismatches += 1;
            if mismatches <= 10 {
                errs.push(format!(
                    "block {b} is {} in the bitmap but {}",
                    if bit { "allocated" } else { "free" },
                    if *o != 0 { "in use" } else { "unused" }
                ));
            }
        }
    }
    if mismatches > 10 {
        errs.push(format!("... {} more bitmap mismatches", mismatches - 10));
    }
    if vh.free_blocks != vh.total_blocks - used {
        errs.push(format!(
            "volume header free blocks {} != {}",
            vh.free_blocks,
            vh.total_blocks - used
        ));
    }
    Ok(errs)
}
