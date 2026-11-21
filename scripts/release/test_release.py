"""Release metadata unit tests (stdlib only, run on every PR)."""
import tempfile
import unittest
from pathlib import Path

from scripts.release import release


class ReleaseTests(unittest.TestCase):
    def test_version_is_taken_from_cargo_manifest(self):
        self.assertEqual(release.verify_version(""), release.manifest_version())
        self.assertEqual(release.verify_version(release.manifest_version()), release.manifest_version())

    def test_rejects_invalid_or_mismatched_versions(self):
        for version in ("v1.2.0", "01.2.0", "1.2", "1.2.0-rc1", "1.2.1;echo unsafe"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.verify_version(version)
        with self.assertRaises(ValueError):
            release.verify_version("999.999.999")

    def test_dist_hashes_are_verified(self):
        version = release.manifest_version()
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            for name in release.expected_assets(version):
                (dist / name).write_bytes(name.encode())
            digests = release.verify_dist(version, dist)
            self.assertEqual(len(digests), 3)
            (dist / "SHA256SUMS").write_text("".join(
                f"{digests[name]}  {name}\n" for name in sorted(digests)
            ))
            self.assertEqual(release.verify_dist(version, dist), digests)
            (dist / release.expected_assets(version)[0]).write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "SHA256SUMS"):
                release.verify_dist(version, dist)

    def test_aur_source_package_has_pinned_archive(self):
        version = release.manifest_version()
        hashes = {key: "a" * 64 for key in release.expected_assets(version)}
        text = release.aur_source(version, hashes)
        self.assertIn("pkgname=argosfs\n", text)
        self.assertIn("cargo build --release --locked --offline", text)
        self.assertIn("cargo fetch --locked", text)
        self.assertIn(f"v${{pkgver}}/argosfs-{version}-source.tar.gz", text)
        self.assertIn("sha256sums=('", text)
        self.assertNotIn("SKIP", text)

    def test_aur_binary_package_covers_both_linux_arches(self):
        version = release.manifest_version()
        hashes = {key: "b" * 64 for key in release.expected_assets(version)}
        text = release.aur_binary(version, hashes)
        self.assertIn("pkgname=argosfs-bin", text)
        self.assertIn("source_x86_64=(", text)
        self.assertIn("source_aarch64=(", text)
        self.assertIn("conflicts=('argosfs')", text)
        self.assertIn("install -Dm755", text)
        self.assertNotIn("SKIP", text)


if __name__ == "__main__":
    unittest.main()
