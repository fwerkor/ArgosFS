# Batched raw checkpoint recovery reads

Raw checkpoints keep a page-index with a per-page SHA-256 digest and a
full stored-payload digest. The index describes fixed contiguous page offsets
within each metadata slot. Recovery previously issued one independent physical
read for each metadata page (normally 4 KiB) for **every** candidate slot on
**every** member, even if the same checkpoint bytes had already been decoded
from another member.

Recovery now validates the existing index extents and fetches groups of
contiguous pages using at most 1 MiB per read. It validates the same per-page
SHA-256 values after each batch and verifies the full stored payload digest
before decoding. The existing metadata payload vector is expanded in place;
no additional full-size buffer is allocated. Other valid page sizes remain
supported, limited by the existing 1 MiB maximum.

The checkpoint wire format, quorum selection, both physical slot checks,
compressed decode and fallback/recovery ordering are unchanged.

A synthetic 513-page checkpoint needs 3 physical data reads, compared with
513 previously. An injected corruption in page 300 still reports that exact
page, and invalid page-index lengths are rejected before body reads. This is
a **syscall-count** optimization, not a measured nine-member wall-clock
improvement. Production retesting should compare mount readiness, block reads,
metadata recovery CPU, and cold Borg throughput on the 294k-inode pool.
