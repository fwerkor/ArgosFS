from pathlib import Path
import re


def sub_once(path: str, pattern: str, replacement: str, flags: int = 0) -> None:
    file = Path(path)
    text = file.read_text()
    updated, count = re.subn(pattern, replacement, text, count=1, flags=flags)
    if count != 1:
        raise SystemExit(
            f"{path}: expected one replacement, found {count}: {pattern}"
        )
    file.write_text(updated)


store_path = "src/storage/raw/store.rs"
sub_once(
    store_path,
    r"pub fn append_transaction_with_previous\(\n"
    r"    backend: &dyn StorageBackend,\n"
    r"    superblocks: &\[RawSuperblock\],\n"
    r"    metadata: &Metadata,\n"
    r"    previous_metadata: Option<&Metadata>,\n"
    r"    action: &str,\n"
    r"    details: serde_json::Value,\n"
    r"\) -> Result<\(\)> \{\n"
    r"    append_journal\(\n"
    r"        backend,\n"
    r"        superblocks,\n"
    r"        metadata,\n"
    r"        previous_metadata,\n"
    r"        action,\n"
    r"        details,\n"
    r"    \)\?;\n"
    r"    journal::inject_crash\(FaultPoint::AfterJournalCommitBeforeMetadataCommit\.as_str\(\)\)\?;\n"
    r"    Ok\(\(\)\)\n"
    r"\}\n",
    """pub fn append_transaction_with_previous(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    append_journal(
        backend,
        superblocks,
        metadata,
        previous_metadata,
        action,
        details,
    )?;
    journal::inject_crash(FaultPoint::AfterJournalCommitBeforeMetadataCommit.as_str())?;
    Ok(())
}

pub(crate) fn append_transaction_with_trusted_previous(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    append_journal_trusted(
        backend,
        superblocks,
        metadata,
        previous_metadata,
        action,
        details,
    )?;
    journal::inject_crash(FaultPoint::AfterJournalCommitBeforeMetadataCommit.as_str())?;
    Ok(())
}
""",
)

sub_once(
    store_path,
    r"fn append_journal\(\n"
    r"    backend: &dyn StorageBackend,\n"
    r"    superblocks: &\[RawSuperblock\],\n"
    r"    metadata: &Metadata,\n"
    r"    previous_metadata: Option<&Metadata>,\n"
    r"    action: &str,\n"
    r"    details: serde_json::Value,\n"
    r"\) -> Result<\(\)> \{\n"
    r"    let delta_base = match previous_metadata \{\n"
    r"        Some\(previous\)\n"
    r"            if journal::canonical_metadata_hash\(previous\)\?\n"
    r"                == metadata\.integrity\.previous_meta_hash =>\n"
    r"        \{\n"
    r"            Some\(previous\)\n"
    r"        \}\n"
    r"        _ => None,\n"
    r"    \};\n",
    """fn append_journal(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    append_journal_with_previous_validation(
        backend,
        superblocks,
        metadata,
        previous_metadata,
        action,
        details,
        false,
    )
}

fn append_journal_trusted(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    append_journal_with_previous_validation(
        backend,
        superblocks,
        metadata,
        previous_metadata,
        action,
        details,
        true,
    )
}

fn append_journal_with_previous_validation(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
    trust_previous_integrity: bool,
) -> Result<()> {
    let delta_base = match previous_metadata {
        Some(previous)
            if previous.integrity.meta_hash == metadata.integrity.previous_meta_hash
                && (trust_previous_integrity
                    || journal::canonical_metadata_hash(previous)?
                        == metadata.integrity.previous_meta_hash) =>
        {
            Some(previous)
        }
        _ => None,
    };
""",
)

volume_path = "src/volume/mod.rs"
volume = Path(volume_path).read_text()
volume, count = re.subn(
    r"raw_store::append_transaction_with_previous\(",
    "raw_store::append_transaction_with_trusted_previous(",
    volume,
)
if count < 3:
    raise SystemExit(
        f"{volume_path}: expected at least 3 trusted raw transaction call sites, found {count}"
    )
Path(volume_path).write_text(volume)

sub_once(
    volume_path,
    r"            let replay_previous = match previous_metadata \{\n"
    r"                Some\(previous\)\n"
    r"                    if journal::canonical_metadata_hash\(previous\)\? == previous_meta_hash =>\n"
    r"                \{\n"
    r"                    Some\(previous\)\n"
    r"                \}\n"
    r"                _ => None,\n"
    r"            \};",
    """            let replay_previous = match previous_metadata {
                Some(previous) if previous.integrity.meta_hash == previous_meta_hash => {
                    Some(previous)
                }
                _ => None,
            };""",
)
