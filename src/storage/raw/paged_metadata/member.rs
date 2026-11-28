use super::physical::{AllocationMap, AllocatorRoot, PreparedAllocation};
use super::wire::*;
use super::{Arena, CommitIo, TreeChanges};
use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;
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

    /// V3 reserves a deterministic tail for its self-contained physical
    /// allocation map. Existing numeric-key arenas retain their exact layout.
    pub(super) fn views<K: TreeKey>(
        &self,
        backend: &dyn StorageBackend,
    ) -> Result<(Arena, Option<AllocationMap>)> {
        self.validate(backend)?;
        if !K::PERSISTENT_ALLOCATOR {
            return Ok((self.clone(), None));
        }
        let total = self.pages();
        let mut low = 1u64;
        let mut high = total;
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            let needed = AllocationMap::required_bytes(mid)? / PAGE_SIZE as u64;
            if mid <= total.saturating_sub(needed) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        if low < RESERVED_PAGES + 2 {
            return Err(ArgosError::Invalid(
                "V3 arena too small for physical allocator".into(),
            ));
        }
        let mut data = self.clone();
        data.length = low * PAGE_SIZE as u64;
        let allocator = Arena {
            disk_id: self.disk_id.clone(),
            offset: self.offset + data.length,
            length: self.length - data.length,
        };
        Ok((data, Some(AllocationMap::new(backend, allocator, low)?)))
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

    fn read_node<K: TreeKey>(&self, backend: &dyn StorageBackend, ptr: Ptr) -> Result<Node<K>> {
        Node::decode(&self.read(backend, ptr.page)?, ptr.hash)
    }
    fn read_value<K: TreeKey>(
        &self,
        backend: &dyn StorageBackend,
        value: Value,
    ) -> Result<Vec<u8>> {
        match value {
            Value::Inline(bytes) => Ok(bytes),
            Value::Blob { len, first } => {
                // Never allocate more than the arena could physically hold.
                if len as u64 > self.length {
                    return Err(corrupt("metadata blob exceeds arena size"));
                }
                let mut out = Vec::new();
                let mut next = Some(first);
                while let Some(ptr) = next {
                    let Node::Blob {
                        next: following,
                        data,
                    } = self.read_node::<K>(backend, ptr)?
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

    pub(super) fn validate_tree_head<K: TreeKey>(
        &self,
        backend: &dyn StorageBackend,
        root: Root,
    ) -> Result<()> {
        if let Some(ptr) = root.tree {
            let node = self.read_node::<K>(backend, ptr)?;
            if if K::PERSISTENT_ALLOCATOR {
                !matches!(node, Node::Catalog(_))
            } else {
                node.level().is_none()
            } {
                return Err(corrupt("metadata root points to an overflow page"));
            }
        }
        Ok(())
    }

    pub(super) fn scan_window<K: TreeKey>(
        &self,
        backend: &dyn StorageBackend,
        root: Root,
        window: &super::ScanWindow<K>,
    ) -> Result<super::ScanPage<K>> {
        struct Task<K> {
            ptr: Ptr,
            level: Option<u8>,
            lower: Option<K>,
            upper: Option<K>,
            domain: Option<u16>,
            catalog: bool,
        }
        let below_start = |key: &K| match &window.start {
            Bound::Included(start) => key < start,
            Bound::Excluded(start) => key <= start,
            Bound::Unbounded => false,
        };
        let past_end = |key: &K| match &window.end {
            Bound::Included(end) => key > end,
            Bound::Excluded(end) => key >= end,
            Bound::Unbounded => false,
        };
        let descending = window.order == super::ScanOrder::Descending;
        let mut result = super::ScanPage {
            records: Vec::new(),
            more: false,
        };
        let mut bytes = 0usize;
        let mut pending = root
            .tree
            .into_iter()
            .map(|ptr| Task {
                ptr,
                level: None,
                lower: None,
                upper: None,
                domain: None,
                catalog: K::PERSISTENT_ALLOCATOR,
            })
            .collect::<Vec<_>>();
        while let Some(task) = pending.pop() {
            let node = self.read_node::<K>(backend, task.ptr)?;
            if task.catalog != matches!(node, Node::Catalog(_))
                || (!task.catalog
                    && (node.level().is_none()
                        || task.level.is_some_and(|level| node.level() != Some(level))
                        || task
                            .upper
                            .as_ref()
                            .is_some_and(|upper| node.max_key().as_ref() != Some(upper))))
            {
                return Err(corrupt("metadata scan path height or upper key mismatch"));
            }
            match node {
                Node::Catalog(roots) => {
                    let domain_start = match &window.start {
                        Bound::Included(key) | Bound::Excluded(key) => key.domain_id(),
                        Bound::Unbounded => None,
                    };
                    let domain_end = match &window.end {
                        Bound::Included(key) | Bound::Excluded(key) => key.domain_id(),
                        Bound::Unbounded => None,
                    };
                    let next = roots
                        .into_iter()
                        .filter(|(domain, _)| {
                            domain_start.is_none_or(|min| *domain >= min)
                                && domain_end.is_none_or(|max| *domain <= max)
                        })
                        .map(|(domain, ptr)| Task {
                            ptr,
                            level: None,
                            lower: None,
                            upper: None,
                            domain: Some(domain),
                            catalog: false,
                        })
                        .collect::<Vec<_>>();
                    if descending {
                        pending.extend(next);
                    } else {
                        pending.extend(next.into_iter().rev());
                    }
                }
                Node::Branch { level, children } => {
                    let mut lower = task.lower;
                    let mut next = Vec::new();
                    for (max, ptr) in children {
                        if task.domain.is_some() && max.domain_id() != task.domain {
                            return Err(corrupt("metadata branch outside catalog domain"));
                        }
                        if lower.as_ref().is_some_and(|key| key >= &max) {
                            return Err(corrupt("metadata scan overlapping branch range"));
                        }
                        if !below_start(&max) && !lower.as_ref().is_some_and(&past_end) {
                            next.push(Task {
                                ptr,
                                level: Some(level - 1),
                                lower: lower.clone(),
                                upper: Some(max.clone()),
                                domain: task.domain,
                                catalog: false,
                            });
                        }
                        lower = Some(max);
                    }
                    if descending {
                        pending.extend(next);
                    } else {
                        pending.extend(next.into_iter().rev());
                    }
                }
                Node::Leaf(mut entries) => {
                    if task.domain.is_some()
                        && entries
                            .iter()
                            .any(|(key, _)| key.domain_id() != task.domain)
                    {
                        return Err(corrupt("metadata leaf outside catalog domain"));
                    }
                    if task
                        .lower
                        .as_ref()
                        .is_some_and(|lower| &entries[0].0 <= lower)
                    {
                        return Err(corrupt("metadata scan overlapping leaf range"));
                    }
                    if descending {
                        entries.reverse();
                    }
                    for (key, value) in entries {
                        if below_start(&key) {
                            if descending {
                                return Ok(result);
                            }
                            continue;
                        }
                        if past_end(&key) {
                            if descending {
                                continue;
                            }
                            return Ok(result);
                        }
                        let len = match &value {
                            Value::Inline(value) => value.len(),
                            Value::Blob { len, .. } => *len,
                        };
                        let cost = key
                            .encoded_len()
                            .checked_add(len)
                            .ok_or_else(|| corrupt("metadata scan record size overflow"))?;
                        if result.records.len() == window.max_records
                            || cost > window.max_bytes.saturating_sub(bytes)
                        {
                            if result.records.is_empty() {
                                return Err(ArgosError::Invalid(
                                    "metadata scan byte budget smaller than next record".into(),
                                ));
                            }
                            result.more = true;
                            return Ok(result);
                        }
                        let value = self.read_value::<K>(backend, value)?;
                        bytes += cost;
                        result.records.push((key, value));
                    }
                }
                Node::Blob { .. } => unreachable!("validated tree node"),
            }
        }
        Ok(result)
    }
}

// Cache only authenticated structural information, not record payloads.
// Consecutive COW roots share most physical pages. Reusing their verified
// descriptors avoids a second device read and hash without treating an
// unverified pointer as evidence or retaining full metadata images in RAM.
enum ScanNode<K: TreeKey> {
    Catalog(Vec<(u16, Ptr)>),
    Leaf {
        range: (K, K),
        blobs: Vec<(Ptr, usize)>,
    },
    Branch {
        level: u8,
        children: Vec<(K, Ptr)>,
    },
    Blob {
        next: Option<Ptr>,
        len: usize,
    },
}

impl<K: TreeKey> ScanNode<K> {
    fn from_node(node: Node<K>) -> Self {
        match node {
            Node::Leaf(entries) => {
                let range = (
                    entries.first().unwrap().0.clone(),
                    entries.last().unwrap().0.clone(),
                );
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
            Node::Catalog(roots) => Self::Catalog(roots),
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
            Self::Catalog(roots) => roots.iter().map(|(_, ptr)| *ptr).collect(),
            Self::Blob { next, .. } => next.iter().copied().collect(),
        }
    }

    fn level(&self) -> Option<u8> {
        match self {
            Self::Leaf { .. } => Some(0),
            Self::Branch { level, .. } => Some(*level),
            Self::Blob { .. } | Self::Catalog(_) => None,
        }
    }
}

type VerifiedPages<K> = BTreeMap<u64, (Ptr, Arc<ScanNode<K>>)>;

struct Scanner<'a, K: TreeKey> {
    arena: &'a Arena,
    backend: &'a dyn StorageBackend,
    verified: &'a mut VerifiedPages<K>,
    graph: Graph,
}

impl<K: TreeKey> Scanner<'_, K> {
    fn visit(&mut self, ptr: Ptr) -> Result<Arc<ScanNode<K>>> {
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
            let (pages, remainder) = data.as_chunks::<PAGE_SIZE>();
            debug_assert!(remainder.is_empty());
            for (&ptr, bytes) in batch.iter().zip(pages) {
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

    fn root(&mut self, ptr: Ptr) -> Result<()> {
        if !K::PERSISTENT_ALLOCATOR {
            self.tree(ptr, None)?;
            return Ok(());
        }
        let node = self.visit(ptr)?;
        let ScanNode::Catalog(roots) = &*node else {
            return Err(corrupt("V3 root is not a catalog"));
        };
        self.prefetch(roots.iter().map(|(_, ptr)| *ptr));
        for (domain, ptr) in roots {
            let (min, max) = self.tree(*ptr, None)?;
            if min.domain_id() != Some(*domain) || max.domain_id() != Some(*domain) {
                return Err(corrupt("metadata tree outside catalog domain"));
            }
        }
        Ok(())
    }

    // Every root still has independent cycle, key-range and overflow checks.
    fn tree(&mut self, ptr: Ptr, expected_level: Option<u8>) -> Result<(K, K)> {
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
                Ok(range.clone())
            }
            ScanNode::Branch { level, children } => {
                self.prefetch(children.iter().map(|(_, ptr)| *ptr));
                let mut first = None;
                let mut last = None;
                for (key, child) in children {
                    let (min, max) = self.tree(*child, Some(level - 1))?;
                    if &max != key || last.as_ref().is_some_and(|previous| previous >= &min) {
                        return Err(corrupt("metadata branch key range mismatch"));
                    }
                    first.get_or_insert(min);
                    last = Some(max);
                }
                Ok((first.unwrap(), last.unwrap()))
            }
            ScanNode::Blob { .. } | ScanNode::Catalog(_) => Err(corrupt("invalid child tree node")),
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

pub(super) struct Member<K: TreeKey> {
    pub(super) arena: Arena,
    data: Arena,
    allocation: Option<AllocationMap>,
    allocator_heads_valid: [bool; 2],
    pub(super) headers_complete: bool,
    headers: [Option<Root>; 2],
    pub(super) roots: [Option<Root>; 2],
    pub(super) certificates: [Option<Root>; 2],
    graph: Graph,
    free: FreePages,
    key: std::marker::PhantomData<K>,
}

impl<K: TreeKey> Member<K> {
    pub(super) fn open(
        backend: &dyn StorageBackend,
        arena: Arena,
        namespace: Hash,
        full: bool,
    ) -> Result<Self> {
        let (data, allocation) = arena.views::<K>(backend)?;
        let mut headers = [None; 2];
        let mut certificates = [None; 2];
        let mut headers_complete = true;
        let identity = super::member_identity(namespace, &arena.disk_id);
        let decode = |bytes: &Bytes, certificate| -> Result<Option<Root>> {
            match Root::decode::<K>(bytes, identity, arena.pages(), certificate) {
                Ok(root) => Ok(Some(root)),
                Err(err @ ArgosError::IncompatibleFormat(_)) => Err(err),
                Err(_) => Ok(None),
            }
        };
        for slot in 0..2 {
            match arena.read(backend, slot as u64 + 2) {
                Ok(bytes) => certificates[slot] = decode(&bytes, true)?,
                Err(_) => headers_complete = false,
            }
            match arena.read(backend, slot as u64) {
                Ok(bytes) => headers[slot] = decode(&bytes, false)?,
                Err(_) => headers_complete = false,
            }
        }
        let mut allocator_heads_valid = [true; 2];
        if let Some(map) = &allocation {
            for (slot, root) in headers.iter().enumerate() {
                if let Some(root) = root {
                    allocator_heads_valid[slot] = root
                        .physical
                        .is_some_and(|state| map.open_root(backend, state.allocator).is_ok());
                }
            }
        }
        let mut member = Self {
            arena,
            data,
            allocation,
            allocator_heads_valid,
            headers_complete,
            headers,
            roots: [None; 2],
            certificates,
            graph: Graph::new(),
            free: FreePages::default(),
            key: std::marker::PhantomData,
        };
        if full {
            match member.scan_graph(backend) {
                Ok((roots, graph)) => {
                    member.roots = roots;
                    member.free = FreePages::from_graph(member.data.pages(), &graph);
                    member.graph = graph;
                }
                Err(_) => {
                    // Keep certificate evidence even when the body is
                    // unreadable, but never recycle a partially read graph.
                    member.headers_complete = false;
                }
            }
        } else {
            for (slot, root) in member.headers.iter().enumerate() {
                if let Some(root) = root {
                    if member.data.validate_tree_head::<K>(backend, *root).is_ok() {
                        member.roots[slot] = Some(*root);
                    }
                }
            }
        }
        Ok(member)
    }

    fn scan_graph(&self, backend: &dyn StorageBackend) -> Result<([Option<Root>; 2], Graph)> {
        let mut roots = [None; 2];
        let mut graph = Graph::new();
        let mut verified = VerifiedPages::<K>::new();
        for (slot, candidate) in self.headers.iter().copied().enumerate() {
            let Some(candidate) = candidate else {
                continue;
            };
            let mut scanner = Scanner {
                arena: &self.data,
                backend,
                verified: &mut verified,
                graph: Graph::new(),
            };
            if let Some(ptr) = candidate.tree {
                match scanner.root(ptr) {
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
            roots[slot] = Some(candidate);
        }
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
        Ok((roots, graph))
    }

    fn allocator_root(&self, slot: usize) -> AllocatorRoot {
        self.headers[slot]
            .and_then(|root| root.physical)
            .map_or_else(AllocatorRoot::default, |state| state.allocator)
    }

    fn paired(&self, keep: usize) -> bool {
        if !self.headers_complete || self.roots != self.headers {
            return false;
        }
        let Some(base) = self.roots[keep] else {
            return false;
        };
        let Some(physical) = base.physical else {
            return false;
        };
        let other = self.roots[1 - keep];
        if base.txid == 0 && base.tree.is_none() && other == Some(base) {
            return physical == PhysicalState::default();
        }
        // An old selected root may have lost its expected alternate during
        // an interrupted newer publication. Never allocate from that stale
        // union; reconstruct the exact live graph before attempting writes.
        physical.retained == other.map_or([0; 32], Root::local_identity)
    }

    /// Exhaustive offline invariant check, intentionally separate from the
    /// mount path. Confirms physical counts exactly match both retained roots.
    pub(super) fn audit_allocator(
        &self,
        backend: &dyn StorageBackend,
        current: (u64, Hash),
    ) -> Result<()> {
        let map = self
            .allocation
            .as_ref()
            .ok_or_else(|| corrupt("missing V3 allocator"))?;
        let slot = self
            .root_slot(current)
            .ok_or_else(|| corrupt("member has no current V3 root"))?;
        if !self.paired(slot) {
            return Err(corrupt(
                "V3 allocator/alternate root pairing needs recovery",
            ));
        }
        let (roots, graph) = self.scan_graph(backend)?;
        if roots != self.headers {
            return Err(corrupt("V3 retained logical root is damaged"));
        }
        let mut expected = BTreeMap::new();
        for (page, entry) in graph {
            expected.insert(
                page,
                u32::try_from(entry.refs)
                    .map_err(|_| corrupt("physical reference count overflow"))?,
            );
        }
        if map.allocated_entries(backend, self.allocator_root(slot))? != expected {
            return Err(corrupt(
                "V3 physical allocator differs from reachable root graph",
            ));
        }
        Ok(())
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
        source: &Member<K>,
        source_slot: usize,
    ) -> Result<Prepared> {
        let root = source.roots[source_slot].ok_or_else(|| corrupt("missing repair source"))?;
        // Verify the source graph before copying. Recovery is allowed to
        // perform full scans; the ordinary commit path is not.
        let source_full = if source.allocation.is_some() {
            let (roots, graph) = source.scan_graph(backend)?;
            if roots[source_slot] != Some(root) {
                return Err(corrupt("repair source root is not fully readable"));
            }
            Some(graph)
        } else {
            None
        };
        let graph = source_full.as_ref().unwrap_or(&source.graph);
        let keep = (0..2)
            .filter(|&slot| self.roots[slot].is_some())
            .max_by_key(|&slot| (self.is_certified(slot), self.roots[slot].unwrap().txid))
            .unwrap_or(1);
        let prepared = self.prepare_with(
            backend,
            keep,
            root.txid,
            root.physical.map(|p| p.features),
            |plan| {
                root.tree
                    .map(|ptr| plan.copy_tree(source, graph, ptr))
                    .transpose()
            },
        )?;
        if prepared.root.identity() != root.identity() {
            return Err(corrupt("repair changed the logical metadata root"));
        }
        Ok(prepared)
    }

    pub(super) fn prepare(
        &self,
        backend: &dyn StorageBackend,
        slot: usize,
        txid: u64,
        changes: &TreeChanges<K>,
    ) -> Result<Prepared> {
        let root = self.roots[slot].ok_or_else(|| corrupt("missing base root"))?;
        self.prepare_with(
            backend,
            slot,
            txid,
            root.physical.map(|p| p.features),
            |plan| {
                if K::PERSISTENT_ALLOCATOR {
                    plan.update_catalog(root.tree, changes)
                } else if root.tree.is_none() {
                    plan.build_empty(changes)
                } else {
                    let mut tree = root.tree;
                    for (key, value) in changes {
                        tree = plan.update_root(tree, key.clone(), value.as_deref())?;
                    }
                    Ok(tree)
                }
            },
        )
    }

    fn prepare_with(
        &self,
        backend: &dyn StorageBackend,
        keep: usize,
        txid: u64,
        features: Option<V3Features>,
        build: impl FnOnce(&mut Plan<'_, K>) -> Result<Option<Ptr>>,
    ) -> Result<Prepared> {
        if !self.headers_complete {
            return Err(corrupt("metadata member has incomplete recovery evidence"));
        }
        if !self.allocator_heads_valid.iter().all(|valid| *valid) {
            return Err(corrupt("member physical allocator head is damaged"));
        }
        let reads_before = self
            .allocation
            .as_ref()
            .map_or(0, AllocationMap::read_calls);
        let fast = self.allocation.is_some() && self.paired(keep);
        let rebuilt = if self.allocation.is_some() && !fast {
            let (roots, graph) = self.scan_graph(backend)?;
            if roots[keep] != self.roots[keep] {
                return Err(corrupt("base metadata graph failed recovery validation"));
            }
            Some((roots, graph))
        } else {
            None
        };
        let fallback_free = rebuilt
            .as_ref()
            .map(|(_, graph)| FreePages::from_graph(self.data.pages(), graph));
        let free = if fast {
            FreeCursor::Persistent {
                map: self.allocation.as_ref().unwrap(),
                root: self.allocator_root(keep),
                next: RESERVED_PAGES,
                buffer: VecDeque::new(),
            }
        } else {
            FreeCursor::Resident(Box::new(
                fallback_free.as_ref().unwrap_or(&self.free).iter(),
            ))
        };
        let mut plan = Plan {
            member: self,
            backend,
            free,
            recycled: Vec::new(),
            pages: BTreeMap::new(),
            reads: 0,
        };
        let tree = build(&mut plan)?;
        let mut prepared = Prepared {
            slot: 1 - keep,
            root: Root {
                txid,
                tree,
                physical: features.map(|features| PhysicalState {
                    features,
                    ..Default::default()
                }),
            },
            pages: plan.pages,
            reads: plan.reads,
            allocation: None,
        };
        if let Some(map) = &self.allocation {
            self.prepare_allocator(backend, map, keep, rebuilt.as_ref(), &mut prepared)?;
            prepared.reads += map.read_calls().saturating_sub(reads_before);
        } else {
            self.validate_plan(&prepared)?;
        }
        Ok(prepared)
    }

    fn prepare_allocator(
        &self,
        backend: &dyn StorageBackend,
        map: &AllocationMap,
        keep: usize,
        rebuilt: Option<&([Option<Root>; 2], Graph)>,
        plan: &mut Prepared,
    ) -> Result<()> {
        let base = self.allocator_root(keep);
        let retained = self.allocator_root(1 - keep);
        map.open_root(backend, base)?;
        map.open_root(backend, retained)?;
        let roots = rebuilt.map_or(&self.roots, |(roots, _)| roots);
        let mut counts = BTreeMap::<u64, u32>::new();
        if let Some((_, graph)) = rebuilt {
            // An interrupted root publication can invalidate the exact
            // allocator/alternate pairing. Rebuild counts conservatively
            // from both actually intact logical roots, clearing stale
            // entries only after scanning those graphs successfully.
            for page in map.allocated_entries(backend, base)?.into_keys() {
                counts.insert(page, 0);
            }
            for (&page, entry) in graph {
                counts.insert(
                    page,
                    u32::try_from(entry.refs)
                        .map_err(|_| corrupt("physical reference count overflow"))?,
                );
            }
        }
        if rebuilt.is_none() {
            let pages: BTreeSet<_> = plan
                .pages
                .keys()
                .copied()
                .chain(
                    plan.pages
                        .values()
                        .flat_map(|page| page.children.iter().map(|ptr| ptr.page)),
                )
                .chain(plan.root.tree.map(|ptr| ptr.page))
                .chain(
                    roots[plan.slot]
                        .and_then(|root| root.tree)
                        .map(|ptr| ptr.page),
                )
                .collect();
            counts = map.reference_counts(backend, base, &pages)?;
        }
        let load = |counts: &mut BTreeMap<u64, u32>, page: u64| -> Result<u32> {
            if page < RESERVED_PAGES || page >= self.data.pages() {
                return Err(corrupt("logical pointer outside V3 data arena"));
            }
            if let Some(&count) = counts.get(&page) {
                return Ok(count);
            }
            let count = if rebuilt.is_some() {
                0
            } else {
                map.reference_count(backend, base, page)?
            };
            counts.insert(page, count);
            Ok(count)
        };
        let mut identities = BTreeMap::new();
        for (&page, staged) in &plan.pages {
            if staged.ptr.page != page || load(&mut counts, page)? != 0 {
                return Err(corrupt("V3 plan attempted to overwrite referenced page"));
            }
            identities.insert(page, staged.ptr.hash);
        }
        // A staged page contributes outgoing references once. The root adds
        // one additional incoming reference. Existing nodes already own their
        // outgoing references, so untouched subtrees are never traversed.
        for ptr in plan
            .pages
            .values()
            .flat_map(|page| page.children.iter().copied())
            .chain(plan.root.tree)
        {
            if identities
                .insert(ptr.page, ptr.hash)
                .is_some_and(|hash| hash != ptr.hash)
            {
                return Err(corrupt("V3 plan has conflicting physical page identities"));
            }
            let count = load(&mut counts, ptr.page)?;
            if let Some(staged) = plan.pages.get(&ptr.page) {
                if staged.ptr != ptr {
                    return Err(corrupt("V3 staged reference mismatch"));
                }
            } else if count == 0 {
                return Err(corrupt("V3 plan references an unallocated page"));
            }
            counts.insert(
                ptr.page,
                count
                    .checked_add(1)
                    .ok_or_else(|| corrupt("physical reference count overflow"))?,
            );
        }
        if plan.pages.keys().any(|page| counts.get(page) != Some(&1)) {
            return Err(corrupt("V3 plan has unreachable or shared staged page"));
        }
        // Release only the overwritten root after adding the new edges.
        // A zero transition is the only reason to read/release descendants.
        let mut pending: Vec<_> = roots[plan.slot]
            .and_then(|root| root.tree)
            .into_iter()
            .collect();
        while let Some(ptr) = pending.pop() {
            if identities
                .insert(ptr.page, ptr.hash)
                .is_some_and(|hash| hash != ptr.hash)
            {
                return Err(corrupt("V3 retired page identity mismatch"));
            }
            let count = load(&mut counts, ptr.page)?
                .checked_sub(1)
                .ok_or_else(|| corrupt("physical reference count underflow"))?;
            counts.insert(ptr.page, count);
            if count == 0 {
                let node = self.data.read_node::<K>(backend, ptr)?;
                plan.reads += 1;
                let children = node.children();
                if rebuilt.is_none() {
                    let missing = children
                        .iter()
                        .map(|ptr| ptr.page)
                        .filter(|page| !counts.contains_key(page))
                        .collect();
                    counts.extend(map.reference_counts(backend, base, &missing)?);
                }
                pending.extend(children);
            }
        }
        let allocation = map.prepare(backend, base, retained, &counts)?;
        plan.root.physical = Some(PhysicalState {
            allocator: allocation.root(),
            retained: roots[keep].map_or([0; 32], Root::local_identity),
            features: plan
                .root
                .physical
                .ok_or_else(|| corrupt("missing V3 feature set"))?
                .features,
        });
        plan.allocation = Some(allocation);
        Ok(())
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
        if let Some(allocation) = &plan.allocation {
            self.allocation
                .as_ref()
                .ok_or_else(|| corrupt("missing V3 allocation map"))?
                .write_prepared(backend, allocation)?;
        }
        Ok(())
    }

    pub(super) fn publish(
        &mut self,
        backend: &dyn StorageBackend,
        namespace: Hash,
        plan: Prepared,
    ) -> Result<CommitIo> {
        let bytes = plan.root.encode::<K>(
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
        let allocator_pages = plan
            .allocation
            .as_ref()
            .map_or(0, PreparedAllocation::pages_written);
        let io = CommitIo {
            pages_read: plan.reads + plan.pages.len() + allocator_pages + 1,
            pages_written: plan.pages.len() + allocator_pages + 1,
        };
        self.headers[plan.slot] = Some(plan.root);
        if self.allocation.is_some() {
            self.roots[plan.slot] = Some(plan.root);
            self.allocator_heads_valid[plan.slot] = true;
            self.graph.clear();
            self.free = FreePages::default();
            return Ok(io);
        }
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
        if !self.headers_complete || !self.allocator_heads_valid.iter().all(|valid| *valid) {
            return Err(corrupt("member recovery evidence is incomplete"));
        }
        let slot = self
            .root_slot(identity)
            .ok_or_else(|| corrupt("cannot certify missing root"))?;
        let root = self.roots[slot].unwrap();
        let bytes = root.encode::<K>(
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

    pub(super) fn get(
        &self,
        backend: &dyn StorageBackend,
        slot: usize,
        key: &K,
    ) -> Result<Option<Vec<u8>>> {
        let mut next = self.roots[slot].and_then(|root| root.tree);
        while let Some(ptr) = next {
            match self.data.read_node::<K>(backend, ptr)? {
                Node::Branch { children, .. } => {
                    next = children
                        .get(children.partition_point(|(max, _)| max < key))
                        .map(|(_, ptr)| *ptr);
                }
                Node::Leaf(mut entries) => {
                    return entries
                        .binary_search_by(|(candidate, _)| candidate.cmp(key))
                        .ok()
                        .map(|index| {
                            self.arena
                                .read_value::<K>(backend, entries.swap_remove(index).1)
                        })
                        .transpose();
                }
                Node::Blob { .. } | Node::Catalog(_) => {
                    return Err(corrupt("metadata lookup encountered invalid page"))
                }
            }
        }
        Ok(None)
    }

    pub(super) fn scan(
        &self,
        backend: &dyn StorageBackend,
        slot: usize,
        window: &super::ScanWindow<K>,
    ) -> Result<super::ScanPage<K>> {
        let root = self.roots[slot].ok_or_else(|| corrupt("missing scan root"))?;
        self.data.scan_window(backend, root, window)
    }

    pub(super) fn records(
        &self,
        backend: &dyn StorageBackend,
        slot: usize,
    ) -> Result<BTreeMap<K, Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut pending = self.roots[slot]
            .and_then(|root| root.tree)
            .into_iter()
            .collect::<Vec<_>>();
        while let Some(ptr) = pending.pop() {
            match self.data.read_node::<K>(backend, ptr)? {
                Node::Branch { children, .. } => {
                    pending.extend(children.into_iter().rev().map(|(_, ptr)| ptr))
                }
                Node::Catalog(roots) => pending.extend(roots.into_iter().rev().map(|(_, ptr)| ptr)),
                Node::Leaf(entries) => {
                    for (key, value) in entries {
                        if out
                            .insert(key, self.data.read_value::<K>(backend, value)?)
                            .is_some()
                        {
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
    allocation: Option<PreparedAllocation>,
}

enum FreeCursor<'a> {
    Resident(Box<dyn Iterator<Item = u64> + 'a>),
    Persistent {
        map: &'a AllocationMap,
        root: AllocatorRoot,
        next: u64,
        buffer: VecDeque<u64>,
    },
}

struct Plan<'a, K: TreeKey> {
    member: &'a Member<K>,
    backend: &'a dyn StorageBackend,
    free: FreeCursor<'a>,
    recycled: Vec<u64>,
    pages: BTreeMap<u64, Staged>,
    reads: usize,
}

impl<K: TreeKey> Plan<'_, K> {
    fn read(&mut self, ptr: Ptr) -> Result<Node<K>> {
        if let Some(page) = self.pages.get(&ptr.page) {
            return Node::decode(&page.bytes, ptr.hash);
        }
        self.reads += 1;
        self.member.data.read_node(self.backend, ptr)
    }

    fn alloc(&mut self) -> Result<u64> {
        if let Some(page) = self.recycled.pop() {
            return Ok(page);
        }
        let page = match &mut self.free {
            FreeCursor::Resident(iter) => iter.next(),
            FreeCursor::Persistent {
                map,
                root,
                next,
                buffer,
            } => {
                if buffer.is_empty() {
                    let pages = map.free_window(self.backend, *root, *next, 128)?;
                    if let Some(last) = pages.last() {
                        *next = *last + 1;
                    }
                    buffer.extend(pages);
                }
                buffer.pop_front()
            }
        };
        page.ok_or_else(|| ArgosError::DiskFull {
            disk_id: self.member.arena.disk_id.clone(),
            required: PAGE_SIZE as u64,
            available: 0,
        })
    }

    fn store(&mut self, node: Node<K>, replace: Option<Ptr>) -> Result<Ptr> {
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

    fn store_tree(&mut self, node: Node<K>, old: Option<Ptr>) -> Result<Vec<(K, Ptr)>> {
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
    fn rebalance(&mut self, children: &mut Vec<(K, Ptr)>, index: usize) -> Result<()> {
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

    fn update(&mut self, ptr: Ptr, key: K, value: Option<&[u8]>) -> Result<Vec<(K, Ptr)>> {
        match self.read(ptr)? {
            Node::Leaf(mut entries) => {
                match (
                    entries.binary_search_by(|(candidate, _)| candidate.cmp(&key)),
                    value,
                ) {
                    (Ok(index), Some(bytes)) => {
                        // Changes are unique per transaction, so no replaced
                        // overflow chain can have been allocated by this plan.
                        if matches!(&entries[index].1, Value::Inline(old) if old == bytes) {
                            return Ok(vec![(entries.last().unwrap().0.clone(), ptr)]);
                        }
                        entries[index].1 = self.value(bytes)?;
                    }
                    (Ok(index), None) => {
                        entries.remove(index);
                    }
                    (Err(index), Some(bytes)) => entries.insert(index, (key, self.value(bytes)?)),
                    (Err(_), None) => return Ok(vec![(entries.last().unwrap().0.clone(), ptr)]),
                }
                self.store_tree(Node::Leaf(entries), Some(ptr))
            }
            Node::Branch {
                level,
                mut children,
            } => {
                let index = children
                    .partition_point(|(max, _)| max < &key)
                    .min(children.len() - 1);
                let old = children[index].clone();
                let replacement = self.update(old.1, key, value)?;
                if replacement.as_slice() == [old] {
                    return Ok(vec![(children.last().unwrap().0.clone(), ptr)]);
                }
                children.splice(index..index + 1, replacement);
                if value.is_none() {
                    self.rebalance(&mut children, index)?;
                }
                self.store_tree(Node::Branch { level, children }, Some(ptr))
            }
            Node::Blob { .. } | Node::Catalog(_) => {
                Err(corrupt("metadata update encountered invalid page"))
            }
        }
    }

    // Copy the exact authenticated tree shape rather than rebuilding from
    // keys (which could change the logical digest). An explicit postorder
    // stack also handles long overflow chains without recursive stack use.
    fn copy_tree(&mut self, source: &Member<K>, graph: &Graph, root: Ptr) -> Result<Ptr> {
        let mut pending = vec![(root, false)];
        let mut translated = BTreeMap::<u64, Ptr>::new();
        while let Some((ptr, expanded)) = pending.pop() {
            if !expanded {
                let entry = graph
                    .get(&ptr.page)
                    .filter(|entry| entry.ptr == ptr)
                    .ok_or_else(|| corrupt("repair source graph changed"))?;
                pending.push((ptr, true));
                pending.extend(entry.children.iter().rev().map(|&child| (child, false)));
                continue;
            }
            let mut node = source.data.read_node(self.backend, ptr)?;
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
                Node::Catalog(roots) => {
                    for (_, child) in roots {
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

    fn update_catalog(
        &mut self,
        root: Option<Ptr>,
        changes: &TreeChanges<K>,
    ) -> Result<Option<Ptr>> {
        let mut roots = match root {
            Some(ptr) => match self.read(ptr)? {
                Node::Catalog(roots) => roots.into_iter().collect::<BTreeMap<_, _>>(),
                _ => return Err(corrupt("V3 transaction root is not a catalog")),
            },
            None => BTreeMap::new(),
        };
        let before = roots.clone();
        let mut grouped = BTreeMap::<u16, Vec<(&K, &Option<Vec<u8>>)>>::new();
        for (key, value) in changes {
            let domain = key
                .domain_id()
                .ok_or_else(|| corrupt("V3 key missing domain"))?;
            grouped.entry(domain).or_default().push((key, value));
        }
        for (domain, changes) in grouped {
            let mut subtree = roots.get(&domain).copied();
            if subtree.is_none() {
                subtree = self.build_empty(changes)?;
            } else {
                for (key, value) in changes {
                    subtree = self.update_root(subtree, key.clone(), value.as_deref())?;
                }
            }
            if let Some(ptr) = subtree {
                roots.insert(domain, ptr);
            } else {
                roots.remove(&domain);
            }
        }
        if roots == before {
            return Ok(root);
        }
        if roots.is_empty() {
            return Ok(None);
        }
        if roots.len() > NODE_CAPACITY / 42 {
            return Err(ArgosError::FileTooLarge("V3 root catalog capacity".into()));
        }
        self.store(Node::Catalog(roots.into_iter().collect()), root)
            .map(Some)
    }

    // Initial import is a bottom-up packed build, not N repeated COW path
    // rewrites. Later transactions retain the ordinary bounded path update.
    fn build_empty<'b>(
        &mut self,
        changes: impl IntoIterator<Item = (&'b K, &'b Option<Vec<u8>>)>,
    ) -> Result<Option<Ptr>>
    where
        K: 'b,
    {
        let mut leaves = Vec::new();
        let mut entries = Vec::new();
        let mut used = 0;
        for (key, value) in changes {
            let Some(bytes) = value else { continue };
            let value = self.value(bytes)?;
            let len = key.encoded_len() + value.encoded_len();
            if used + len > NODE_CAPACITY {
                leaves.extend(self.store_tree(Node::Leaf(std::mem::take(&mut entries)), None)?);
                used = 0;
            }
            entries.push((key.clone(), value));
            used += len;
        }
        if !entries.is_empty() {
            leaves.extend(self.store_tree(Node::Leaf(entries), None)?);
        }
        let mut level = 1;
        while leaves.len() > 1 {
            let mut parents = Vec::new();
            let mut children = Vec::new();
            let mut used = 0;
            for (key, ptr) in leaves {
                let len = key.encoded_len() + 40;
                if used + len > NODE_CAPACITY {
                    parents.extend(self.store_tree(
                        Node::Branch {
                            level,
                            children: std::mem::take(&mut children),
                        },
                        None,
                    )?);
                    used = 0;
                }
                children.push((key, ptr));
                used += len;
            }
            if !children.is_empty() {
                parents.extend(self.store_tree(Node::Branch { level, children }, None)?);
            }
            leaves = parents;
            level += 1;
        }
        Ok(leaves.first().map(|(_, ptr)| *ptr))
    }

    fn update_root(
        &mut self,
        root: Option<Ptr>,
        key: K,
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
