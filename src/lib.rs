extern crate self as fuser;

pub use fuser_crate::{
    AccessFlags, BsdFileFlags, Config, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType,
    Filesystem, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption,
    OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyLseek, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request,
    SessionACL, TimeOrNow, WriteFlags,
};

#[doc(hidden)]
pub fn mount2<FS: fuser_crate::Filesystem, P: AsRef<std::path::Path>>(
    filesystem: FS,
    mountpoint: P,
    options: &fuser_crate::Config,
) -> std::io::Result<()> {
    fuser_crate::mount(filesystem, mountpoint, options)
}

pub mod cli;
pub mod control;
pub mod data;
pub mod error;
pub mod frontend;
pub mod metadata;
pub mod model;
pub mod security;
pub mod storage;
pub mod util;
pub mod volume;

pub use control::autopilot::AutopilotPolicy;
pub use control::{autopilot, health};
pub use data::{advanced_io, cache, compression, crypto, erasure};
pub use error::{ArgosError, Result};
pub use frontend::{fusefs, metrics, rootfs};
pub use metadata::inode_ops::{DirEntry, NodeAttr, RenamePolicy};
pub use metadata::{inode_ops, journal, store as metadata_store};
pub use model::types;
pub use security::acl;
pub use storage::raw::{allocator, format as raw_format, store as raw_store};
pub use storage::{backend, scan};
pub use types::{BackendKind, Compression, DiskStatus, StorageTier, VolumeConfig};
pub use volume::{ArgosFs, AutopilotConfig};
