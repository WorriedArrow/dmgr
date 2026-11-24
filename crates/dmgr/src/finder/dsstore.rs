//! Reading and writing Finder `.DS_Store` files.
//!
//! A `.DS_Store` file is a buddy-allocated store holding a B-tree of
//! `(file name, structure id, value)` records.

use std::cmp::Ordering;

use crate::bytes::{PutBe, Reader, be_u32};
use crate::error::{Result, corrupt};

/// A record value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// `long`: a 32-bit integer.
    Long(u32),
    /// `shor`: a 16-bit integer (stored in 4 bytes).
    Short(u16),
    /// `bool`.
    Bool(bool),
    /// `blob`: opaque bytes (often a binary property list).
    Blob(Vec<u8>),
    /// `type`: a four-character code.
    Type([u8; 4]),
    /// `ustr`: a UTF-16 string.
    Ustr(String),
    /// `comp`: a 64-bit integer.
    Comp(u64),
    /// `dutc`: a 64-bit timestamp.
    Dutc(u64),
}

/// One `.DS_Store` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The file the record describes (`.` for the folder itself).
    pub filename: String,
    /// Structure ID, e.g. `Iloc`, `bwsp`, `icvp`.
    pub code: [u8; 4],
    pub value: Value,
}

impl Record {
    pub fn new(filename: impl Into<String>, code: &[u8; 4], value: Value) -> Self {
        Self {
            filename: filename.into(),
            code: *code,
            value,
        }
    }

    fn encode(&self, o: &mut Vec<u8>) {
        let units: Vec<u16> = self.filename.encode_utf16().collect();
        o.put_u32(units.len() as u32);
        for u in units {
            o.put_u16(u);
        }
        o.extend_from_slice(&self.code);
        match &self.value {
            Value::Long(v) => {
                o.extend_from_slice(b"long");
                o.put_u32(*v);
            }
            Value::Short(v) => {
                o.extend_from_slice(b"shor");
                o.put_u32(u32::from(*v));
            }
            Value::Bool(v) => {
                o.extend_from_slice(b"bool");
                o.put_u8(u8::from(*v));
            }
            Value::Blob(b) => {
                o.extend_from_slice(b"blob");
                o.put_u32(b.len() as u32);
                o.extend_from_slice(b);
            }
            Value::Type(t) => {
                o.extend_from_slice(b"type");
                o.extend_from_slice(t);
            }
            Value::Ustr(s) => {
                o.extend_from_slice(b"ustr");
                let units: Vec<u16> = s.encode_utf16().collect();
                o.put_u32(units.len() as u32);
                for u in units {
                    o.put_u16(u);
                }
            }
            Value::Comp(v) => {
                o.extend_from_slice(b"comp");
                o.put_u64(*v);
            }
            Value::Dutc(v) => {
                o.extend_from_slice(b"dutc");
                o.put_u64(*v);
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let n = r.u32()? as usize;
        let mut units = Vec::with_capacity(n);
        for _ in 0..n {
            units.push(r.u16()?);
        }
        let filename = String::from_utf16_lossy(&units);
        let code = r.array()?;
        let ty: [u8; 4] = r.array()?;
        let value = match &ty {
            b"long" => Value::Long(r.u32()?),
            b"shor" => Value::Short(r.u32()? as u16),
            b"bool" => Value::Bool(r.u8()? != 0),
            b"blob" => {
                let n = r.u32()? as usize;
                Value::Blob(r.bytes(n)?.to_vec())
            }
            b"type" => Value::Type(r.array()?),
            b"ustr" => {
                let n = r.u32()? as usize;
                let mut u = Vec::with_capacity(n);
                for _ in 0..n {
                    u.push(r.u16()?);
                }
                Value::Ustr(String::from_utf16_lossy(&u))
            }
            b"comp" => Value::Comp(r.u64()?),
            b"dutc" => Value::Dutc(r.u64()?),
            other => {
                return Err(corrupt(format!(
                    "unknown .DS_Store value type {:?}",
                    String::from_utf8_lossy(other)
                )));
            }
        };
        Ok(Self {
            filename,
            code,
            value,
        })
    }

    fn encoded_len(&self) -> usize {
        let mut v = Vec::new();
        self.encode(&mut v);
        v.len()
    }
}

fn record_order(a: &Record, b: &Record) -> Ordering {
    let la = a.filename.to_lowercase();
    let lb = b.filename.to_lowercase();
    la.cmp(&lb).then_with(|| a.code.cmp(&b.code))
}

const PAGE_SIZE: usize = 4096;

struct NodeData {
    /// Rightmost child for internal nodes; `None` for leaves.
    rightmost: Option<usize>,
    /// `(left child, record)`; the child is unused in leaves.
    entries: Vec<(usize, usize)>,
}

/// Splits items with the given costs into groups that fit in `cap`, with one
/// separator item between consecutive groups. Returns the groups and the
/// separator indices.
fn split(costs: &[usize], cap: usize) -> (Vec<std::ops::Range<usize>>, Vec<usize>) {
    let (mut groups, mut seps) = (Vec::new(), Vec::new());
    let (mut start, mut used, mut i) = (0usize, 0usize, 0usize);
    let n = costs.len();
    while i < n {
        if used + costs[i] > cap && i > start {
            if i + 1 < n {
                groups.push(start..i);
                seps.push(i);
                start = i + 1;
                used = 0;
                i += 1;
                continue;
            } else if i > start + 1 {
                // The last item does not fit: promote its predecessor instead.
                groups.push(start..i - 1);
                seps.push(i - 1);
                start = i;
                used = 0;
            }
        }
        used += costs[i];
        i += 1;
    }
    groups.push(start..n);
    (groups, seps)
}

/// A simple buddy allocator mirroring the on-disk free lists.
struct Buddy {
    free: Vec<Vec<u32>>,
}

impl Buddy {
    fn new() -> Self {
        let mut free = vec![Vec::new(); 32];
        free[31].push(0);
        Self { free }
    }

    fn alloc(&mut self, size: usize) -> u32 {
        let want = (size.max(32).next_power_of_two().trailing_zeros()) as usize;
        let mut level = want;
        while self.free[level].is_empty() {
            level += 1;
            assert!(level < 32, ".DS_Store buddy space exhausted");
        }
        self.free[level].sort_unstable();
        let off = self.free[level].remove(0);
        while level > want {
            level -= 1;
            self.free[level].push(off + (1u32 << level));
        }
        off
    }
}

/// Serializes records into a `.DS_Store` file.
pub fn write(mut records: Vec<Record>) -> Vec<u8> {
    records.sort_by(record_order);
    records.dedup_by(|a, b| record_order(a, b) == Ordering::Equal);

    // Build the B-tree bottom-up. Leaves hold runs of records; the record
    // between two neighbouring nodes is promoted into their parent.
    let cap = PAGE_SIZE - 8;
    let costs: Vec<usize> = records.iter().map(Record::encoded_len).collect();
    let mut nodes: Vec<NodeData> = Vec::new();
    let (groups, mut seps) = split(&costs, cap);
    let mut level: Vec<usize> = Vec::new();
    for g in groups {
        nodes.push(NodeData {
            rightmost: None,
            entries: g.map(|r| (0, r)).collect(),
        });
        level.push(nodes.len() - 1);
    }
    let mut levels = 0u32;
    while level.len() > 1 {
        let sep_costs: Vec<usize> = seps.iter().map(|&r| costs[r] + 4).collect();
        let (groups, promoted) = split(&sep_costs, cap);
        let mut next = Vec::new();
        for g in groups {
            nodes.push(NodeData {
                rightmost: Some(level[g.end]),
                entries: g.map(|k| (level[k], seps[k])).collect(),
            });
            next.push(nodes.len() - 1);
        }
        seps = promoted.iter().map(|&k| seps[k]).collect();
        level = next;
        levels += 1;
    }
    let root_node = level[0];

    // Allocate blocks: header (32 bytes at 0), root block, DSDB, nodes.
    let n_blocks = 2 + nodes.len();
    let offsets_bytes = n_blocks.next_multiple_of(256) * 4;
    let root_size = (8 + offsets_bytes + 13 + 32 * 4 + 64 * 4).next_power_of_two();
    let mut buddy = Buddy::new();
    let header_off = buddy.alloc(32);
    debug_assert_eq!(header_off, 0);
    let root_off = buddy.alloc(root_size);
    let dsdb_off = buddy.alloc(20);
    let node_offs: Vec<u32> = nodes.iter().map(|_| buddy.alloc(PAGE_SIZE)).collect();

    let addr = |off: u32, size: usize| off | size.next_power_of_two().max(32).trailing_zeros();
    // Block numbers: 0 = root block, 1 = DSDB, 2.. = nodes.
    let mut offsets = vec![addr(root_off, root_size), addr(dsdb_off, 32)];
    offsets.extend(node_offs.iter().map(|&o| addr(o, PAGE_SIZE)));
    let block_of_node = |i: usize| (i + 2) as u32;

    let end = node_offs
        .iter()
        .map(|&o| o as usize + PAGE_SIZE)
        .chain([root_off as usize + root_size, dsdb_off as usize + 32])
        .max()
        .unwrap_or(0);
    let mut file = vec![0u8; 4 + end];

    // Nodes.
    for (i, node) in nodes.iter().enumerate() {
        let mut b = Vec::with_capacity(PAGE_SIZE);
        b.put_u32(node.rightmost.map_or(0, block_of_node));
        b.put_u32(node.entries.len() as u32);
        for &(child, rec) in &node.entries {
            if node.rightmost.is_some() {
                b.put_u32(block_of_node(child));
            }
            records[rec].encode(&mut b);
        }
        debug_assert!(b.len() <= PAGE_SIZE);
        let o = 4 + node_offs[i] as usize;
        file[o..o + b.len()].copy_from_slice(&b);
    }

    // DSDB.
    let mut d = Vec::new();
    d.put_u32(block_of_node(root_node));
    d.put_u32(levels);
    d.put_u32(records.len() as u32);
    d.put_u32(nodes.len() as u32);
    d.put_u32(PAGE_SIZE as u32);
    let o = 4 + dsdb_off as usize;
    file[o..o + d.len()].copy_from_slice(&d);

    // Root block: offsets, table of contents, free lists.
    let mut r = Vec::with_capacity(root_size);
    r.put_u32(offsets.len() as u32);
    r.put_u32(0);
    for i in 0..offsets_bytes / 4 {
        r.put_u32(offsets.get(i).copied().unwrap_or(0));
    }
    r.put_u32(1);
    r.put_u8(4);
    r.extend_from_slice(b"DSDB");
    r.put_u32(1);
    for list in &mut buddy.free {
        list.sort_unstable();
        r.put_u32(list.len() as u32);
        for &off in list.iter() {
            r.put_u32(off);
        }
    }
    assert!(r.len() <= root_size, ".DS_Store root block overflow");
    let o = 4 + root_off as usize;
    file[o..o + r.len()].copy_from_slice(&r);

    // File header.
    let mut h = Vec::with_capacity(36);
    h.put_u32(1);
    h.extend_from_slice(b"Bud1");
    h.put_u32(root_off);
    h.put_u32(root_size as u32);
    h.put_u32(root_off);
    h.extend_from_slice(&[
        0, 0, 0x10, 0x0C, 0, 0, 0, 0x87, 0, 0, 0x20, 0x0B, 0, 0, 0, 0,
    ]);
    file[..h.len()].copy_from_slice(&h);
    file
}

/// Parses a `.DS_Store` file into its records (in B-tree order).
pub fn read(data: &[u8]) -> Result<Vec<Record>> {
    if data.len() < 36 || &data[4..8] != b"Bud1" {
        return Err(corrupt("not a .DS_Store file"));
    }
    let root_off = be_u32(data, 8) as usize;
    let block = |off: usize| -> Result<&[u8]> {
        data.get(4 + off..)
            .ok_or_else(|| corrupt(".DS_Store block out of range"))
    };
    let mut r = Reader::new(block(root_off)?);
    let count = r.u32()? as usize;
    r.skip(4)?;
    let mut offsets = Vec::with_capacity(count);
    for i in 0..count.next_multiple_of(256) {
        let v = r.u32()?;
        if i < count {
            offsets.push(v);
        }
    }
    let toc_n = r.u32()?;
    let mut dsdb = None;
    for _ in 0..toc_n {
        let n = r.u8()? as usize;
        let name = r.bytes(n)?;
        let v = r.u32()?;
        if name == b"DSDB" {
            dsdb = Some(v);
        }
    }
    let addr_of = |blk: u32| -> Result<usize> {
        let a = *offsets
            .get(blk as usize)
            .ok_or_else(|| corrupt(".DS_Store block id out of range"))?;
        Ok((a & !0x1F) as usize)
    };
    let dsdb = dsdb.ok_or_else(|| corrupt(".DS_Store has no DSDB"))?;
    let mut d = Reader::new(block(addr_of(dsdb)?)?);
    let root = d.u32()?;
    let mut out = Vec::new();
    fn walk<'d>(
        blk: u32,
        depth: u32,
        addr_of: &dyn Fn(u32) -> Result<usize>,
        block: &dyn Fn(usize) -> Result<&'d [u8]>,
        out: &mut Vec<Record>,
    ) -> Result<()> {
        if depth > 32 {
            return Err(corrupt(".DS_Store B-tree too deep"));
        }
        let mut r = Reader::new(block(addr_of(blk)?)?);
        let p = r.u32()?;
        let n = r.u32()?;
        for _ in 0..n {
            if p != 0 {
                let child = r.u32()?;
                walk(child, depth + 1, addr_of, block, out)?;
            }
            out.push(Record::decode(&mut r)?);
        }
        if p != 0 {
            walk(p, depth + 1, addr_of, block, out)?;
        }
        Ok(())
    }
    walk(root, 0, &addr_of, &block, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        let recs = vec![
            Record::new(
                "b",
                b"Iloc",
                Value::Blob(vec![
                    0, 0, 0, 10, 0, 0, 0, 20, 255, 255, 255, 255, 255, 255, 0, 0,
                ]),
            ),
            Record::new(".", b"vSrn", Value::Long(1)),
            Record::new("A", b"Iloc", Value::Blob(vec![1; 16])),
            Record::new(".", b"vstl", Value::Type(*b"icnv")),
        ];
        let data = write(recs.clone());
        let back = read(&data).unwrap();
        let mut sorted = recs;
        sorted.sort_by(record_order);
        assert_eq!(back, sorted);
        assert_eq!(&data[0..8], b"\0\0\0\x01Bud1");
    }

    #[test]
    fn roundtrip_large() {
        let recs: Vec<Record> = (0..3000)
            .map(|i| {
                Record::new(
                    format!("file number {i:05}"),
                    b"Iloc",
                    Value::Blob(vec![i as u8; 16]),
                )
            })
            .collect();
        let data = write(recs.clone());
        let back = read(&data).unwrap();
        assert_eq!(back.len(), 3000);
        let mut sorted = recs;
        sorted.sort_by(record_order);
        assert_eq!(back, sorted);
    }
}
