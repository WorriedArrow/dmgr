//! HFS+ B-tree files: building from sorted records and reading back.

use std::ops::Range;

use super::format::{HeaderRecord, NodeDescriptor, bt_attr, node_kind};
use crate::bytes::{be_u16, set_be_u16};
use crate::error::{Result, corrupt, invalid};

/// Shape parameters for a B-tree file.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TreeParams {
    pub node_size: usize,
    pub max_key_length: u16,
    pub key_compare_type: u8,
    pub attributes: u32,
}

impl TreeParams {
    fn variable_index_keys(&self) -> bool {
        self.attributes & bt_attr::VARIABLE_INDEX_KEYS != 0
    }

    /// Bytes available for records (and their offsets) in one node.
    fn capacity(&self) -> usize {
        self.node_size - NodeDescriptor::SIZE - 2
    }

    /// Nodes tracked by the map record in the header node.
    fn header_map_bits(&self) -> usize {
        (self.node_size - 256) * 8
    }

    /// Nodes tracked by each additional map node.
    fn map_node_bits(&self) -> usize {
        (self.node_size - 20) * 8
    }

    fn index_key(&self, key: &[u8]) -> Vec<u8> {
        if self.variable_index_keys() {
            key.to_vec()
        } else {
            let mut k = vec![0u8; self.max_key_length as usize + 2];
            set_be_u16(&mut k, 0, self.max_key_length);
            k[2..key.len()].copy_from_slice(&key[2..]);
            k
        }
    }
}

/// A leaf record: encoded key (including its `keyLength` prefix) and data.
pub(crate) type Record = (Vec<u8>, Vec<u8>);

#[derive(Debug)]
struct Plan {
    /// Record index ranges for each leaf node (node numbers 1..=leaves.len()).
    leaves: Vec<Range<usize>>,
    /// For each index level (bottom-up): (first node number, child ranges per node).
    index_levels: Vec<(u32, Vec<Range<usize>>)>,
    /// Header + leaf + index nodes.
    used_nodes: u32,
}

fn pack(costs: impl Iterator<Item = usize>, cap: usize) -> Result<Vec<Range<usize>>> {
    let mut out = Vec::new();
    let (mut start, mut used, mut n) = (0usize, 0usize, 0usize);
    for (i, cost) in costs.enumerate() {
        if cost > cap {
            return Err(invalid("B-tree record does not fit in a node"));
        }
        if used + cost > cap {
            out.push(start..i);
            start = i;
            used = 0;
        }
        used += cost;
        n = i + 1;
    }
    if start < n {
        out.push(start..n);
    }
    Ok(out)
}

fn plan(params: &TreeParams, records: &[Record]) -> Result<Plan> {
    let cap = params.capacity();
    let leaves = pack(records.iter().map(|(k, d)| k.len() + d.len() + 2), cap)?;
    let mut next_node = 1 + leaves.len() as u32;
    let mut index_levels = Vec::new();
    // first keys of nodes in the current level
    let mut level_keys: Vec<usize> = leaves.iter().map(|r| r.start).collect();
    while level_keys.len() > 1 {
        let costs = level_keys
            .iter()
            .map(|&ri| params.index_key(&records[ri].0).len() + 4 + 2);
        let nodes = pack(costs, cap)?;
        let first = next_node;
        next_node += nodes.len() as u32;
        level_keys = nodes.iter().map(|r| level_keys[r.start]).collect();
        index_levels.push((first, nodes));
    }
    Ok(Plan {
        leaves,
        index_levels,
        used_nodes: next_node,
    })
}

/// Number of nodes (header, leaves and index) needed to hold `records`.
pub(crate) fn nodes_required(params: &TreeParams, records: &[Record]) -> Result<u32> {
    Ok(plan(params, records)?.used_nodes)
}

/// Number of extra map nodes needed for a tree of `total` nodes.
fn map_nodes_for(params: &TreeParams, total: usize) -> usize {
    let hdr = params.header_map_bits();
    if total <= hdr {
        0
    } else {
        (total - hdr).div_ceil(params.map_node_bits())
    }
}

/// Total node count for a tree that needs `used` nodes plus `slack` free ones,
/// including any map nodes.
pub(crate) fn total_nodes_for(params: &TreeParams, used: u32, slack: u32) -> u32 {
    let mut total = (used + slack) as usize;
    loop {
        let need = used as usize + map_nodes_for(params, total) + slack as usize;
        if need <= total {
            return total as u32;
        }
        total = need;
    }
}

fn write_node(buf: &mut [u8], desc: NodeDescriptor, records: &[&[u8]]) {
    let node_size = buf.len();
    let mut head = Vec::with_capacity(NodeDescriptor::SIZE);
    desc.encode(&mut head);
    buf[..NodeDescriptor::SIZE].copy_from_slice(&head);
    let mut pos = NodeDescriptor::SIZE;
    for (i, rec) in records.iter().enumerate() {
        buf[pos..pos + rec.len()].copy_from_slice(rec);
        set_be_u16(buf, node_size - 2 * (i + 1), pos as u16);
        pos += rec.len();
    }
    set_be_u16(buf, node_size - 2 * (records.len() + 1), pos as u16);
}

fn set_bit(bits: &mut [u8], n: usize) {
    bits[n / 8] |= 0x80 >> (n % 8);
}

/// Serializes a complete B-tree file with exactly `total_nodes` nodes.
///
/// `records` must already be sorted by the tree's key order.
pub(crate) fn build(
    params: &TreeParams,
    records: &[Record],
    total_nodes: u32,
    clump_size: u32,
) -> Result<Vec<u8>> {
    let ns = params.node_size;
    let p = plan(params, records)?;
    let map_nodes = map_nodes_for(params, total_nodes as usize);
    let used = p.used_nodes as usize + map_nodes;
    if used > total_nodes as usize {
        return Err(invalid("B-tree node budget too small"));
    }
    let mut data = vec![0u8; total_nodes as usize * ns];

    // Leaves.
    let n_leaves = p.leaves.len() as u32;
    for (i, r) in p.leaves.iter().enumerate() {
        let num = i as u32 + 1;
        let recs: Vec<Vec<u8>> = records[r.clone()]
            .iter()
            .map(|(k, d)| {
                let mut v = k.clone();
                v.extend_from_slice(d);
                v
            })
            .collect();
        let refs: Vec<&[u8]> = recs.iter().map(Vec::as_slice).collect();
        let desc = NodeDescriptor {
            flink: if num < n_leaves { num + 1 } else { 0 },
            blink: num.saturating_sub(1),
            kind: node_kind::LEAF,
            height: 1,
            num_records: refs.len() as u16,
        };
        write_node(
            &mut data[num as usize * ns..(num as usize + 1) * ns],
            desc,
            &refs,
        );
    }

    // Index levels: children of level 0 are leaves.
    let mut child_first: u32 = 1;
    let mut child_keys: Vec<usize> = p.leaves.iter().map(|r| r.start).collect();
    for (depth, (first, nodes)) in p.index_levels.iter().enumerate() {
        let count = nodes.len() as u32;
        for (i, r) in nodes.iter().enumerate() {
            let num = first + i as u32;
            let recs: Vec<Vec<u8>> = r
                .clone()
                .map(|ci| {
                    let mut v = params.index_key(&records[child_keys[ci]].0);
                    v.extend_from_slice(&(child_first + ci as u32).to_be_bytes());
                    v
                })
                .collect();
            let refs: Vec<&[u8]> = recs.iter().map(Vec::as_slice).collect();
            let desc = NodeDescriptor {
                flink: if i as u32 + 1 < count { num + 1 } else { 0 },
                blink: if i > 0 { num - 1 } else { 0 },
                kind: node_kind::INDEX,
                height: depth as u8 + 2,
                num_records: refs.len() as u16,
            };
            write_node(
                &mut data[num as usize * ns..(num as usize + 1) * ns],
                desc,
                &refs,
            );
        }
        child_keys = nodes.iter().map(|r| child_keys[r.start]).collect();
        child_first = *first;
    }

    let (depth, root) = if records.is_empty() {
        (0u16, 0u32)
    } else if let Some((first, _)) = p.index_levels.last() {
        (p.index_levels.len() as u16 + 1, *first)
    } else {
        (1, 1)
    };

    // Node allocation bitmap spread over the header map record and map nodes.
    let mut bits = vec![0u8; params.header_map_bits() / 8 + map_nodes * params.map_node_bits() / 8];
    for n in 0..p.used_nodes as usize {
        set_bit(&mut bits, n);
    }
    let first_map = p.used_nodes;
    for m in 0..map_nodes {
        set_bit(&mut bits, first_map as usize + m);
    }

    // Map nodes.
    let hdr_bytes = params.header_map_bits() / 8;
    let map_bytes = params.map_node_bits() / 8;
    for m in 0..map_nodes {
        let num = first_map + m as u32;
        let desc = NodeDescriptor {
            flink: if m + 1 < map_nodes { num + 1 } else { 0 },
            blink: 0,
            kind: node_kind::MAP,
            height: 0,
            num_records: 1,
        };
        let slice = &bits[hdr_bytes + m * map_bytes..hdr_bytes + (m + 1) * map_bytes];
        write_node(
            &mut data[num as usize * ns..(num as usize + 1) * ns],
            desc,
            &[slice],
        );
    }

    // Header node.
    let header = HeaderRecord {
        tree_depth: depth,
        root_node: root,
        leaf_records: records.len() as u32,
        first_leaf_node: if n_leaves > 0 { 1 } else { 0 },
        last_leaf_node: n_leaves,
        node_size: ns as u16,
        max_key_length: params.max_key_length,
        total_nodes,
        free_nodes: total_nodes - used as u32,
        clump_size,
        btree_type: 0,
        key_compare_type: params.key_compare_type,
        attributes: params.attributes,
    };
    let mut hdr = Vec::with_capacity(HeaderRecord::SIZE);
    header.encode(&mut hdr);
    let user = [0u8; 128];
    let desc = NodeDescriptor {
        flink: if map_nodes > 0 { first_map } else { 0 },
        blink: 0,
        kind: node_kind::HEADER,
        height: 0,
        num_records: 3,
    };
    write_node(&mut data[..ns], desc, &[&hdr, &user, &bits[..hdr_bytes]]);
    Ok(data)
}

/// A B-tree file loaded into memory.
#[derive(Debug, Clone)]
pub struct BTreeFile {
    data: Vec<u8>,
    header: HeaderRecord,
}

/// A parsed node.
#[derive(Debug, Clone)]
pub struct Node<'a> {
    pub number: u32,
    pub desc: NodeDescriptor,
    pub records: Vec<&'a [u8]>,
}

impl BTreeFile {
    /// Parses the header node of a B-tree file.
    pub fn new(data: Vec<u8>) -> Result<Self> {
        if data.len() < 512 {
            return Err(corrupt("B-tree file too small"));
        }
        let desc = NodeDescriptor::decode(&data)?;
        if desc.kind != node_kind::HEADER {
            return Err(corrupt("B-tree header node has wrong kind"));
        }
        let header = HeaderRecord::decode(&data[NodeDescriptor::SIZE..])?;
        let ns = header.node_size as usize;
        if !ns.is_power_of_two() || !(512..=32768).contains(&ns) {
            return Err(corrupt(format!("invalid B-tree node size {ns}")));
        }
        if data.len() < ns * header.total_nodes as usize {
            return Err(corrupt("B-tree file shorter than its node count"));
        }
        Ok(Self { data, header })
    }

    pub fn header(&self) -> &HeaderRecord {
        &self.header
    }

    pub fn node_size(&self) -> usize {
        self.header.node_size as usize
    }

    /// Returns the raw bytes of node `n`.
    pub fn raw_node(&self, n: u32) -> Result<&[u8]> {
        let ns = self.node_size();
        let start = n as usize * ns;
        self.data
            .get(start..start + ns)
            .ok_or_else(|| corrupt(format!("B-tree node {n} out of range")))
    }

    /// Parses node `n` into its descriptor and record slices.
    pub fn node(&self, n: u32) -> Result<Node<'_>> {
        let buf = self.raw_node(n)?;
        let ns = buf.len();
        let desc = NodeDescriptor::decode(buf)?;
        let count = desc.num_records as usize;
        if 2 * (count + 1) + NodeDescriptor::SIZE > ns {
            return Err(corrupt(format!("B-tree node {n} has too many records")));
        }
        let offset = |i: usize| be_u16(buf, ns - 2 * (i + 1)) as usize;
        let mut records = Vec::with_capacity(count);
        for i in 0..count {
            let (start, end) = (offset(i), offset(i + 1));
            if start < NodeDescriptor::SIZE || end < start || end > ns - 2 * (count + 1) {
                return Err(corrupt(format!(
                    "B-tree node {n} record {i} has bad offsets"
                )));
            }
            records.push(&buf[start..end]);
        }
        Ok(Node {
            number: n,
            desc,
            records,
        })
    }

    /// Splits a record into `(key including length, data)`.
    pub fn split_record<'a>(
        &self,
        rec: &'a [u8],
        index_node: bool,
    ) -> Result<(&'a [u8], &'a [u8])> {
        let key_len = if index_node && self.header.attributes & bt_attr::VARIABLE_INDEX_KEYS == 0 {
            self.header.max_key_length as usize
        } else {
            if rec.len() < 2 {
                return Err(corrupt("B-tree record too short"));
            }
            be_u16(rec, 0) as usize
        };
        let mut data_start = 2 + key_len;
        if data_start % 2 == 1 {
            data_start += 1;
        }
        if data_start > rec.len() {
            return Err(corrupt("B-tree key longer than its record"));
        }
        Ok((&rec[..2 + key_len], &rec[data_start..]))
    }

    /// Collects every leaf record by walking the leaf chain.
    pub fn leaf_records(&self) -> Result<Vec<(&[u8], &[u8])>> {
        let mut out = Vec::with_capacity(self.header.leaf_records as usize);
        let mut n = self.header.first_leaf_node;
        let mut visited = 0u32;
        while n != 0 {
            visited += 1;
            if visited > self.header.total_nodes {
                return Err(corrupt("B-tree leaf chain loops"));
            }
            let node = self.node(n)?;
            if node.desc.kind != node_kind::LEAF {
                return Err(corrupt(format!(
                    "B-tree node {n} in leaf chain is not a leaf"
                )));
            }
            for rec in node.records {
                out.push(self.split_record(rec, false)?);
            }
            n = node.desc.flink;
        }
        Ok(out)
    }

    /// Returns whether node `n` is marked in use in the node bitmap.
    pub fn node_in_use(&self, n: u32) -> Result<bool> {
        let header = self.node(0)?;
        let map = header
            .records
            .get(2)
            .ok_or_else(|| corrupt("B-tree header node has no map record"))?;
        let mut bits: &[u8] = map;
        let mut base = 0usize;
        let mut next = header.desc.flink;
        let mut n = n as usize;
        loop {
            if n < base + bits.len() * 8 {
                n -= base;
                return Ok(bits[n / 8] & (0x80 >> (n % 8)) != 0);
            }
            if next == 0 {
                return Ok(false);
            }
            base += bits.len() * 8;
            let node = self.node(next)?;
            bits = node
                .records
                .first()
                .ok_or_else(|| corrupt("B-tree map node has no record"))?;
            next = node.desc.flink;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes::PutBe;

    fn params(ns: usize) -> TreeParams {
        TreeParams {
            node_size: ns,
            max_key_length: 10,
            key_compare_type: 0,
            attributes: bt_attr::BIG_KEYS,
        }
    }

    fn rec(i: u32) -> Record {
        let mut k = Vec::new();
        k.put_u16(10);
        k.put_u8(0);
        k.put_u8(0);
        k.put_u32(i);
        k.put_u32(0);
        (k, vec![0xAB; 64])
    }

    #[test]
    fn empty_tree() {
        let p = params(4096);
        let data = build(&p, &[], 4, 4096 * 4).unwrap();
        let t = BTreeFile::new(data).unwrap();
        assert_eq!(t.header().tree_depth, 0);
        assert_eq!(t.header().free_nodes, 3);
        assert!(t.leaf_records().unwrap().is_empty());
        assert!(t.node_in_use(0).unwrap());
        assert!(!t.node_in_use(1).unwrap());
    }

    #[test]
    fn multi_level_with_map_nodes() {
        let p = params(512);
        let records: Vec<Record> = (0..5000).map(rec).collect();
        let used = nodes_required(&p, &records).unwrap();
        let total = total_nodes_for(&p, used, 4000);
        assert!(total as usize > p.header_map_bits());
        let data = build(&p, &records, total, 0).unwrap();
        let t = BTreeFile::new(data).unwrap();
        assert!(t.header().tree_depth >= 3);
        let leaves = t.leaf_records().unwrap();
        assert_eq!(leaves.len(), 5000);
        for (i, (k, d)) in leaves.iter().enumerate() {
            assert_eq!(&k[..], &rec(i as u32).0[..]);
            assert_eq!(d.len(), 64);
        }
        for n in 0..total {
            let expect = n < used || n >= used && n < total - t.header().free_nodes;
            assert_eq!(t.node_in_use(n).unwrap(), expect, "node {n}");
        }
    }
}
