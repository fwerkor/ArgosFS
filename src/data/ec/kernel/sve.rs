//! Linux AArch64 SVE/SVE2 fixed-coefficient kernels.
//! The predicated loop is vector-length agnostic, including the final vector.
//! Rust's stable stdarch does not expose scalable vector types; the leaf
//! functions use the ordinary AAPCS64 ABI and only caller-saved registers.
#![allow(unsafe_code)]

use super::{scalar, tables};
use crate::data::ec::shards::ShardSource;

core::arch::global_asm!(
    r#"
    .pushsection .text.argosfs_ec_sve,"ax",%progbits
    .arch armv8-a+sve
    .p2align 4
    .macro ec_sve name, sve2
    .global \name
    .hidden \name
    .type \name, %function
\name:
    cbz x2, 3f
    ptrue p1.b
    ld1rqb z2.b, p1/z, [x3]
    ld1rqb z3.b, p1/z, [x3, #16]
    mov x5, #0
1:
    whilelo p0.b, x5, x2
    ld1b z0.b, p0/z, [x0, x5]
    mov z1.d, z0.d
    and z1.b, z1.b, #0xf
    lsr z0.b, z0.b, #4
    tbl z1.b, {z2.b}, z1.b
    tbl z0.b, {z3.b}, z0.b
    cbz x4, 2f
    ld1b z4.b, p0/z, [x1, x5]
    .if \sve2
    eor3 z0.d, z0.d, z1.d, z4.d
    .else
    eor z0.d, z0.d, z1.d
    eor z0.d, z0.d, z4.d
    .endif
    b 4f
2:
    eor z0.d, z0.d, z1.d
4:
    st1b z0.b, p0, [x1, x5]
    incb x5
    cmp x5, x2
    b.lo 1b
3:
    ret
    .size \name, .-\name
    .endm
    ec_sve argosfs_ec_sve_apply, 0
    .arch armv9-a+sve2
    ec_sve argosfs_ec_sve2_apply, 1
    .arch armv8-a+sve
    .global argosfs_ec_sve_bytes
    .hidden argosfs_ec_sve_bytes
    .type argosfs_ec_sve_bytes, %function
argosfs_ec_sve_bytes:
    cntb x0
    ret
    .size argosfs_ec_sve_bytes, .-argosfs_ec_sve_bytes
    .popsection
    "#,
    options(raw)
);

unsafe extern "C" {
    fn argosfs_ec_sve_apply(src: *const u8, dst: *mut u8, len: usize, table: *const u8, add: usize);
    fn argosfs_ec_sve2_apply(
        src: *const u8,
        dst: *mut u8,
        len: usize,
        table: *const u8,
        add: usize,
    );
    fn argosfs_ec_sve_bytes() -> usize;
}

pub(super) fn supported<const SVE2: bool>() -> bool {
    if SVE2 {
        std::arch::is_aarch64_feature_detected!("sve2")
    } else {
        std::arch::is_aarch64_feature_detected!("sve")
    }
}

/// Width is a per-thread Linux setting; the kernel never assumes it is fixed.
pub(super) fn wide_supported() -> bool {
    supported::<false>() && unsafe { argosfs_ec_sve_bytes() } >= 32
}

pub(super) unsafe fn apply<const SVE2: bool, const ADD: bool>(c: u8, src: &[u8], dst: &mut [u8]) {
    assert_eq!(src.len(), dst.len());
    match c {
        0 => {
            if !ADD {
                dst.fill(0);
            }
        }
        1 if !ADD => dst.copy_from_slice(src),
        1 => scalar::mul_add_slice(c, src, dst),
        _ => {
            let table = tables::SPLIT.get(c);
            // SAFETY: the caller checked the appropriate ISA. The assembly
            // predicates each read/write by len, and table has 32 bytes.
            unsafe {
                if SVE2 {
                    argosfs_ec_sve2_apply(
                        src.as_ptr(),
                        dst.as_mut_ptr(),
                        src.len(),
                        table.as_ptr(),
                        ADD as usize,
                    );
                } else {
                    argosfs_ec_sve_apply(
                        src.as_ptr(),
                        dst.as_mut_ptr(),
                        src.len(),
                        table.as_ptr(),
                        ADD as usize,
                    );
                }
            }
        }
    }
}

pub(super) unsafe fn mul_rows<const SVE2: bool, S: ShardSource + ?Sized, O: AsMut<[u8]>>(
    matrix: &[u8],
    k: usize,
    srcs: &S,
    outs: &mut [O],
) {
    let len = srcs.shard(0).len();
    let tile = super::encode_tile_size(k, outs.len());
    for start in (0..len).step_by(tile) {
        let end = (start + tile).min(len);
        for (row, out) in outs.iter_mut().enumerate() {
            let coeffs = &matrix[row * k..(row + 1) * k];
            let out = &mut out.as_mut()[start..end];
            // SAFETY: caller checked the appropriate ISA.
            unsafe {
                apply::<SVE2, false>(coeffs[0], &srcs.shard(0)[start..end], out);
                for (j, &c) in coeffs.iter().enumerate().skip(1) {
                    apply::<SVE2, true>(c, &srcs.shard(j)[start..end], out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn required_vector_length_is_active() {
        let Ok(expected) = std::env::var("RS_REQUIRE_SVE_BYTES") else {
            return;
        };
        assert!(super::supported::<false>());
        let expected: usize = expected.parse().unwrap();
        // SAFETY: SVE is supported on this CPU.
        assert_eq!(unsafe { super::argosfs_ec_sve_bytes() }, expected);
    }
}
