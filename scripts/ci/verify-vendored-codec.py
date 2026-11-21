#!/usr/bin/env python3
"""Guard the audited Reed-Solomon vendor source and declared version.

The hashes pin every production Rust source file to the upstream 0.2.0
release, without introducing a network dependency into normal CI.
"""
from __future__ import annotations

import hashlib
import pathlib
import tomllib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
VENDOR = ROOT / "vendor" / "reed-solomon-engine"
KNOWN_SHA256 = VENDOR / "CODE_HASHES.sha256"
# SHA-256 of the upstream source manifest; a vendor import requires code review.
UPSTREAM_MANIFEST_SHA256 = "f42ea94393788a953ac0db426189336cbe36673ee304ed8184aa305133d63e69"


def fail(message: str) -> None:
    raise SystemExit(f"vendored codec verification failed: {message}")


def main() -> None:
    manifest_bytes = KNOWN_SHA256.read_bytes()
    if hashlib.sha256(manifest_bytes).hexdigest() != UPSTREAM_MANIFEST_SHA256:
        fail("source checksum manifest changed without reviewed pin update")
    lines = manifest_bytes.decode("utf-8").splitlines()
    expected = {}
    for line in lines:
        checksum, marker, relpath = line.partition("  ")
        if (
            not marker
            or len(checksum) != 64
            or any(ch not in "0123456789abcdef" for ch in checksum)
            or not relpath.startswith("src/")
            or not relpath.endswith(".rs")
        ):
            fail(f"invalid digest entry: {line!r}")
        if relpath in expected or ".." in pathlib.PurePosixPath(relpath).parts:
            fail(f"invalid or duplicate source path: {relpath}")
        expected[relpath] = checksum
    actual = {
        path.relative_to(VENDOR).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in (VENDOR / "src").rglob("*.rs")
    }
    if actual != expected:
        for path in sorted(actual.keys() | expected.keys()):
            if actual.get(path) != expected.get(path):
                print(f"source mismatch: {path}", file=sys.stderr)
        fail("vendored source differs from the pinned production code")
    if not actual:
        fail("no vendored Rust sources found")

    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    dep = manifest["dependencies"].get("reed-solomon-engine")
    if dep != {"version": "=0.2.0", "path": "vendor/reed-solomon-engine"}:
        fail("Cargo.toml must pin both vendor path and exact registry version 0.2.0")
    if sorted(manifest["workspace"]["exclude"]) != ["fuzz", "vendor/reed-solomon-engine"]:
        fail("fuzz and vendor must not be workspace members")

    vendor_manifest = tomllib.loads((VENDOR / "Cargo.toml").read_text(encoding="utf-8"))
    package = vendor_manifest["package"]
    if package["name"] != "reed-solomon-engine" or package["version"] != "0.2.0":
        fail("vendored crate identity mismatch")
    if package.get("publish") is not False:
        fail("vendor must not be published independently")
    if vendor_manifest.get("dependencies"):
        fail("vendored crate unexpectedly acquired a runtime dependency")
    for item in ("LICENSE-APACHE", "LICENSE-MIT", "docs/WIRE_FORMAT.md"):
        if not (VENDOR / item).is_file():
            fail(f"vendor missing notice/documentation: {item}")

    print(f"Verified {len(expected)} unchanged vendored Rust source files (0.2.0)")


if __name__ == "__main__":
    main()
