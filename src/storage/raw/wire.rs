//! Shared fixed-width little-endian primitives for raw metadata formats.
use crate::error::{ArgosError, Result};

pub(super) fn put_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub(super) fn put_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn field<'a>(bytes: &'a [u8], offset: usize, len: usize, error: &str) -> Result<&'a [u8]> {
    offset
        .checked_add(len)
        .and_then(|end| bytes.get(offset..end))
        .ok_or_else(|| ArgosError::CorruptedMetadata(error.to_string()))
}

pub(super) fn read_u32(bytes: &[u8], offset: usize, error: &str) -> Result<u32> {
    Ok(u32::from_le_bytes(
        field(bytes, offset, 4, error)?.try_into().unwrap(),
    ))
}

pub(super) fn read_u64(bytes: &[u8], offset: usize, error: &str) -> Result<u64> {
    Ok(u64::from_le_bytes(
        field(bytes, offset, 8, error)?.try_into().unwrap(),
    ))
}

/// The caller supplies the usable width: superblock strings reserve a NUL
/// byte, while checkpoint hashes may occupy the entire field.
pub(super) fn put_string(out: &mut [u8], offset: usize, width: usize, value: &str) {
    let raw = value.as_bytes();
    let copy_len = raw.len().min(width);
    out[offset..offset + copy_len].copy_from_slice(&raw[..copy_len]);
}

pub(super) fn read_string(bytes: &[u8], offset: usize, len: usize, error: &str) -> Result<String> {
    let raw = field(bytes, offset, len, error)?;
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    std::str::from_utf8(&raw[..end])
        .map(str::to_string)
        .map_err(|err| ArgosError::CorruptedMetadata(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_width_strings_keep_both_wire_conventions() {
        let mut bytes = [0; 8];
        put_string(&mut bytes, 0, 3, "abcd");
        put_string(&mut bytes, 4, 4, "abcd");
        assert_eq!(&bytes, b"abc\0abcd");
        assert_eq!(read_string(&bytes, 0, 4, "short").unwrap(), "abc");
        assert_eq!(read_string(&bytes, 4, 4, "short").unwrap(), "abcd");
    }

    #[test]
    fn integer_fields_preserve_endianness_and_reject_invalid_ranges() {
        let mut bytes = [0; 12];
        put_u32(&mut bytes, 0, 0x12345678);
        put_u64(&mut bytes, 4, 0x0123456789abcdef);
        assert_eq!(&bytes[..4], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(read_u32(&bytes, 0, "short").unwrap(), 0x12345678);
        assert_eq!(read_u64(&bytes, 4, "short").unwrap(), 0x0123456789abcdef);
        assert!(read_u64(&bytes, 5, "short").is_err());
        assert!(read_u32(&bytes, usize::MAX, "short").is_err());
    }
}
