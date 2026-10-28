use super::*;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone)]
struct MemoryBackend {
    data: Arc<Mutex<Vec<u8>>>,
    read_delay: Duration,
    write_delay: Duration,
    flush_delay: Duration,
    write_error: Option<i32>,
}

impl MemoryBackend {
    fn new(size: usize) -> Self {
        Self {
            data: Arc::new(Mutex::new(vec![0u8; size])),
            read_delay: Duration::ZERO,
            write_delay: Duration::ZERO,
            flush_delay: Duration::ZERO,
            write_error: None,
        }
    }

    fn device(&self) -> Result<()> {
        Ok(())
    }
}

impl StorageBackend for MemoryBackend {
    fn backend_kind(&self) -> BackendKind {
        BackendKind::LoopBlock
    }

    fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>> {
        Ok(vec![BackendDeviceInfo {
            device_id: "disk-0000".to_string(),
            path: PathBuf::from("/dev/null"),
            capacity: self.data.lock().len() as u64,
            status: DiskStatus::Online,
        }])
    }

    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        if device_id != "disk-0000" {
            return Err(ArgosError::MissingDevice(device_id.clone()));
        }
        if !self.read_delay.is_zero() {
            std::thread::sleep(self.read_delay);
        }
        let start = usize::try_from(offset)
            .map_err(|_| ArgosError::Invalid("read offset is too large".to_string()))?;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| ArgosError::Invalid("read range is too large".to_string()))?;
        let data = self.data.lock();
        if end > data.len() {
            return Err(ArgosError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short read",
            )));
        }
        buf.copy_from_slice(&data[start..end]);
        Ok(())
    }

    fn write_at(&self, device_id: &String, offset: u64, data: &[u8]) -> Result<()> {
        if device_id != "disk-0000" {
            return Err(ArgosError::MissingDevice(device_id.clone()));
        }
        if !self.write_delay.is_zero() {
            std::thread::sleep(self.write_delay);
        }
        if let Some(errno) = self.write_error {
            return Err(ArgosError::Io(io::Error::from_raw_os_error(errno)));
        }
        let start = usize::try_from(offset)
            .map_err(|_| ArgosError::Invalid("write offset is too large".to_string()))?;
        let end = start
            .checked_add(data.len())
            .ok_or_else(|| ArgosError::Invalid("write range is too large".to_string()))?;
        let mut target = self.data.lock();
        if end > target.len() {
            return Err(ArgosError::DiskFull {
                disk_id: device_id.clone(),
                required: data.len() as u64,
                available: target.len().saturating_sub(start) as u64,
            });
        }
        target[start..end].copy_from_slice(data);
        Ok(())
    }

    fn flush_device(&self, device_id: &String) -> Result<()> {
        self.device()?;
        if device_id != "disk-0000" {
            return Err(ArgosError::MissingDevice(device_id.clone()));
        }
        if !self.flush_delay.is_zero() {
            std::thread::sleep(self.flush_delay);
        }
        Ok(())
    }

    fn flush_all(&self) -> Result<()> {
        self.flush_device(&"disk-0000".to_string())
    }

    fn capacity(&self, device_id: &String) -> Result<u64> {
        self.device()?;
        if device_id != "disk-0000" {
            return Err(ArgosError::MissingDevice(device_id.clone()));
        }
        Ok(self.data.lock().len() as u64)
    }

    fn device_status(&self, device_id: &String) -> Result<DiskStatus> {
        self.device()?;
        if device_id != "disk-0000" {
            return Err(ArgosError::MissingDevice(device_id.clone()));
        }
        Ok(DiskStatus::Online)
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            direct_io: false,
            fallocate: true,
            discard: false,
        }
    }
}

fn id() -> String {
    "disk-0000".to_string()
}

#[test]
fn fast_operations_pass_through() {
    let backend =
        GuardedBackend::with_timeout(Arc::new(MemoryBackend::new(4096)), Duration::from_secs(1));
    let disk = id();
    backend.write_at(&disk, 0, b"hello").unwrap();
    backend.flush_device(&disk).unwrap();
    backend.flush_all().unwrap();
    let mut data = [0u8; 5];
    backend.read_at(&disk, 0, &mut data).unwrap();
    assert_eq!(&data, b"hello");
    assert_eq!(backend.capacity(&disk).unwrap(), 4096);
    assert_eq!(backend.device_status(&disk).unwrap(), DiskStatus::Online);
    assert!(backend.failure(&disk).is_none());
}

#[test]
fn slow_write_is_fenced_after_deadline() {
    let mut memory = MemoryBackend::new(4096);
    memory.write_delay = Duration::from_millis(500);
    let backend = GuardedBackend::with_timeout(Arc::new(memory), Duration::from_millis(50));
    let disk = id();

    let started = Instant::now();
    let error = backend.write_at(&disk, 0, b"hello").unwrap_err();
    assert!(matches!(error, ArgosError::DeviceTimeout { .. }), "{error}");
    assert!(started.elapsed() < Duration::from_millis(300));

    let started = Instant::now();
    let error = backend.write_at(&disk, 0, b"hello").unwrap_err();
    assert!(matches!(error, ArgosError::DeviceFailed { .. }), "{error}");
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(backend.device_status(&disk).unwrap(), DiskStatus::Failed);
    assert!(backend.flush_all().is_ok());
}

#[test]
fn slow_read_is_fenced_after_deadline() {
    let mut memory = MemoryBackend::new(4096);
    memory.read_delay = Duration::from_millis(500);
    let backend = GuardedBackend::with_timeout(Arc::new(memory), Duration::from_millis(50));
    let disk = id();
    let started = Instant::now();
    let error = backend.read_at(&disk, 0, &mut [0u8; 8]).unwrap_err();
    assert!(matches!(error, ArgosError::DeviceTimeout { .. }), "{error}");
    assert!(started.elapsed() < Duration::from_millis(300));
}

#[test]
fn slow_flush_is_fenced_after_deadline() {
    let mut memory = MemoryBackend::new(4096);
    memory.flush_delay = Duration::from_millis(500);
    let backend = GuardedBackend::with_timeout(Arc::new(memory), Duration::from_millis(50));
    let disk = id();
    let started = Instant::now();
    let error = backend.flush_device(&disk).unwrap_err();
    assert!(matches!(error, ArgosError::DeviceTimeout { .. }), "{error}");
    assert!(started.elapsed() < Duration::from_millis(300));
}

#[test]
fn fatal_write_error_fences_device() {
    let mut memory = MemoryBackend::new(4096);
    memory.write_error = Some(libc::EIO);
    let backend = GuardedBackend::with_timeout(Arc::new(memory), Duration::from_secs(1));
    let disk = id();
    let error = backend.write_at(&disk, 0, b"hello").unwrap_err();
    assert!(matches!(error, ArgosError::DeviceFailed { .. }), "{error}");
    let started = Instant::now();
    let error = backend.write_at(&disk, 0, b"hello").unwrap_err();
    assert!(matches!(error, ArgosError::DeviceFailed { .. }), "{error}");
    assert!(started.elapsed() < Duration::from_millis(50));
}

#[test]
fn missing_device_is_passed_through_without_fencing() {
    let backend =
        GuardedBackend::with_timeout(Arc::new(MemoryBackend::new(4096)), Duration::from_secs(1));
    let missing = "disk-9999".to_string();
    for _ in 0..2 {
        let error = backend.write_at(&missing, 0, b"hello").unwrap_err();
        assert!(matches!(error, ArgosError::MissingDevice(_)), "{error}");
        assert!(backend.failure(&missing).is_none());
    }
}
