use super::*;

/// Copy credentials before releasing the kernel request buffer. In particular,
/// supplementary groups must not be looked up after a queued process exits.
pub(super) struct RequestIdentity {
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) groups: Vec<u32>,
}

impl RequestIdentity {
    pub(super) fn from_request(req: &Request) -> Self {
        Self {
            uid: req.uid(),
            gid: req.gid(),
            groups: request_groups(req),
        }
    }

    pub(super) fn uid(&self) -> u32 {
        self.uid
    }
    pub(super) fn gid(&self) -> u32 {
        self.gid
    }
}

pub(super) trait Credentials {
    fn uid(&self) -> u32;
    fn gid(&self) -> u32;
    fn groups(&self) -> Vec<u32>;
}

impl Credentials for Request {
    fn uid(&self) -> u32 {
        Request::uid(self)
    }
    fn gid(&self) -> u32 {
        Request::gid(self)
    }
    fn groups(&self) -> Vec<u32> {
        read_proc_status(self.pid())
            .and_then(|status| parse_status_groups_for_identity(&status, self.uid(), self.gid()))
            .unwrap_or_default()
    }
}

impl Credentials for RequestIdentity {
    fn uid(&self) -> u32 {
        self.uid
    }
    fn gid(&self) -> u32 {
        self.gid
    }
    fn groups(&self) -> Vec<u32> {
        self.groups.clone()
    }
}

pub(super) fn request_groups(req: &impl Credentials) -> Vec<u32> {
    let mut groups = req.groups();
    if !groups.contains(&req.gid()) {
        groups.push(req.gid());
    }
    groups.sort_unstable();
    groups.dedup();
    groups
}

pub(super) fn read_proc_status(pid: u32) -> Option<String> {
    let proc_dir = std::fs::File::open(format!("/proc/{pid}")).ok()?;
    let name = CString::new("status").ok()?;
    let fd = unsafe {
        libc::openat(
            proc_dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return None;
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut status = String::new();
    file.read_to_string(&mut status).ok()?;
    Some(status)
}

pub(super) fn parse_status_groups_for_identity(
    status: &str,
    uid: u32,
    gid: u32,
) -> Option<Vec<u32>> {
    let fsuid = parse_status_identity(status, "Uid:")?;
    let fsgid = parse_status_identity(status, "Gid:")?;
    if fsuid != uid || fsgid != gid {
        return None;
    }
    Some(parse_status_groups(status))
}

pub(super) fn parse_status_identity(status: &str, key: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(key))?
        .split_whitespace()
        .last()?
        .parse::<u32>()
        .ok()
}

pub(super) fn parse_status_groups(status: &str) -> Vec<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Groups:"))
        .into_iter()
        .flat_map(str::split_whitespace)
        .filter_map(|value| value.parse::<u32>().ok())
        .collect()
}
