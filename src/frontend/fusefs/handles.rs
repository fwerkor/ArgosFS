use super::*;

#[derive(Clone, Debug)]
pub(super) struct OpenFileHandle {
    pub(super) ino: InodeId,
    pub(super) flags: i32,
}

impl OpenFileHandle {
    pub(super) fn can_read(&self) -> bool {
        self.flags & libc::O_ACCMODE != libc::O_WRONLY
    }

    pub(super) fn can_write(&self) -> bool {
        self.flags & libc::O_ACCMODE != libc::O_RDONLY
    }
}

#[derive(Default)]
pub(super) struct FuseHandles {
    pub(super) next: u64,
    pub(super) files: BTreeMap<u64, OpenFileHandle>,
    pub(super) refs: BTreeMap<InodeId, usize>,
}

impl FuseHandles {
    pub(super) fn open(&mut self, file: OpenFileHandle) -> FileHandle {
        loop {
            self.next = self.next.wrapping_add(1).max(1);
            if self.files.contains_key(&self.next) {
                continue;
            }
            let handle = self.next;
            *self.refs.entry(file.ino).or_default() += 1;
            self.files.insert(handle, file);
            return FileHandle(handle);
        }
    }

    pub(super) fn get(&self, handle: FileHandle) -> Option<&OpenFileHandle> {
        self.files.get(&handle.0)
    }

    pub(super) fn refs(&self, ino: InodeId) -> usize {
        self.refs.get(&ino).copied().unwrap_or(0)
    }

    pub(super) fn close(&mut self, handle: FileHandle) -> Option<(OpenFileHandle, bool)> {
        let file = self.files.remove(&handle.0)?;
        let last = if let Some(count) = self.refs.get_mut(&file.ino) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.refs.remove(&file.ino);
                true
            } else {
                false
            }
        } else {
            true
        };
        Some((file, last))
    }
}
