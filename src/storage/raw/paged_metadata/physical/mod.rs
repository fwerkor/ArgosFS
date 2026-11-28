//! Experimental member-local authenticated reference-count and free-space
//! map. A deterministic three-slot radix layout removes the allocator's
//! self-allocation recursion: at most two retained allocator roots plus one
//! prepared transaction can refer to any logical radix node. This module
//! deliberately does not publish transaction roots; its caller must bind
//! both allocator and logical roots in one durable root/certificate protocol.
use super::Arena;
use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

const PAGE: usize = 4096;
const LEAF: u64 = 512;
const FANOUT: u64 = 64;
const PTR: usize = 48;
const HEADER: usize = 40;
const MAGIC: &[u8; 8] = b"ARGAL003";
type Bytes = [u8; PAGE];
type Digest32 = [u8; 32];

fn corrupt(message: &str) -> ArgosError {
    ArgosError::CorruptedMetadata(message.into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ref {
    slot: u8,
    free: u64,
    hash: Digest32,
}

/// The empty root represents an entirely free region. The root is meaningful
/// only together with its fixed member-local region geometry and page count.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AllocatorRoot(Option<Ref>);

impl AllocatorRoot {
    pub const WIRE_SIZE: usize = PTR;

    pub fn encode(self) -> [u8; PTR] {
        encode_ref(self.0)
    }

    pub fn decode(bytes: &[u8; PTR]) -> Result<Self> {
        Ok(Self(decode_ref(bytes)?))
    }
}

fn encode_ref(reference: Option<Ref>) -> [u8; PTR] {
    let mut bytes = [0; PTR];
    if let Some(reference) = reference {
        bytes[0] = reference.slot;
        bytes[8..16].copy_from_slice(&reference.free.to_le_bytes());
        bytes[16..].copy_from_slice(&reference.hash);
    } else {
        bytes[0] = u8::MAX;
    }
    bytes
}

fn decode_ref(bytes: &[u8]) -> Result<Option<Ref>> {
    if bytes.len() != PTR || bytes[1..8].iter().any(|&b| b != 0) {
        return Err(corrupt("invalid physical allocator pointer"));
    }
    if bytes[0] == u8::MAX {
        if bytes[8..].iter().any(|&b| b != 0) {
            return Err(corrupt("noncanonical empty allocator pointer"));
        }
        return Ok(None);
    }
    if bytes[0] > 2 {
        return Err(corrupt("invalid physical allocator slot"));
    }
    Ok(Some(Ref {
        slot: bytes[0],
        free: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        hash: bytes[16..].try_into().unwrap(),
    }))
}

#[derive(Clone, Debug)]
struct Level {
    first: u64,
    nodes: u64,
    span: u64,
}

#[derive(Debug)]
pub struct AllocationMap {
    arena: Arena,
    pages: u64,
    levels: Vec<Level>,
    required: u64,
    reads: AtomicUsize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Node {
    Leaf(Vec<u32>),
    Branch(Vec<Option<Ref>>),
}

impl AllocationMap {
    /// Size of the separately reserved allocator area. It is approximately
    /// 3/512 of the tracked page area, plus upper levels and final rounding.
    pub fn required_bytes(pages: u64) -> Result<u64> {
        Ok(Self::geometry(pages)?.1)
    }

    fn geometry(pages: u64) -> Result<(Vec<Level>, u64)> {
        if pages == 0 || pages > u64::MAX / PAGE as u64 {
            return Err(ArgosError::Invalid(
                "invalid physical allocator page count".into(),
            ));
        }
        let mut span = LEAF;
        let mut first = 0u64;
        let mut levels = Vec::new();
        loop {
            let nodes = pages.div_ceil(span);
            levels.push(Level { first, nodes, span });
            first = first
                .checked_add(
                    nodes
                        .checked_mul(3)
                        .ok_or_else(|| corrupt("allocator geometry overflow"))?,
                )
                .ok_or_else(|| corrupt("allocator geometry overflow"))?;
            if nodes == 1 {
                break;
            }
            span = span
                .checked_mul(FANOUT)
                .ok_or_else(|| corrupt("allocator span overflow"))?;
        }
        let bytes = first
            .checked_mul(PAGE as u64)
            .ok_or_else(|| corrupt("allocator size overflow"))?;
        Ok((levels, bytes))
    }

    pub fn new(backend: &dyn StorageBackend, arena: Arena, pages: u64) -> Result<Self> {
        let (levels, required) = Self::geometry(pages)?;
        if !arena.offset.is_multiple_of(PAGE as u64)
            || arena.length < required
            || arena.offset.checked_add(arena.length).is_none_or(|end| {
                backend
                    .capacity(&arena.disk_id)
                    .map_or(true, |capacity| end > capacity)
            })
        {
            return Err(ArgosError::Invalid(
                "invalid physical allocator arena".into(),
            ));
        }
        Ok(Self {
            arena,
            pages,
            levels,
            required,
            reads: AtomicUsize::new(0),
        })
    }

    pub(super) fn read_calls(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    fn top(&self) -> usize {
        self.levels.len() - 1
    }

    fn span(&self, level: usize, index: u64) -> Result<u64> {
        let layout = self
            .levels
            .get(level)
            .ok_or_else(|| corrupt("allocator level outside geometry"))?;
        if index >= layout.nodes {
            return Err(corrupt("allocator node outside geometry"));
        }
        let start = index
            .checked_mul(layout.span)
            .ok_or_else(|| corrupt("allocator offset overflow"))?;
        Ok((self.pages - start).min(layout.span))
    }

    fn address(&self, level: usize, index: u64, slot: u8) -> Result<u64> {
        self.span(level, index)?;
        if slot > 2 {
            return Err(corrupt("invalid allocator slot"));
        }
        Ok(self.arena.offset
            + (self.levels[level].first + index * 3 + u64::from(slot)) * PAGE as u64)
    }

    fn empty(level: usize) -> Node {
        if level == 0 {
            Node::Leaf(vec![0; LEAF as usize])
        } else {
            Node::Branch(vec![None; FANOUT as usize])
        }
    }

    fn free_count(&self, level: usize, index: u64, node: &Node) -> Result<u64> {
        let span = self.span(level, index)?;
        match node {
            Node::Leaf(counts) if level == 0 && counts.len() == LEAF as usize => {
                if counts[span as usize..].iter().any(|&count| count != 0) {
                    return Err(corrupt("allocated page outside tracked region"));
                }
                Ok(counts[..span as usize]
                    .iter()
                    .filter(|&&count| count == 0)
                    .count() as u64)
            }
            Node::Branch(children) if level > 0 && children.len() == FANOUT as usize => {
                let mut free = 0u64;
                for (i, reference) in children.iter().enumerate() {
                    let child = index * FANOUT + i as u64;
                    if child >= self.levels[level - 1].nodes {
                        if reference.is_some() {
                            return Err(corrupt("allocator child outside geometry"));
                        }
                        continue;
                    }
                    let span = self.span(level - 1, child)?;
                    let available = reference.map_or(span, |reference| reference.free);
                    if available > span || (reference.is_some() && available == span) {
                        return Err(corrupt("invalid allocator child free count"));
                    }
                    free += available;
                }
                Ok(free)
            }
            _ => Err(corrupt("invalid allocator node type")),
        }
    }

    fn read_node(
        &self,
        backend: &dyn StorageBackend,
        level: usize,
        index: u64,
        reference: Option<Ref>,
    ) -> Result<Node> {
        let Some(reference) = reference else {
            return Ok(Self::empty(level));
        };
        let mut bytes = [0; PAGE];
        self.reads.fetch_add(1, Ordering::Relaxed);
        backend.read_at(
            &self.arena.disk_id,
            self.address(level, index, reference.slot)?,
            &mut bytes,
        )?;
        let digest: Digest32 = Sha256::digest(bytes).into();
        if digest != reference.hash {
            return Err(ArgosError::Checksum(
                "physical allocator page digest mismatch".into(),
            ));
        }
        let span = self.span(level, index)?;
        if &bytes[..8] != MAGIC
            || bytes[8] as usize != level
            || bytes[9] != reference.slot
            || bytes[10..16].iter().any(|&b| b != 0)
            || u64::from_le_bytes(bytes[16..24].try_into().unwrap()) != index
            || u64::from_le_bytes(bytes[24..32].try_into().unwrap()) != span
            || u64::from_le_bytes(bytes[32..40].try_into().unwrap()) != reference.free
        {
            return Err(corrupt("invalid physical allocator page header"));
        }
        let body_len = if level == 0 {
            LEAF as usize * 4
        } else {
            FANOUT as usize * PTR
        };
        let body = &bytes[HEADER..HEADER + body_len];
        if bytes[HEADER + body_len..].iter().any(|&b| b != 0) {
            return Err(corrupt("invalid physical allocator padding"));
        }
        let node = if level == 0 {
            Node::Leaf(
                body.chunks_exact(4)
                    .map(|value| u32::from_le_bytes(value.try_into().unwrap()))
                    .collect(),
            )
        } else {
            Node::Branch(
                body.chunks_exact(PTR)
                    .map(decode_ref)
                    .collect::<Result<_>>()?,
            )
        };
        if self.free_count(level, index, &node)? != reference.free || reference.free == span {
            return Err(corrupt("physical allocator free count mismatch"));
        }
        Ok(node)
    }

    fn encode_node(&self, level: usize, index: u64, slot: u8, node: &Node) -> Result<(Ref, Bytes)> {
        let free = self.free_count(level, index, node)?;
        let mut bytes = [0; PAGE];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8] = level as u8;
        bytes[9] = slot;
        bytes[16..24].copy_from_slice(&index.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.span(level, index)?.to_le_bytes());
        bytes[32..40].copy_from_slice(&free.to_le_bytes());
        match node {
            Node::Leaf(counts) => {
                for (dst, count) in bytes[HEADER..].chunks_exact_mut(4).zip(counts) {
                    dst.copy_from_slice(&count.to_le_bytes());
                }
            }
            Node::Branch(children) => {
                for (dst, reference) in bytes[HEADER..].chunks_exact_mut(PTR).zip(children) {
                    dst.copy_from_slice(&encode_ref(*reference));
                }
            }
        }
        Ok((
            Ref {
                slot,
                free,
                hash: Sha256::digest(bytes).into(),
            },
            bytes,
        ))
    }

    /// Validate only the root node, with zero I/O for an implicit empty map.
    pub fn open_root(&self, backend: &dyn StorageBackend, root: AllocatorRoot) -> Result<()> {
        self.read_node(backend, self.top(), 0, root.0)?;
        Ok(())
    }

    pub fn reference_count(
        &self,
        backend: &dyn StorageBackend,
        root: AllocatorRoot,
        page: u64,
    ) -> Result<u32> {
        if page >= self.pages {
            return Err(ArgosError::Invalid("allocator page outside region".into()));
        }
        let mut reference = root.0;
        for level in (0..=self.top()).rev() {
            let Some(current) = reference else {
                return Ok(0);
            };
            let index = page / self.levels[level].span;
            match self.read_node(backend, level, index, Some(current))? {
                Node::Leaf(counts) => return Ok(counts[(page % LEAF) as usize]),
                Node::Branch(children) => {
                    reference = children[((page / self.levels[level - 1].span) % FANOUT) as usize];
                }
            }
        }
        Err(corrupt("allocator lookup did not reach leaf"))
    }

    /// Read many references by grouping them by physical leaf, without
    /// fetching the same authenticated path once per branch child.
    pub fn reference_counts(
        &self,
        backend: &dyn StorageBackend,
        root: AllocatorRoot,
        pages: &std::collections::BTreeSet<u64>,
    ) -> Result<BTreeMap<u64, u32>> {
        if pages.last().is_some_and(|page| *page >= self.pages) {
            return Err(ArgosError::Invalid("allocator page outside region".into()));
        }
        let pages: Vec<_> = pages.iter().copied().collect();
        let mut result = BTreeMap::new();
        self.collect_counts(backend, self.top(), 0, root.0, &pages, &mut result)?;
        Ok(result)
    }

    fn collect_counts(
        &self,
        backend: &dyn StorageBackend,
        level: usize,
        index: u64,
        reference: Option<Ref>,
        pages: &[u64],
        out: &mut BTreeMap<u64, u32>,
    ) -> Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let Some(reference) = reference else {
            out.extend(pages.iter().map(|&page| (page, 0)));
            return Ok(());
        };
        match self.read_node(backend, level, index, Some(reference))? {
            Node::Leaf(counts) => {
                out.extend(
                    pages
                        .iter()
                        .map(|&page| (page, counts[(page % LEAF) as usize])),
                );
            }
            Node::Branch(children) => {
                let span = self.levels[level - 1].span;
                let mut start = 0;
                while start < pages.len() {
                    let child = pages[start] / span;
                    let end = start + pages[start..].partition_point(|page| page / span == child);
                    self.collect_counts(
                        backend,
                        level - 1,
                        child,
                        children[(child % FANOUT) as usize],
                        &pages[start..end],
                        out,
                    )?;
                    start = end;
                }
            }
        }
        Ok(())
    }

    /// Collect a bounded window of free pages in ascending order. A whole
    /// empty subtree costs no body I/O, and a full subtree can be skipped.
    pub fn free_window(
        &self,
        backend: &dyn StorageBackend,
        root: AllocatorRoot,
        from: u64,
        limit: usize,
    ) -> Result<Vec<u64>> {
        let mut result = Vec::new();
        if from < self.pages && limit > 0 {
            self.collect_free(backend, self.top(), 0, root.0, from, limit, &mut result)?;
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_free(
        &self,
        backend: &dyn StorageBackend,
        level: usize,
        index: u64,
        reference: Option<Ref>,
        from: u64,
        limit: usize,
        result: &mut Vec<u64>,
    ) -> Result<()> {
        let start = index * self.levels[level].span;
        let span = self.span(level, index)?;
        if from >= start + span || result.len() >= limit {
            return Ok(());
        }
        let Some(reference) = reference else {
            result.extend((from.max(start)..start + span).take(limit - result.len()));
            return Ok(());
        };
        let node = self.read_node(backend, level, index, Some(reference))?;
        if reference.free == 0 {
            return Ok(());
        }
        match node {
            Node::Leaf(counts) => {
                result.extend(
                    counts[..span as usize]
                        .iter()
                        .enumerate()
                        .skip(from.saturating_sub(start) as usize)
                        .filter(|(_, count)| **count == 0)
                        .map(|(slot, _)| start + slot as u64)
                        .take(limit - result.len()),
                );
            }
            Node::Branch(children) => {
                for (i, child) in children.into_iter().enumerate() {
                    let id = index * FANOUT + i as u64;
                    if id >= self.levels[level - 1].nodes || result.len() == limit {
                        break;
                    }
                    if child.is_some_and(|reference| reference.free == 0) {
                        continue;
                    }
                    self.collect_free(backend, level - 1, id, child, from, limit, result)?;
                }
            }
        }
        Ok(())
    }

    /// Find the first zero-reference page at or after a cursor, skipping
    /// full subtrees using authenticated counts. No free-page enumeration.
    pub fn next_free(
        &self,
        backend: &dyn StorageBackend,
        root: AllocatorRoot,
        from: u64,
    ) -> Result<Option<u64>> {
        Ok(self.free_window(backend, root, from, 1)?.pop())
    }

    /// Plan reference-count replacements against the chosen root. The other
    /// retained root must be supplied even if it is not the current winner.
    /// Neither old root is overwritten during the body phase.
    pub fn prepare(
        &self,
        backend: &dyn StorageBackend,
        base: AllocatorRoot,
        retained: AllocatorRoot,
        changes: &BTreeMap<u64, u32>,
    ) -> Result<PreparedAllocation> {
        if changes
            .keys()
            .next_back()
            .is_some_and(|page| *page >= self.pages)
        {
            return Err(ArgosError::Invalid("allocator edit outside region".into()));
        }
        let entries: Vec<_> = changes.iter().map(|(&page, &refs)| (page, refs)).collect();
        let mut plan = PreparedAllocation {
            arena: self.arena.clone(),
            pages: self.pages,
            root: base,
            writes: BTreeMap::new(),
        };
        if !entries.is_empty() {
            plan.root = AllocatorRoot(self.edit(
                backend,
                self.top(),
                0,
                [base.0, retained.0],
                &entries,
                &mut plan.writes,
            )?);
        }
        Ok(plan)
    }

    fn edit(
        &self,
        backend: &dyn StorageBackend,
        level: usize,
        index: u64,
        roots: [Option<Ref>; 2],
        changes: &[(u64, u32)],
        writes: &mut BTreeMap<u64, Bytes>,
    ) -> Result<Option<Ref>> {
        if changes.is_empty() {
            return Ok(roots[0]);
        }
        let old = self.read_node(backend, level, index, roots[0])?;
        let retained = if roots[0] == roots[1] {
            old.clone()
        } else {
            self.read_node(backend, level, index, roots[1])?
        };
        let mut new = old.clone();
        match (&mut new, &retained) {
            (Node::Leaf(counts), Node::Leaf(_)) => {
                for &(page, refs) in changes {
                    counts[(page % LEAF) as usize] = refs;
                }
            }
            (Node::Branch(children), Node::Branch(retained)) => {
                let span = self.levels[level - 1].span;
                let mut start = 0;
                while start < changes.len() {
                    let child = changes[start].0 / span;
                    let slot = (child % FANOUT) as usize;
                    let end =
                        start + changes[start..].partition_point(|(page, _)| page / span == child);
                    children[slot] = self.edit(
                        backend,
                        level - 1,
                        child,
                        [children[slot], retained[slot]],
                        &changes[start..end],
                        writes,
                    )?;
                    start = end;
                }
            }
            _ => return Err(corrupt("incompatible allocator nodes")),
        }
        if new == old {
            return Ok(roots[0]);
        }
        if self.free_count(level, index, &new)? == self.span(level, index)? {
            return Ok(None);
        }
        let slot = (0..3u8)
            .find(|slot| !roots.iter().flatten().any(|r| r.slot == *slot))
            .ok_or_else(|| corrupt("no safe allocator COW slot"))?;
        let (reference, bytes) = self.encode_node(level, index, slot, &new)?;
        if writes
            .insert(self.address(level, index, slot)?, bytes)
            .is_some()
        {
            return Err(corrupt("duplicate staged allocator page"));
        }
        Ok(Some(reference))
    }

    /// Persist the prepared body and verify readback. The caller MUST then
    /// publish the matching logical and allocator roots atomically. A crash
    /// before root publication leaves both retained maps intact.
    pub fn write_prepared(
        &self,
        backend: &dyn StorageBackend,
        plan: &PreparedAllocation,
    ) -> Result<()> {
        if self.arena != plan.arena || self.pages != plan.pages {
            return Err(ArgosError::Invalid(
                "allocator plan geometry changed".into(),
            ));
        }
        for (&offset, bytes) in &plan.writes {
            if offset < self.arena.offset || offset >= self.arena.offset + self.required {
                return Err(corrupt("allocator staged page outside reserved area"));
            }
            backend.write_at(&self.arena.disk_id, offset, bytes)?;
        }
        backend.flush_device(&self.arena.disk_id)?;
        for (&offset, expected) in &plan.writes {
            let mut actual = [0; PAGE];
            backend.read_at(&self.arena.disk_id, offset, &mut actual)?;
            if &actual != expected {
                return Err(ArgosError::Checksum(
                    "physical allocator readback mismatch".into(),
                ));
            }
        }
        Ok(())
    }
}

pub struct PreparedAllocation {
    arena: Arena,
    pages: u64,
    root: AllocatorRoot,
    writes: BTreeMap<u64, Bytes>,
}

impl PreparedAllocation {
    pub fn root(&self) -> AllocatorRoot {
        self.root
    }
    pub fn pages_written(&self) -> usize {
        self.writes.len()
    }
}

/// Walk allocated entries for exhaustive recovery/audit only.
impl AllocationMap {
    pub fn allocated_entries(
        &self,
        backend: &dyn StorageBackend,
        root: AllocatorRoot,
    ) -> Result<BTreeMap<u64, u32>> {
        let mut out = BTreeMap::new();
        let mut pending = vec![(self.top(), 0u64, root.0)];
        while let Some((level, index, reference)) = pending.pop() {
            let Some(reference) = reference else {
                continue;
            };
            match self.read_node(backend, level, index, Some(reference))? {
                Node::Leaf(counts) => {
                    for (slot, refs) in counts
                        .into_iter()
                        .enumerate()
                        .filter(|(_, refs)| *refs != 0)
                    {
                        out.insert(index * LEAF + slot as u64, refs);
                    }
                }
                Node::Branch(children) => {
                    pending.extend(
                        children
                            .into_iter()
                            .enumerate()
                            .filter(|(_, child)| child.is_some())
                            .map(|(slot, child)| (level - 1, index * FANOUT + slot as u64, child)),
                    );
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
