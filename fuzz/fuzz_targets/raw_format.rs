#![no_main]

mod common;

use argosfs::raw_format::{
    RawDeviceLabel, RawSuperblock, DEVICE_LABEL_SIZE, MIN_DEVICE_BYTES, SUPERBLOCK_SIZE,
};
use libfuzzer_sys::fuzz_target;
use uuid::Uuid;

fuzz_target!(|data: &[u8]| {
    if data.len() >= SUPERBLOCK_SIZE {
        if let Ok(decoded) = RawSuperblock::decode(data) {
            let encoded = decoded.encode();
            let round = RawSuperblock::decode(&encoded).expect("decoded superblock must re-decode");
            assert_eq!(decoded, round);
        }
    }

    if data.len() >= DEVICE_LABEL_SIZE {
        if let Ok(decoded) = RawDeviceLabel::decode(data) {
            let encoded = decoded.encode();
            let round = RawDeviceLabel::decode(&encoded).expect("decoded label must re-decode");
            assert_eq!(decoded, round);
        }
    }

    let mut pool = [0u8; 16];
    let mut device = [0u8; 16];
    for index in 0..16 {
        pool[index] = common::byte(data, index);
        device[index] = common::byte(data, 16 + index);
    }
    let k = 1 + (common::byte(data, 32) as u32 % 6);
    let m = common::byte(data, 33) as u32 % 4;
    let chunk_size = 4096u64 << (common::byte(data, 34) as u32 % 7);
    let capacity = (128 * 1024 * 1024u64) + (common::u64_at(data, 35) % (512 * 1024 * 1024u64));
    let disk_id = common::text_at(data, 43, 48, "disk-fuzz");
    let label = common::text_at(data, 96, 96, "pool-fuzz");

    let superblock = RawSuperblock::new(
        Uuid::from_bytes(pool),
        Uuid::from_bytes(device),
        disk_id,
        common::byte(data, 193) as u32,
        k,
        m,
        chunk_size,
        capacity.max(MIN_DEVICE_BYTES),
        label,
    )
    .expect("generated raw superblock parameters must be valid");

    let encoded = superblock.encode();
    let decoded = RawSuperblock::decode(&encoded).expect("encoded superblock must decode");
    assert_eq!(superblock, decoded);

    let label = superblock.device_label();
    let encoded_label = label.encode();
    let decoded_label = RawDeviceLabel::decode(&encoded_label).expect("encoded label must decode");
    assert_eq!(label, decoded_label);
});
