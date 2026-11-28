use argosfs::ArgosFs;
use std::process::Command;

#[test]
fn cli_new_pool_format_defaults_to_paged_and_retains_legacy() {
    for (flag, paged) in [
        (None, true),
        (Some("--paged-metadata"), true),
        (Some("--legacy-metadata"), false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("member.img");
        let output = Command::new(env!("CARGO_BIN_EXE_argosfs"))
            .args(["mkfs", "--images"])
            .arg(&image)
            .args(["--image-size", "64MiB", "--k", "1", "--m", "0"])
            .args(flag)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pool = ArgosFs::open_loop(std::slice::from_ref(&image), true).unwrap();
        assert_eq!(
            pool.transaction_report().unwrap().selected_metadata_source == "raw-paged-metadata",
            paged
        );
        pool.write_file("/data", b"preserved", 0o600).unwrap();
        pool.sync().unwrap();
        pool.mark_clean_unmount().unwrap();
        drop(pool);
        if paged {
            let output = Command::new(env!("CARGO_BIN_EXE_argosfs"))
                .args(["upgrade-integrity", "--images"])
                .arg(&image)
                .arg("--offline-confirmed")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["upgraded"], false);
            assert_eq!(report["already_paged"], true);
        }
        let pool = ArgosFs::open_loop(&[image], false).unwrap();
        assert_eq!(
            pool.transaction_report().unwrap().selected_metadata_source == "raw-paged-metadata",
            paged
        );
        assert_eq!(pool.read_file("/data", false).unwrap(), b"preserved");
    }
}

#[test]
fn raw_cli_uses_the_same_format_policy() {
    // File-backed raw devices exercise the real raw backend without requiring
    // privileged device access or touching any physical disks.
    for (flag, paged) in [(None, true), (Some("--legacy-metadata"), false)] {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("raw.img");
        std::fs::File::create(&image)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_argosfs"))
            .args(["mkfs", "--devices"])
            .arg(&image)
            .args(["--k", "1", "--m", "0"])
            .args(flag)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pool = ArgosFs::open_raw(std::slice::from_ref(&image), true).unwrap();
        assert_eq!(
            pool.transaction_report().unwrap().selected_metadata_source == "raw-paged-metadata",
            paged
        );
        pool.write_file("/data", b"raw data", 0o600).unwrap();
        pool.sync().unwrap();
        pool.mark_clean_unmount().unwrap();
        drop(pool);
        let pool = ArgosFs::open_raw(&[image], false).unwrap();
        assert_eq!(pool.read_file("/data", false).unwrap(), b"raw data");
        assert_eq!(
            pool.transaction_report().unwrap().selected_metadata_source == "raw-paged-metadata",
            paged
        );
    }
}

#[test]
fn host_rejects_raw_format_options_before_creation() {
    for flag in ["--paged-metadata", "--legacy-metadata"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("host");
        let output = Command::new(env!("CARGO_BIN_EXE_argosfs"))
            .arg("mkfs")
            .arg(&root)
            .arg(flag)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(flag));
        assert!(!root.exists());
    }
}
