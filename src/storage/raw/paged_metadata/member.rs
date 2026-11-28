use super::wire::*;
use super::{Arena, Changes, CommitIo};
use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug)]
struct GraphPage {
    ptr: Ptr,
    children: Vec<Ptr>,
    refs: usize,
}

type Graph = BTreeMap<u64, GraphPage>;

impl Arena {
    pub(super) fn pages(&self) -> u64 {
        self.length / PAGE_SIZE as u64
    }

    pub(super) fn validate(&self, backend: &dyn StorageBackend) -> Result<()> {
        let capacity = backend.capacity(&self.disk_id)?;
        if !self.offset.is_multiple_of(PAGE_SIZE as u64)
            || !self.length.is_multiple_of(PAGE_SIZE as u64)
            || self.pages() < RESERVED_PAGES + 2
            || self
                .offset
                .checked_add(self.length)
                .is_none_or(|end| end > capacity)
        {
            return Err(ArgosError::Invalid(format!(
                "invalid paged metadata arena on {}",
                self.disk_id
            )));
        }
        Ok(())
    }

    fn offset_of(&self, page: u64) -> Result<u64> {
        if page >= self.pages() {
            return Err(corrupt("metadata page outside arena"));
        }
        Ok(self.offset + page * PAGE_SIZE as u64)
    }

    pub(super) fn read(&self, backend: &dyn StorageBackend, page: u64) -> Result<Bytes> {
        let mut bytes = [0; PAGE_SIZE];
        backend.read_at(&self.disk_id, self.offset_of(page)?, &mut bytes)?;
        Ok(bytes)
    }

    pub(super) fn write(
        &self,
        backend: &dyn StorageBackend,
        page: u64,
        bytes: &Bytes,
    ) -> Result<()> {
        backend.write_at(&self.disk_id, self.offset_of(page)?, bytes)
    }

    fn read_node(&self, backend: &dyn StorageBackend, ptr: Ptr) -> Result<Node> {
        Node::decode(&self.read(backend, ptr.page)?, ptr.hash)
    }
}

// Cache only authenticated structural information, not record payloads.
// Consecutive COW roots share most physical pages. Reusing their verified
// descriptors avoids a second device read and hash without treating an
// unverified pointer as evidence or retaining full metadata images in RAM.
enum ScanNode {
    Leaf {
        range: (u64, u64),
        blobs: Vec<(Ptr, usize)>,
    },
    Branch {
        level: u8,
        children: Vec<(u64, Ptr)>,
    },
    Blob {
        next: Option<Ptr>,
        len: usize,
    },
}

impl ScanNode {
    fn from_node(node: Node) -> Self {
        match node {
            Node::Leaf(entries) => {
                let range = (entries.first().unwrap().0, entries.last().unwrap().0);
                let blobs = entries
                    .into_iter()
                    .filter_map(|(_, value)| match value {
                        Value::Blob { first, len } => Some((first, len)),
                        Value::Inline(_) => None,
                    })
                    .collect();
                Self::Leaf { range, blobs }
            }
            Node::Branch { level, children } => Self::Branch { level, children },
            Node::Blob { next, data } => Self::Blob {
                next,
                len: data.len(),
            },
        }
    }

    fn children(&self) -> Vec<Ptr> {
        match self {
            Self::Leaf { blobs, .. } => blobs.iter().map(|(ptr, _)| *ptr).collect(),
            Self::Branch { children, .. } => children.iter().map(|(_, ptr)| *ptr).collect(),
            Self::Blob { next, .. } => next.iter().copied().collect(),
        }
    }

    fn level(&self) -> Option<u8> {
        match self {
            Self::Leaf { .. } => Some(0),
            Self::Branch { level, .. } => Some(*level),
            Self::Blob { .. } => None,
        }
    }
}

type VerifiedPages = BTreeMap<u64, (Ptr, Arc<ScanNode>)>;

struct Scanner<'a> {
    arena: &'a Arena,
    backend: &'a dyn StorageBackend,
    verified: &'a mut VerifiedPages,
    graph: Graph,
}

impl Scanner<'_> {
    fn visit(&mut self, ptr: Ptr) -> Result<Arc<ScanNode>> {
        if self.graph.contains_key(&ptr.page) {
            return Err(corrupt("metadata root contains a cycle or duplicate page"));
        }
        let node = if let Some((authenticated, node)) = self.verified.get(&ptr.page) {
            if *authenticated != ptr {
                return Err(corrupt(
                    "metadata page reference disagrees with authenticated identity",
                ));
            }
            Arc::clone(node)
        } else {
            let node = Arc::new(ScanNode::from_node(
                self.arena.read_node(self.backend, ptr)?,
            ));
            self.verified.insert(ptr.page, (ptr, Arc::clone(&node)));
            node
        };
        self.graph.insert(
            ptr.page,
            GraphPage {
                ptr,
                children: node.children(),
                refs: 0,
            },
        );
        Ok(node)
    }

    /// The page tree's physical order need not match key order. Group
    /// adjacent authenticated child pointers into bounded device reads.
    /// No speculative free pages are read. If a bulk read fails, let normal
    /// per-page traversal report the error for the actual referenced page.
    fn prefetch(&mut self, pointers: impl Iterator<Item = Ptr>) {
        const MAX_PAGES: usize = 64;
        let mut pending = pointers
            .filter(|ptr| {
                ptr.page >= RESERVED_PAGES
                    && ptr.page < self.arena.pages()
                    && !self.verified.contains_key(&ptr.page)
            })
            .collect::<Vec<_>>();
        pending.sort_unstable_by_key(|ptr| ptr.page);
        pending.dedup_by_key(|ptr| ptr.page);
        let mut start = 0;
        while start < pending.len() {
            let mut end = start + 1;
            while end < pending.len()
                && end - start < MAX_PAGES
                && pending[end].page == pending[end - 1].page + 1
            {
                end += 1;
            }
            let batch = &pending[start..end];
            start = end;
            if batch.len() < 2 {
                continue;
            }
            let Ok(offset) = self.arena.offset_of(batch[0].page) else {
                continue;
            };
            let mut data = vec![0; batch.len() * PAGE_SIZE];
            if self
                .backend
                .read_at(&self.arena.disk_id, offset, &mut data)
                .is_err()
            {
                continue;
            }
            for (&ptr, bytes) in batch.iter().zip(data.chunks_exact(PAGE_SIZE)) {
                let bytes: &Bytes = bytes.try_into().expect("exact page");
                if let Ok(node) = Node::decode(bytes, ptr.hash) {
                    self.verified
                        .insert(ptr.page, (ptr, Arc::new(ScanNode::from_node(node))));
                }
            }
        }
    }

    fn blob(&mut self, mut ptr: Ptr, mut remaining: usize) -> Result<()> {
        loop {
            let node = self.visit(ptr)?;
            let ScanNode::Blob { next, len } = &*node else {
                return Err(corrupt("metadata overflow points to a tree page"));
            };
            remaining = remaining
                .checked_sub(*len)
                .ok_or_else(|| corrupt("metadata overflow exceeds declared length"))?;
            match (*next, remaining) {
                (None, 0) => return Ok(()),
                (Some(p), n) if n > 0 => ptr = p,
                _ => return Err(corrupt("metadata overflow length mismatch")),
            }
        }
    }

    // Every root still has independent cycle, key-range and overflow checks.
    fn tree(&mut self, ptr: Ptr, expected_level: Option<u8>) -> Result<(u64, u64)> {
        let node = self.visit(ptr)?;
        if expected_level.is_some() && node.level() != expected_level {
            return Err(corrupt("metadata tree height mismatch"));
        }
        match &*node {
            ScanNode::Leaf { range, blobs } => {
                self.prefetch(blobs.iter().map(|(ptr, _)| *ptr));
                for &(first, len) in blobs {
                    self.blob(first, len)?;
                }
                Ok(*range)
            }
            ScanNode::Branch { level, children } => {
                self.prefetch(children.iter().map(|(_, ptr)| *ptr));
                let mut first = None;
                let mut last = None;
                for &(key, child) in children {
                    let (min, max) = self.tree(child, Some(level - 1))?;
                    if max != key || last.is_some_and(|previous| previous >= min) {
                        return Err(corrupt("metadata branch key range mismatch"));
                    }
                    first.get_or_insert(min);
                    last = Some(max);
                }
                Ok((first.unwrap(), last.unwrap()))
            }
            ScanNode::Blob { .. } => Err(corrupt("metadata tree points to an overflow page")),
        }
    }
}

/// Sorted, disjoint free page ranges, with exclusive end offsets. A large
/// mostly empty arena must not allocate one B-tree entry per unused page.
#[derive(Default)]
struct FreePages(BTreeMap<u64, u64>);

impl FreePages {
    fn from_graph(pages: u64, graph: &Graph) -> Self {
        let mut free = Self::default();
        let mut start = RESERVED_PAGES;
        for &page in graph.keys() {
            if start < page {
                free.0.insert(start, page);
            }
            start = page + 1;
        }
        if start < pages {
            free.0.insert(start, pages);
        }
        free
    }

    fn contains(&self, page: &u64) -> bool {
        self.0
            .range(..=page)
            .next_back()
            .is_some_and(|(_, end)| page < end)
    }

    fn remove(&mut self, page: &u64) {
        let Some((&start, &end)) = self.0.range(..=page).next_back() else {
            return;
        };
        if *page >= end {
            return;
        }
        self.0.remove(&start);
        if start < *page {
            self.0.insert(start, *page);
        }
        if page + 1 < end {
            self.0.insert(page + 1, end);
        }
    }

    fn insert(&mut self, page: u64) {
        if self.contains(&page) {
            return;
        }
        let mut start = page;
        let mut end = page + 1;
        if let Some((&left, &left_end)) = self.0.range(..page).next_back() {
            if left_end == page {
                start = left;
                self.0.remove(&left);
            }
        }
        if let Some(right_end) = self.0.remove(&end) {
            end = right_end;
        }
        self.0.insert(start, end);
    }

    fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().flat_map(|(&start, &end)| start..end)
    }
}

pub(super) struct Member {
    pub(super) arena: Arena,
    pub(super) roots: [Option<Root>; 2],
    pub(super) certificates: [Option<Root>; 2],
    graph: Graph,
    free: FreePages,
}

impl Member {
    pub(super) fn open(
        backend: &dyn StorageBackend,
        arena: Arena,
        namespace: Hash,
    ) -> Result<Self> {
        arena.validate(backend)?;
        let mut roots = [None; 2];
        let mut certificates = [None; 2];
        let mut graph = Graph::new();
        let mut verified = VerifiedPages::new();
        for (slot, root) in roots.iter_mut().enumerate() {
            // A read error is not proof that a root is invalid. Exclude the
            // whole member rather than recycling potentially live pages.
            let certificate = arena.read(backend, slot as u64 + 2)?;
            let identity = super::member_identity(namespace, &arena.disk_id);
            certificates[slot] = Root::decode(&certificate, identity, arena.pages(), true).ok();
            let bytes = arena.read(backend, slot as u64)?;
            let Ok(candidate) = Root::decode(&bytes, identity, arena.pages(), false) else {
                continue;
            };
            let mut scanner = Scanner {
                arena: &arena,
                backend,
                verified: &mut verified,
                graph: Graph::new(),
            };
            if let Some(ptr) = candidate.tree {
                match scanner.tree(ptr, None) {
                    Ok(_) => {}
                    Err(ArgosError::Checksum(_) | ArgosError::CorruptedMetadata(_)) => continue,
                    Err(err) => return Err(err),
                }
            }
            for (id, entry) in scanner.graph {
                if let Some(previous) = graph.get(&id) {
                    if previous.ptr != entry.ptr {
                        return Err(corrupt("retained metadata roots disagree on page identity"));
                    }
                } else {
                    graph.insert(id, entry);
                }
            }
            *root = Some(candidate);
        }
        // Each physical page contributes its outgoing references exactly
        // once even when both roots share it.
        let edges = graph
            .values()
            .flat_map(|entry| entry.children.iter().copied())
            .chain(roots.iter().flatten().filter_map(|root| root.tree))
            .collect::<Vec<_>>();
        for ptr in edges {
            let page = graph
                .get_mut(&ptr.page)
                .ok_or_else(|| corrupt("metadata graph has missing reference"))?;
            if page.ptr != ptr {
                return Err(corrupt("metadata graph reference hash mismatch"));
            }
            page.refs += 1;
        }
        let free = FreePages::from_graph(arena.pages(), &graph);
        Ok(Self {
            arena,
            roots,
            certificates,
            graph,
            free,
        })
    }

    pub(super) fn is_certified(&self, slot: usize) -> bool {
        self.roots[slot].is_some() && self.roots[slot] == self.certificates[slot]
    }

    pub(super) fn root_slot(&self, identity: (u64, Hash)) -> Option<usize> {
        let matches = |slot: usize| self.roots[slot].is_some_and(|r| r.identity() == identity);
        (0..2)
            .find(|&slot| matches(slot) && self.is_certified(slot))
            .or_else(|| (0..2).find(|&slot| matches(slot)))
    }

    pub(super) fn prepare_repair(
        &self,
        backend: &dyn StorageBackend,
        source: &Member,
        source_slot: usize,
    ) -> Result<Prepared> {
        let root = source.roots[source_slot].ok_or_else(|| corrupt("missing repair source"))?;
        // Keep the best verified local root while the new graph is copied.
        let keep = (0..2)
            .filter(|&slot| self.roots[slot].is_some())
            .max_by_key(|&slot| (self.is_certified(slot), self.roots[slot].unwrap().txid))
            .unwrap_or(1);
        let mut plan = Plan {
            member: self,
            backend,
            free: Box::new(self.free.iter()),
            recycled: Vec::new(),
            pages: BTreeMap::new(),
            reads: 0,
        };
        let tree = root
            .tree
            .map(|ptr| plan.copy_tree(source, ptr))
            .transpose()?;
        let repaired = Root {
            txid: root.txid,
            tree,
        };
        if repaired.identity() != root.identity() {
            return Err(corrupt("repair changed the logical metadata root"));
        }
        let prepared = Prepared {
            slot: 1 - keep,
            root: repaired,
            pages: plan.pages,
            reads: plan.reads,
        };
        self.validate_plan(&prepared)?;
        Ok(prepared)
    }

    pub(super) fn prepare(
        &self,
        backend: &dyn StorageBackend,
        slot: usize,
        txid: u64,
        changes: &Changes,
    ) -> Result<Prepared> {
        let root = self.roots[slot].ok_or_else(|| corrupt("missing base root"))?;
        let mut plan = Plan {
            member: self,
            backend,
            free: Box::new(self.free.iter()),
            recycled: Vec::new(),
            pages: BTreeMap::new(),
            reads: 0,
        };
        let tree = if root.tree.is_none() {
            plan.build_empty(changes)?
        } else {
            let mut tree = root.tree;
            for (&key, value) in changes {
                tree = plan.update_root(tree, key, value.as_deref())?;
            }
            tree
        };
        let prepared = Prepared {
            slot: 1 - slot,
            root: Root { txid, tree },
            pages: plan.pages,
            reads: plan.reads,
        };
        self.validate_plan(&prepared)?;
        Ok(prepared)
    }

    fn validate_plan(&self, plan: &Prepared) -> Result<()> {
        let mut incoming = BTreeMap::new();
        for (&id, page) in &plan.pages {
            if !self.free.contains(&id) || page.ptr.page != id {
                return Err(corrupt("metadata plan attempted to overwrite a live page"));
            }
            incoming.insert(id, 0usize);
        }
        for child in plan
            .pages
            .values()
            .flat_map(|page| &page.children)
            .copied()
            .chain(plan.root.tree)
        {
            if let Some(page) = plan.pages.get(&child.page) {
                if page.ptr != child {
                    return Err(corrupt("metadata plan has stale staged reference"));
                }
                *incoming.get_mut(&child.page).unwrap() += 1;
            } else if self
                .graph
                .get(&child.page)
                .is_none_or(|page| page.ptr != child)
            {
                return Err(corrupt("metadata plan has missing live reference"));
            }
        }
        if incoming.values().any(|refs| *refs != 1) {
            return Err(corrupt(
                "metadata plan contains unreachable or shared new pages",
            ));
        }
        Ok(())
    }

    pub(super) fn write_body(&self, backend: &dyn StorageBackend, plan: &Prepared) -> Result<()> {
        // Write neighboring new pages together, never covering a live page.
        const MAX_BATCH: usize = 256 * PAGE_SIZE;
        let mut start = 0;
        let mut batch = Vec::new();
        for (&id, page) in &plan.pages {
            if !batch.is_empty()
                && (id != start + (batch.len() / PAGE_SIZE) as u64 || batch.len() == MAX_BATCH)
            {
                backend.write_at(&self.arena.disk_id, self.arena.offset_of(start)?, &batch)?;
                batch.clear();
            }
            if batch.is_empty() {
                start = id;
            }
            batch.extend_from_slice(&page.bytes);
        }
        if !batch.is_empty() {
            backend.write_at(&self.arena.disk_id, self.arena.offset_of(start)?, &batch)?;
        }
        backend.flush_device(&self.arena.disk_id)?;
        // Verify only the newly written pages, coalesced in the same
        // contiguous batches. This catches silent discarded/corrupted
        // writes before they can become part of a certified root.
        let mut batch = Vec::with_capacity(MAX_BATCH);
        let mut start = 0;
        let verify = |start: u64, expected: &[u8]| -> Result<()> {
            let mut actual = vec![0; expected.len()];
            backend.read_at(
                &self.arena.disk_id,
                self.arena.offset_of(start)?,
                &mut actual,
            )?;
            if actual != expected {
                return Err(ArgosError::Checksum(format!(
                    "metadata body readback mismatch on {}",
                    self.arena.disk_id
                )));
            }
            Ok(())
        };
        for (&id, page) in &plan.pages {
            if !batch.is_empty()
                && (id != start + (batch.len() / PAGE_SIZE) as u64 || batch.len() == MAX_BATCH)
            {
                verify(start, &batch)?;
                batch.clear();
            }
            if batch.is_empty() {
                start = id;
            }
            batch.extend_from_slice(&page.bytes);
        }
        if !batch.is_empty() {
            verify(start, &batch)?;
        }
        Ok(())
    }

    pub(super) fn publish(
        &mut self,
        backend: &dyn StorageBackend,
        namespace: Hash,
        plan: Prepared,
    ) -> Result<CommitIo> {
        let bytes = plan.root.encode(
            super::member_identity(namespace, &self.arena.disk_id),
            self.arena.pages(),
            false,
        );
        self.arena.write(backend, plan.slot as u64, &bytes)?;
        backend.flush_device(&self.arena.disk_id)?;
        if self.arena.read(backend, plan.slot as u64)? != bytes {
            return Err(ArgosError::Checksum(format!(
                "metadata root readback mismatch on {}",
                self.arena.disk_id
            )));
        }
        let io = CommitIo {
            pages_read: plan.reads + plan.pages.len() + 1,
            pages_written: plan.pages.len() + 1,
        };
        // Only after the replacement root is durable can the retired root
        // release references. All pages reachable from the other root remain
        // allocated, including those shared by both trees.
        for (id, page) in plan.pages {
            self.free.remove(&id);
            self.graph.insert(
                id,
                GraphPage {
                    ptr: page.ptr,
                    children: page.children,
                    refs: 0,
                },
            );
        }
        // Walk only newly allocated pages; existing pages already own their
        // outgoing edges. Increment before releasing the overwritten root.
        let mut pending = plan.root.tree.into_iter().collect::<Vec<_>>();
        while let Some(ptr) = pending.pop() {
            let page = self
                .graph
                .get_mut(&ptr.page)
                .expect("validated metadata graph");
            if page.refs == 0 {
                pending.extend(page.children.iter().copied());
            }
            page.refs += 1;
        }
        if let Some(old) = self.roots[plan.slot]
            .replace(plan.root)
            .and_then(|root| root.tree)
        {
            let mut pending = vec![old];
            while let Some(ptr) = pending.pop() {
                let page = self
                    .graph
                    .get_mut(&ptr.page)
                    .expect("validated metadata graph");
                page.refs -= 1;
                if page.refs == 0 {
                    let page = self.graph.remove(&ptr.page).unwrap();
                    pending.extend(page.children);
                    self.free.insert(ptr.page);
                }
            }
        }
        Ok(io)
    }

    pub(super) fn certify(
        &mut self,
        backend: &dyn StorageBackend,
        namespace: Hash,
        identity: (u64, Hash),
    ) -> Result<()> {
        let slot = self
            .root_slot(identity)
            .ok_or_else(|| corrupt("cannot certify missing root"))?;
        let root = self.roots[slot].unwrap();
        let bytes = root.encode(
            super::member_identity(namespace, &self.arena.disk_id),
            self.arena.pages(),
            true,
        );
        self.arena.write(backend, slot as u64 + 2, &bytes)?;
        backend.flush_device(&self.arena.disk_id)?;
        if self.arena.read(backend, slot as u64 + 2)? != bytes {
            return Err(ArgosError::Checksum(format!(
                "metadata certificate readback mismatch on {}",
                self.arena.disk_id
            )));
        }
        self.certificates[slot] = Some(root);
        Ok(())
    }

    fn read_value(&self, backend: &dyn StorageBackend, value: Value) -> Result<Vec<u8>> {
        match value {
            Value::Inline(bytes) => Ok(bytes),
            Value::Blob { len, first } => {
                // Never allocate more than the arena could physically hold.
                if len as u64 > self.arena.length {
                    return Err(corrupt("metadata blob exceeds arena size"));
                }
                let mut out = Vec::new();
                let mut next = Some(first);
                while let Some(ptr) = next {
                    let Node::Blob {
                        next: following,
                        data,
                    } = self.arena.read_node(backend, ptr)?
                    else {
                        return Err(corrupt("metadata overflow points to tree page"));
                    };
                    if out.len() + data.len() > len {
                        return Err(corrupt("metadata overflow length mismatch"));
                    }
                    out.extend_from_slice(&data);
                    next = following;
                }
                if out.len() != len {
                    return Err(corrupt("metadata overflow length mismatch"));
                }
                Ok(out)
            }
        }
    }

    pub(super) fn get(
        &self,
        backend: &dyn StorageBackend,
        slot: usize,
        key: u64,
    ) -> Result<Option<Vec<u8>>> {
        let mut next = self.roots[slot].and_then(|root| root.tree);
        while let Some(ptr) = next {
            match self.arena.read_node(backend, ptr)? {
                Node::Branch { children, .. } => {
                    next = children
                        .get(children.partition_point(|(max, _)| *max < key))
                        .map(|(_, ptr)| *ptr);
                }
                Node::Leaf(mut entries) => {
                    return entries
                        .binary_search_by_key(&key, |(key, _)| *key)
                        .ok()
                        .map(|index| self.read_value(backend, entries.swap_remove(index).1))
                        .transpose();
                }
                Node::Blob { .. } => {
                    return Err(corrupt("metadata lookup encountered overflow page"))
                }
            }
        }
        Ok(None)
    }

    pub(super) fn records(
        &self,
        backend: &dyn StorageBackend,
        slot: usize,
    ) -> Result<BTreeMap<u64, Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut pending = self.roots[slot]
            .and_then(|root| root.tree)
            .into_iter()
            .collect::<Vec<_>>();
        while let Some(ptr) = pending.pop() {
            match self.arena.read_node(backend, ptr)? {
                Node::Branch { children, .. } => {
                    pending.extend(children.into_iter().rev().map(|(_, ptr)| ptr))
                }
                Node::Leaf(entries) => {
                    for (key, value) in entries {
                        if out.insert(key, self.read_value(backend, value)?).is_some() {
                            return Err(corrupt("duplicate metadata key"));
                        }
                    }
                }
                Node::Blob { .. } => {
                    return Err(corrupt("metadata traversal encountered overflow page"))
                }
            }
        }
        Ok(out)
    }
}

struct Staged {
    ptr: Ptr,
    bytes: Bytes,
    children: Vec<Ptr>,
}

pub(super) struct Prepared {
    slot: usize,
    pub(super) root: Root,
    pages: BTreeMap<u64, Staged>,
    reads: usize,
}

struct Plan<'a> {
    member: &'a Member,
    backend: &'a dyn StorageBackend,
    free: Box<dyn Iterator<Item = u64> + 'a>,
    recycled: Vec<u64>,
    pages: BTreeMap<u64, Staged>,
    reads: usize,
}

impl Plan<'_> {
    fn read(&mut self, ptr: Ptr) -> Result<Node> {
        if let Some(page) = self.pages.get(&ptr.page) {
            return Node::decode(&page.bytes, ptr.hash);
        }
        self.reads += 1;
        self.member.arena.read_node(self.backend, ptr)
    }

    fn alloc(&mut self) -> Result<u64> {
        self.recycled
            .pop()
            .or_else(|| self.free.next())
            .ok_or_else(|| ArgosError::DiskFull {
                disk_id: self.member.arena.disk_id.clone(),
                required: PAGE_SIZE as u64,
                available: 0,
            })
    }

    fn store(&mut self, node: Node, replace: Option<Ptr>) -> Result<Ptr> {
        let id = match replace.filter(|ptr| self.pages.contains_key(&ptr.page)) {
            Some(ptr) => ptr.page,
            None => self.alloc()?,
        };
        let (bytes, hash) = node.encode()?;
        let ptr = Ptr { page: id, hash };
        self.pages.insert(
            id,
            Staged {
                ptr,
                bytes,
                children: node.children(),
            },
        );
        Ok(ptr)
    }

    fn release_staged(&mut self, ptr: Ptr) {
        if self.pages.remove(&ptr.page).is_some() {
            self.recycled.push(ptr.page);
        }
    }

    fn value(&mut self, bytes: &[u8]) -> Result<Value> {
        if bytes.len() <= INLINE_MAX {
            return Ok(Value::Inline(bytes.to_vec()));
        }
        if bytes.len() > MAX_VALUE {
            return Err(ArgosError::FileTooLarge("paged metadata record".into()));
        }
        let mut next = None;
        for chunk in bytes.rchunks(BLOB_MAX) {
            next = Some(self.store(
                Node::Blob {
                    next,
                    data: chunk.to_vec(),
                },
                None,
            )?);
        }
        Ok(Value::Blob {
            len: bytes.len(),
            first: next.unwrap(),
        })
    }

    fn store_tree(&mut self, node: Node, old: Option<Ptr>) -> Result<Vec<(u64, Ptr)>> {
        if node.max_key().is_none() {
            if let Some(old) = old {
                self.release_staged(old);
            }
            return Ok(Vec::new());
        }
        if node.fits() {
            return Ok(vec![(node.max_key().unwrap(), self.store(node, old)?)]);
        }
        let (left, right) = node.split()?;
        Ok(vec![
            (left.max_key().unwrap(), self.store(left, old)?),
            (right.max_key().unwrap(), self.store(right, None)?),
        ])
    }

    // Merge or redistribute underfull siblings on deletion. Only the local
    // neighbors of a changed path are touched; no global compaction pass.
    fn rebalance(&mut self, children: &mut Vec<(u64, Ptr)>, index: usize) -> Result<()> {
        if children.len() < 2 {
            return Ok(());
        }
        let index = index.min(children.len() - 1);
        let node = self.read(children[index].1)?;
        if node.used() >= NODE_CAPACITY / 3 {
            return Ok(());
        }
        let left = if index == 0 { 0 } else { index - 1 };
        let right = left + 1;
        let combined = if index == left {
            node.combine(self.read(children[right].1)?)?
        } else {
            self.read(children[left].1)?.combine(node)?
        };
        self.release_staged(children[left].1);
        self.release_staged(children[right].1);
        let replacement = self.store_tree(combined, None)?;
        children.splice(left..=right, replacement);
        Ok(())
    }

    fn update(&mut self, ptr: Ptr, key: u64, value: Option<&[u8]>) -> Result<Vec<(u64, Ptr)>> {
        match self.read(ptr)? {
            Node::Leaf(mut entries) => {
                match (entries.binary_search_by_key(&key, |(key, _)| *key), value) {
                    (Ok(index), Some(bytes)) => {
                        // Changes are unique per transaction, so no replaced
                        // overflow chain can have been allocated by this plan.
                        if matches!(&entries[index].1, Value::Inline(old) if old == bytes) {
                            return Ok(vec![(entries.last().unwrap().0, ptr)]);
                        }
                        entries[index].1 = self.value(bytes)?;
                    }
                    (Ok(index), None) => {
                        entries.remove(index);
                    }
                    (Err(index), Some(bytes)) => entries.insert(index, (key, self.value(bytes)?)),
                    (Err(_), None) => return Ok(vec![(entries.last().unwrap().0, ptr)]),
                }
                self.store_tree(Node::Leaf(entries), Some(ptr))
            }
            Node::Branch {
                level,
                mut children,
            } => {
                let index = children
                    .partition_point(|(max, _)| *max < key)
                    .min(children.len() - 1);
                let old = children[index];
                let replacement = self.update(old.1, key, value)?;
                if replacement.as_slice() == [old] {
                    return Ok(vec![(children.last().unwrap().0, ptr)]);
                }
                children.splice(index..index + 1, replacement);
                if value.is_none() {
                    self.rebalance(&mut children, index)?;
                }
                self.store_tree(Node::Branch { level, children }, Some(ptr))
            }
            Node::Blob { .. } => Err(corrupt("metadata update encountered overflow page")),
        }
    }

    // Copy the exact authenticated tree shape rather than rebuilding from
    // keys (which could change the logical digest). An explicit postorder
    // stack also handles long overflow chains without recursive stack use.
    fn copy_tree(&mut self, source: &Member, root: Ptr) -> Result<Ptr> {
        let mut pending = vec![(root, false)];
        let mut translated = BTreeMap::<u64, Ptr>::new();
        while let Some((ptr, expanded)) = pending.pop() {
            if !expanded {
                let entry = source
                    .graph
                    .get(&ptr.page)
                    .filter(|entry| entry.ptr == ptr)
                    .ok_or_else(|| corrupt("repair source graph changed"))?;
                pending.push((ptr, true));
                pending.extend(entry.children.iter().rev().map(|&child| (child, false)));
                continue;
            }
            let mut node = source.arena.read_node(self.backend, ptr)?;
            self.reads += 1;
            let replace = |child: &mut Ptr| -> Result<()> {
                *child = translated
                    .get(&child.page)
                    .copied()
                    .filter(|new| new.hash == child.hash)
                    .ok_or_else(|| corrupt("repair child translation missing"))?;
                Ok(())
            };
            match &mut node {
                Node::Branch { children, .. } => {
                    for (_, child) in children {
                        replace(child)?;
                    }
                }
                Node::Leaf(entries) => {
                    for (_, value) in entries {
                        if let Value::Blob { first, .. } = value {
                            replace(first)?;
                        }
                    }
                }
                Node::Blob { next, .. } => {
                    if let Some(child) = next {
                        replace(child)?;
                    }
                }
            }
            let new = self.store(node, None)?;
            if new.hash != ptr.hash {
                return Err(corrupt("repair changed a logical page digest"));
            }
            translated.insert(ptr.page, new);
        }
        translated
            .get(&root.page)
            .copied()
            .ok_or_else(|| corrupt("repair did not copy root"))
    }

    // Initial import is a bottom-up packed build, not N repeated COW path
    // rewrites. Later transactions retain the ordinary bounded path update.
    fn build_empty(&mut self, changes: &Changes) -> Result<Option<Ptr>> {
        let mut leaves = Vec::new();
        let mut entries = Vec::new();
        let mut used = 0;
        for (&key, value) in changes {
            let Some(bytes) = value else { continue };
            let value = self.value(bytes)?;
            let len = value.encoded_len();
            if used + len > NODE_CAPACITY {
                leaves.extend(self.store_tree(Node::Leaf(std::mem::take(&mut entries)), None)?);
                used = 0;
            }
            entries.push((key, value));
            used += len;
        }
        if !entries.is_empty() {
            leaves.extend(self.store_tree(Node::Leaf(entries), None)?);
        }
        let mut level = 1;
        while leaves.len() > 1 {
            let mut parents = Vec::new();
            for children in leaves.chunks(NODE_CAPACITY / 48) {
                parents.extend(self.store_tree(
                    Node::Branch {
                        level,
                        children: children.to_vec(),
                    },
                    None,
                )?);
            }
            leaves = parents;
            level += 1;
        }
        Ok(leaves.first().map(|(_, ptr)| *ptr))
    }

    fn update_root(
        &mut self,
        root: Option<Ptr>,
        key: u64,
        value: Option<&[u8]>,
    ) -> Result<Option<Ptr>> {
        let Some(root) = root else {
            return match value {
                Some(bytes) => {
                    let value = self.value(bytes)?;
                    Ok(Some(self.store(Node::Leaf(vec![(key, value)]), None)?))
                }
                None => Ok(None),
            };
        };
        let level = self
            .read(root)?
            .level()
            .ok_or_else(|| corrupt("invalid metadata tree root"))?;
        let updated = self.update(root, key, value)?;
        match updated.as_slice() {
            [] => Ok(None),
            [(_, ptr)] => {
                let mut ptr = *ptr;
                while let Node::Branch { children, .. } = self.read(ptr)? {
                    if children.len() != 1 {
                        break;
                    }
                    self.release_staged(ptr);
                    ptr = children[0].1;
                }
                Ok(Some(ptr))
            }
            _ => Ok(Some(self.store(
                Node::Branch {
                    level: level + 1,
                    children: updated,
                },
                None,
            )?)),
        }
    }
}

#[cfg(test)]
mod free_page_tests {
    use super::*;

    #[test]
    fn huge_sparse_arena_uses_ranges_and_reclaims_holes() {
        let mut graph = Graph::new();
        for page in [4, 6, 999_999_999] {
            graph.insert(
                page,
                GraphPage {
                    ptr: Ptr {
                        page,
                        hash: [0; 32],
                    },
                    children: Vec::new(),
                    refs: 1,
                },
            );
        }
        let mut free = FreePages::from_graph(1_000_000_000, &graph);
        assert_eq!(free.0.len(), 2);
        assert_eq!(free.iter().take(4).collect::<Vec<_>>(), vec![5, 7, 8, 9]);
        assert!(!free.contains(&6));
        free.remove(&8);
        assert_eq!(free.0.len(), 3);
        assert!(!free.contains(&8));
        free.insert(6);
        free.insert(8);
        free.insert(4);
        free.insert(999_999_999);
        assert_eq!(free.0, BTreeMap::from([(4, 1_000_000_000)]));
    }
}
