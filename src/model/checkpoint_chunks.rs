//! Snapshot-local, copy-on-write cache of independently encoded inode ranges.
//! Only checkpoint encoding uses it; normal metadata and journal JSON remain
//! unchanged. Mutating an inode invalidates its group's frame before exposing
//! the mutable reference, so old snapshots cannot acquire newer bytes.
use super::types::{Inode, InodeId};
use crate::error::{ArgosError, Result};
use imbl::{HashMap, OrdMap, OrdSet};
use std::io::Write;
use std::sync::{Arc, OnceLock};

const INODES_PER_FRAME: u64 = 512;
const FRAME_ZSTD_LEVEL: i32 = 3;

#[derive(Debug)]
pub(crate) struct CheckpointFrame {
    pub(crate) bytes: Vec<u8>,
    pub(crate) expanded_len: usize,
}

#[derive(Clone, Debug, Default)]
struct Chunk {
    ids: OrdSet<InodeId>,
    first: OnceLock<Arc<CheckpointFrame>>,
    following: OnceLock<Arc<CheckpointFrame>>,
}

impl Chunk {
    fn invalidate(&mut self) {
        self.first = OnceLock::new();
        self.following = OnceLock::new();
    }

    fn frame(&self, map: &HashMap<InodeId, Inode>, first: bool) -> Result<Arc<CheckpointFrame>> {
        let cell = if first { &self.first } else { &self.following };
        if let Some(frame) = cell.get() {
            return Ok(frame.clone());
        }
        // Stream large individual inodes into zstd instead of retaining an
        // expanded group-sized JSON buffer.
        let encoder = zstd::stream::write::Encoder::new(Vec::new(), FRAME_ZSTD_LEVEL)?;
        let mut writer = FrameWriter {
            output: std::io::BufWriter::with_capacity(64 * 1024, encoder),
            len: 0,
        };
        for (index, ino) in self.ids.iter().enumerate() {
            if !first || index != 0 {
                writer.write_all(b",")?;
            }
            write!(writer, "\"{ino}\":")?;
            let inode = map.get(ino).ok_or_else(|| {
                ArgosError::CorruptedMetadata(format!("missing checkpoint inode {ino}"))
            })?;
            serde_json::to_writer(&mut writer, inode)?;
        }
        let frame = Arc::new(CheckpointFrame {
            expanded_len: writer.len,
            bytes: writer
                .output
                .into_inner()
                .map_err(|err| err.into_error())?
                .finish()?,
        });
        // Concurrent readers may encode the same immutable chunk. Either
        // result is byte-identical; retain the one already published.
        Ok(cell.get_or_init(|| frame).clone())
    }
}

struct FrameWriter<W> {
    output: W,
    len: usize,
}

impl<W: Write> Write for FrameWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.len = self
            .len
            .checked_add(written)
            .ok_or_else(|| std::io::Error::other("checkpoint fragment length overflow"))?;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct CheckpointChunks {
    groups: OrdMap<u64, Arc<Chunk>>,
}

impl CheckpointChunks {
    pub(super) fn from_map(map: &HashMap<InodeId, Inode>) -> Self {
        let mut index = Self::default();
        for ino in map.keys() {
            index.insert(*ino);
        }
        index
    }

    pub(super) fn insert(&mut self, ino: InodeId) {
        let chunk = Arc::make_mut(self.groups.entry(ino / INODES_PER_FRAME).or_default());
        chunk.ids.insert(ino);
        chunk.invalidate();
    }

    pub(super) fn invalidate(&mut self, ino: InodeId) {
        if let Some(chunk) = self.groups.get_mut(&(ino / INODES_PER_FRAME)) {
            Arc::make_mut(chunk).invalidate();
        }
    }

    pub(super) fn remove(&mut self, ino: InodeId) {
        let group = ino / INODES_PER_FRAME;
        if let Some(chunk) = self.groups.get_mut(&group) {
            let chunk = Arc::make_mut(chunk);
            chunk.ids.remove(&ino);
            if chunk.ids.is_empty() {
                self.groups.remove(&group);
            } else {
                chunk.invalidate();
            }
        }
    }

    pub(super) fn len(&self) -> usize {
        self.groups.len()
    }

    pub(super) fn frames(
        &self,
        map: &HashMap<InodeId, Inode>,
    ) -> Result<Vec<Arc<CheckpointFrame>>> {
        self.groups
            .values()
            .enumerate()
            .map(|(index, chunk)| chunk.frame(map, index == 0))
            .collect()
    }
}
