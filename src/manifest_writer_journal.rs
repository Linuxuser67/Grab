//! Fixed-size, append-only digest records for ordered manifest track recovery.

use std::path::Path;

use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

use super::{
    CommittedResource, MAX_RESOURCE_BYTES, MAX_TRACK_BYTES, digest_hex, hash_file_range,
    remove_regular_file_if_present, sync_directory,
};

// Each record is an 8-byte big-endian length followed by 32 raw SHA-256 bytes.
const RESOURCE_RECORD_BYTES: usize = 40;
// 100,001 live-manifest resources need about 4 MiB at 40 bytes per record.
const MAX_RESOURCE_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;

/// Load a checkpointed prefix, rebuilding the journal only from retained sources.
///
/// # Errors
///
/// Returns an error if the journal and staged sources cannot establish the same prefix.
pub(super) async fn load_resource_records(
    journal_path: &Path,
    directory: &Path,
    resources: &[String],
    resource_count: usize,
    committed_offset: u64,
    truncate_uncommitted_tail: bool,
) -> Result<Vec<CommittedResource>, String> {
    match tokio::fs::symlink_metadata(journal_path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("The manifest resource digest journal is not a regular file".to_string());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect {}: {error}",
                journal_path.display()
            ));
        }
    }

    if let Ok(records) = read_resource_journal(
        journal_path,
        directory,
        resource_count,
        truncate_uncommitted_tail,
    )
    .await
        && validate_committed_resources(&records, resource_count, committed_offset).is_ok()
    {
        return Ok(records);
    }

    // Rebuild a lost or damaged journal only when every committed source is
    // still present. The caller then verifies the output byte ranges before
    // any source cleanup can continue.
    let mut rebuilt = Vec::with_capacity(resource_count);
    for resource_name in resources.iter().take(resource_count) {
        let source = directory.join(resource_name);
        let metadata = tokio::fs::symlink_metadata(&source)
            .await
            .map_err(|error| {
                format!(
                    "Could not read {} to rebuild its manifest digest: {error}",
                    source.display()
                )
            })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() > MAX_RESOURCE_BYTES
        {
            return Err("A staged manifest resource cannot rebuild its committed digest".to_string());
        }
        rebuilt.push(CommittedResource {
            length: metadata.len(),
            sha256: hash_file_range(&source, 0, metadata.len()).await?,
        });
    }
    validate_committed_resources(&rebuilt, resource_count, committed_offset)?;
    write_resource_journal(journal_path, directory, &rebuilt).await?;
    Ok(rebuilt)
}

/// Read exactly the checkpointed number of fixed-size digest records.
///
/// # Errors
///
/// Returns an error if the journal is missing, malformed, or shorter than the prefix.
pub(super) async fn read_resource_journal(
    journal_path: &Path,
    directory: &Path,
    resource_count: usize,
    truncate_uncommitted_tail: bool,
) -> Result<Vec<CommittedResource>, String> {
    let record_count = u64::try_from(resource_count)
        .map_err(|_| "The manifest resource count is too large".to_string())?;
    let expected_length = record_count
        .checked_mul(RESOURCE_RECORD_BYTES as u64)
        .filter(|length| *length <= MAX_RESOURCE_JOURNAL_BYTES)
        .ok_or_else(|| "The manifest resource digest journal exceeds its size limit".to_string())?;

    let metadata = match tokio::fs::symlink_metadata(journal_path).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("The manifest resource digest journal is not a regular file".to_string());
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && resource_count == 0 => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("The manifest resource digest journal is missing".to_string());
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect {}: {error}",
                journal_path.display()
            ));
        }
    };

    if let Some(metadata) = metadata {
        if metadata.len() < expected_length {
            return Err(
                "The manifest resource digest journal is shorter than its checkpoint".to_string(),
            );
        }
        if metadata.len() > expected_length {
            if !truncate_uncommitted_tail {
                return Err(
                    "The manifest resource digest journal contains uncommitted records".to_string(),
                );
            }
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(journal_path)
                .await
                .map_err(|error| format!("Could not open {}: {error}", journal_path.display()))?;
            file.set_len(expected_length).await.map_err(|error| {
                format!(
                    "Could not truncate uncommitted records in {}: {error}",
                    journal_path.display()
                )
            })?;
            file.sync_all()
                .await
                .map_err(|error| format!("Could not sync {}: {error}", journal_path.display()))?;
            sync_directory(directory).await?;
        }
    }

    let mut records = Vec::with_capacity(resource_count);
    if resource_count == 0 {
        return Ok(records);
    }
    let mut file = tokio::fs::File::open(journal_path)
        .await
        .map_err(|error| format!("Could not open {}: {error}", journal_path.display()))?;
    for _ in 0..resource_count {
        let mut record = [0_u8; RESOURCE_RECORD_BYTES];
        file.read_exact(&mut record).await.map_err(|error| {
            format!(
                "Could not read a digest record from {}: {error}",
                journal_path.display()
            )
        })?;
        let mut length_bytes = [0_u8; 8];
        length_bytes.copy_from_slice(&record[..8]);
        records.push(CommittedResource {
            length: u64::from_be_bytes(length_bytes),
            sha256: digest_hex(&record[8..]),
        });
    }
    Ok(records)
}

/// Append and sync one resource digest record at its manifest-order position.
///
/// # Errors
///
/// Returns an error if the journal position or file operations are invalid.
pub(super) async fn append_resource_record(
    journal_path: &Path,
    resource_index: usize,
    resource: &CommittedResource,
) -> Result<(), String> {
    let index = u64::try_from(resource_index)
        .map_err(|_| "The manifest resource index is too large".to_string())?;
    let expected_offset = index
        .checked_mul(RESOURCE_RECORD_BYTES as u64)
        .ok_or_else(|| "The manifest resource digest journal exceeds its size limit".to_string())?;
    let next_length = expected_offset
        .checked_add(RESOURCE_RECORD_BYTES as u64)
        .filter(|length| *length <= MAX_RESOURCE_JOURNAL_BYTES)
        .ok_or_else(|| "The manifest resource digest journal exceeds its size limit".to_string())?;

    match tokio::fs::symlink_metadata(journal_path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("The manifest resource digest journal is not a regular file".to_string());
        }
        Ok(metadata) if metadata.len() < expected_offset => {
            return Err(
                "The manifest resource digest journal is shorter than its checkpoint".to_string(),
            );
        }
        Ok(metadata) if metadata.len() > expected_offset => {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(journal_path)
                .await
                .map_err(|error| format!("Could not open {}: {error}", journal_path.display()))?;
            file.set_len(expected_offset).await.map_err(|error| {
                format!(
                    "Could not truncate uncommitted records in {}: {error}",
                    journal_path.display()
                )
            })?;
            file.sync_all()
                .await
                .map_err(|error| format!("Could not sync {}: {error}", journal_path.display()))?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && resource_index == 0 => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("The manifest resource digest journal is missing".to_string());
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect {}: {error}",
                journal_path.display()
            ));
        }
    }

    let record = encode_resource_record(resource)?;
    debug_assert_eq!(
        u64::try_from(record.len()).ok(),
        Some(next_length - expected_offset)
    );
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(journal_path)
        .await
        .map_err(|error| format!("Could not open {}: {error}", journal_path.display()))?;
    file.set_len(expected_offset)
        .await
        .map_err(|error| format!("Could not position {}: {error}", journal_path.display()))?;
    file.seek(std::io::SeekFrom::Start(expected_offset))
        .await
        .map_err(|error| format!("Could not seek in {}: {error}", journal_path.display()))?;
    file.write_all(&record).await.map_err(|error| {
        format!(
            "Could not append a digest record to {}: {error}",
            journal_path.display()
        )
    })?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not sync the manifest resource digest record: {error}"))?;
    Ok(())
}

/// Atomically replace the journal with a complete legacy-derived prefix.
///
/// # Errors
///
/// Returns an error if the record set exceeds limits or durable replacement fails.
pub(super) async fn write_resource_journal(
    journal_path: &Path,
    directory: &Path,
    resources: &[CommittedResource],
) -> Result<(), String> {
    let record_count = u64::try_from(resources.len())
        .map_err(|_| "The manifest resource count is too large".to_string())?;
    let _journal_length = record_count
        .checked_mul(RESOURCE_RECORD_BYTES as u64)
        .filter(|length| *length <= MAX_RESOURCE_JOURNAL_BYTES)
        .ok_or_else(|| "The manifest resource digest journal exceeds its size limit".to_string())?;
    let journal_name = journal_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The manifest resource journal filename is invalid".to_string())?;
    let temporary = directory.join(format!("{journal_name}.tmp"));
    remove_regular_file_if_present(&temporary).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(|error| format!("Could not create {}: {error}", temporary.display()))?;
    for resource in resources {
        let record = encode_resource_record(resource)?;
        file.write_all(&record)
            .await
            .map_err(|error| format!("Could not write {}: {error}", temporary.display()))?;
    }
    file.sync_all()
        .await
        .map_err(|error| format!("Could not sync {}: {error}", temporary.display()))?;
    drop(file);
    tokio::fs::rename(&temporary, journal_path)
        .await
        .map_err(|error| format!("Could not publish {}: {error}", journal_path.display()))?;
    sync_directory(directory).await
}

fn encode_resource_record(
    resource: &CommittedResource,
) -> Result<[u8; RESOURCE_RECORD_BYTES], String> {
    if resource.length == 0 || resource.length > MAX_RESOURCE_BYTES {
        return Err("A manifest resource digest record has an invalid length".to_string());
    }
    let digest = parse_digest(&resource.sha256)?;
    let mut record = [0_u8; RESOURCE_RECORD_BYTES];
    record[..8].copy_from_slice(&resource.length.to_be_bytes());
    record[8..].copy_from_slice(&digest);
    Ok(record)
}

fn parse_digest(value: &str) -> Result<[u8; 32], String> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return Err("A manifest resource digest is not a SHA-256 value".to_string());
    }
    let mut digest = [0_u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        let high = hex_nibble(bytes[index * 2])
            .ok_or_else(|| "A manifest resource digest is invalid".to_string())?;
        let low = hex_nibble(bytes[index * 2 + 1])
            .ok_or_else(|| "A manifest resource digest is invalid".to_string())?;
        *byte = (high << 4) | low;
    }
    Ok(digest)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_committed_resources(
    resources: &[CommittedResource],
    resource_count: usize,
    committed_offset: u64,
) -> Result<(), String> {
    if resources.len() != resource_count {
        return Err("The manifest resource digest journal has an invalid record count".to_string());
    }
    let mut offset = 0_u64;
    for resource in resources {
        if resource.length == 0
            || resource.length > MAX_RESOURCE_BYTES
            || parse_digest(&resource.sha256).is_err()
        {
            return Err("The manifest resource digest journal contains invalid metadata".to_string());
        }
        offset = offset
            .checked_add(resource.length)
            .filter(|value| *value <= MAX_TRACK_BYTES)
            .ok_or_else(|| {
                "The committed manifest resource prefix exceeds the 1 TiB limit".to_string()
            })?;
    }
    if offset != committed_offset {
        return Err(
            "The manifest checkpoint offset does not match its resource digest journal".to_string(),
        );
    }
    Ok(())
}
