//! Isolated PlayReady license processing.
//!
//! The parent process owns HTTPS policy and transport. The child loads the
//! provisioned device, creates a challenge, and processes the returned license.
//! Device data and content keys never appear in argv, logs, or persistent output.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read as _, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt as _;
use playready::{Cdm, Device, Pssh};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader as AsyncBufReader,
};
use tokio::process::Command;
use url::Url;
use zeroize::Zeroizing;

const INTERNAL_WORKER_ARG: &str = "--grab-internal-playready-worker";
const MAX_WORKER_FRAME_BYTES: usize = 256 * 1024;
const MAX_DEVICE_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DEVICE_PATH_BYTES: usize = 4096;
const MAX_CHALLENGE_BYTES: usize = 128 * 1024;
const MAX_LICENSE_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_LICENSE_KEYS: usize = 64;
const LICENSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYREADY_SOAP_ACTION: &str =
    "\"http://schemas.microsoft.com/DRM/2007/03/protocols/AcquireLicense\"";
const WORKER_TIMEOUT: Duration = Duration::from_secs(60);
const WORKER_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerRequest {
    Start {
        device_path: PathBuf,
        pssh_base64: String,
    },
    LicenseResponse {
        body: Zeroizing<String>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerResponse {
    Challenge { body: Zeroizing<String> },
    Keys { keys: Vec<WorkerKey> },
    Failure { reason: WorkerFailure },
}

#[derive(Serialize, Deserialize)]
struct WorkerKey {
    kid: [u8; 16],
    key: Zeroizing<[u8; 16]>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkerFailure {
    Protocol,
    InvalidDevice,
    InvalidPssh,
    ChallengeFailed,
    LicenseResponseInvalid,
    KeyMaterialInvalid,
}

impl WorkerFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Protocol => "PlayReady worker protocol failed",
            Self::InvalidDevice => "The configured PlayReady device is invalid or unavailable",
            Self::InvalidPssh => "The selected PlayReady initialization data is invalid",
            Self::ChallengeFailed => "The PlayReady CDM could not create a license challenge",
            Self::LicenseResponseInvalid => "The PlayReady license response could not be processed",
            Self::KeyMaterialInvalid => "The PlayReady CDM returned unsupported key material",
        }
    }
}

/// One PlayReady content key, held in zeroizing memory.
pub(crate) struct PlayReadyKey {
    /// The 16-byte key identifier in ISO Common Encryption byte order.
    pub(crate) kid: [u8; 16],
    /// The 16-byte AES content key. It is zeroized when dropped.
    pub(crate) key: Zeroizing<[u8; 16]>,
}

impl std::fmt::Debug for PlayReadyKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlayReadyKey")
            .field("kid", &self.kid)
            .field("key", &"[redacted]")
            .finish()
    }
}

/// Run the internal worker entry point when the parent starts this executable
/// with its private worker argument.
pub(crate) fn is_worker_requested() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(OsStr::new(INTERNAL_WORKER_ARG))
}

/// Execute the child-side PlayReady protocol. The return value is a process
/// exit code; all worker diagnostics are deliberately reduced to stable codes.
pub(crate) fn run_worker() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    match process_worker(&mut reader, &mut writer) {
        Ok(()) => 0,
        Err(reason) => {
            let _ = write_sync_frame(&mut writer, &WorkerResponse::Failure { reason });
            1
        }
    }
}

/// Acquire PlayReady keys while keeping device parsing and CDM state in a
/// short-lived child process. The supplied HTTP client remains owned by Grab,
/// so its DNS, proxy, TLS, and redirect policy is not bypassed. The caller
/// must also validate the license host with Grab's public-host policy before
/// calling this function.
///
/// # Errors
///
/// Returns an error when the device, PSSH, license endpoint, worker exchange,
/// or bounded license response is invalid.
pub(crate) async fn acquire_keys(
    client: &reqwest::Client,
    device_path: &Path,
    pssh: &[u8],
    license_url: &Url,
) -> Result<Vec<PlayReadyKey>, String> {
    crate::drm::validate_pssh(pssh, crate::drm::ProtectionSystem::PlayReady)?;
    if pssh.len() > crate::drm::MAX_PSSH_BYTES {
        return Err("The PlayReady PSSH exceeds its size limit".to_string());
    }
    if license_url.scheme() != "https"
        || license_url.host_str().is_none()
        || !license_url.username().is_empty()
        || license_url.password().is_some()
        || license_url.fragment().is_some()
    {
        return Err(
            "The PlayReady license URL must be HTTPS without user-info or a fragment".to_string(),
        );
    }

    if device_path.as_os_str().as_encoded_bytes().len() > MAX_DEVICE_PATH_BYTES {
        return Err("The PlayReady device path exceeds its size limit".to_string());
    }

    let executable = std::env::current_exe()
        .map_err(|_| "Could not locate the Grab executable for the PlayReady worker".to_string())?;
    let mut child = Command::new(executable)
        .arg(INTERNAL_WORKER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "Could not start the PlayReady CDM worker".to_string())?;

    let result = acquire_keys_with_child(client, device_path, pssh, license_url, &mut child).await;
    if result.is_err() {
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.start_kill();
        }
        let _ = tokio::time::timeout(WORKER_EXIT_TIMEOUT, child.wait()).await;
    }
    result
}

async fn acquire_keys_with_child(
    client: &reqwest::Client,
    device_path: &Path,
    pssh: &[u8],
    license_url: &Url,
    child: &mut tokio::process::Child,
) -> Result<Vec<PlayReadyKey>, String> {
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Could not open PlayReady worker input".to_string())?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Could not open PlayReady worker output".to_string())?;
    let mut child_stdout = AsyncBufReader::new(child_stdout);

    let start = WorkerRequest::Start {
        device_path: device_path.to_path_buf(),
        pssh_base64: STANDARD.encode(pssh),
    };
    write_async_frame(&mut child_stdin, &start).await?;

    let challenge_response = tokio::time::timeout(
        WORKER_TIMEOUT,
        read_async_message::<_, WorkerResponse>(&mut child_stdout),
    )
    .await
    .map_err(|_| "The PlayReady CDM worker timed out while creating a challenge".to_string())??;
    let challenge = match challenge_response {
        WorkerResponse::Challenge { body } => body,
        WorkerResponse::Failure { reason } => return Err(reason.message().to_string()),
        WorkerResponse::Keys { .. } => {
            return Err("The PlayReady worker returned an invalid protocol response".to_string());
        }
    };
    if challenge.is_empty() || challenge.len() > MAX_CHALLENGE_BYTES {
        return Err("The PlayReady challenge exceeds its size limit".to_string());
    }

    let response = tokio::time::timeout(
        LICENSE_REQUEST_TIMEOUT,
        client
            .post(license_url.clone())
            .timeout(LICENSE_REQUEST_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "text/xml; charset=utf-8")
            .header("SOAPAction", PLAYREADY_SOAP_ACTION)
            .body(challenge.to_string())
            .send(),
    )
    .await
    .map_err(|_| "The PlayReady license request timed out".to_string())?
    .map_err(|error| {
        format!(
            "Could not contact the PlayReady license server: {}",
            error.without_url()
        )
    })?;
    if !response.status().is_success() {
        return Err(format!(
            "The PlayReady license server returned HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_LICENSE_RESPONSE_BYTES as u64)
    {
        return Err("The PlayReady license response exceeds its size limit".to_string());
    }

    let mut response_bytes = Zeroizing::new(Vec::with_capacity(MAX_LICENSE_RESPONSE_BYTES));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout(LICENSE_REQUEST_TIMEOUT, stream.next())
        .await
        .map_err(|_| "The PlayReady license response timed out".to_string())?
    {
        let chunk = chunk.map_err(|error| {
            format!(
                "Could not read the PlayReady license response: {}",
                error.without_url()
            )
        })?;
        let next_len = response_bytes
            .len()
            .checked_add(chunk.len())
            .filter(|length| *length <= MAX_LICENSE_RESPONSE_BYTES)
            .ok_or_else(|| "The PlayReady license response exceeds its size limit".to_string())?;
        response_bytes.extend_from_slice(&chunk);
        debug_assert_eq!(response_bytes.len(), next_len);
    }
    let response_text = std::str::from_utf8(&response_bytes)
        .map_err(|_| "The PlayReady license response is not valid UTF-8".to_string())?;
    let license_response = WorkerRequest::LicenseResponse {
        body: Zeroizing::new(response_text.to_owned()),
    };
    write_async_frame(&mut child_stdin, &license_response).await?;
    drop(license_response);
    drop(child_stdin);

    let keys_response = tokio::time::timeout(
        WORKER_TIMEOUT,
        read_async_message::<_, WorkerResponse>(&mut child_stdout),
    )
    .await
    .map_err(|_| "The PlayReady CDM worker timed out while processing the license".to_string())??;
    let keys = match keys_response {
        WorkerResponse::Keys { keys } => keys,
        WorkerResponse::Failure { reason } => return Err(reason.message().to_string()),
        WorkerResponse::Challenge { .. } => {
            return Err("The PlayReady worker returned an invalid protocol response".to_string());
        }
    };

    let status = tokio::time::timeout(WORKER_EXIT_TIMEOUT, child.wait())
        .await
        .map_err(|_| "The PlayReady CDM worker did not exit cleanly".to_string())?
        .map_err(|_| "Could not read the PlayReady CDM worker exit status".to_string())?;
    if !status.success() {
        return Err("The PlayReady CDM worker exited unsuccessfully".to_string());
    }

    if keys.is_empty() || keys.len() > MAX_LICENSE_KEYS {
        return Err("The PlayReady license returned an unsupported key count".to_string());
    }
    let mut seen = BTreeSet::new();
    let mut output = Vec::with_capacity(keys.len());
    for key in keys {
        if !seen.insert(key.kid) {
            return Err("The PlayReady license returned a duplicate key identifier".to_string());
        }
        output.push(PlayReadyKey {
            kid: key.kid,
            key: key.key,
        });
    }
    Ok(output)
}

fn process_worker<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> Result<(), WorkerFailure> {
    let request: WorkerRequest = read_sync_message(reader)?;
    let WorkerRequest::Start {
        device_path,
        pssh_base64,
    } = request
    else {
        return Err(WorkerFailure::Protocol);
    };

    if device_path.as_os_str().as_encoded_bytes().len() > MAX_DEVICE_PATH_BYTES {
        return Err(WorkerFailure::InvalidDevice);
    }
    let device_file = fs::File::open(&device_path).map_err(|_| WorkerFailure::InvalidDevice)?;
    let metadata = device_file
        .metadata()
        .map_err(|_| WorkerFailure::InvalidDevice)?;
    if !metadata.is_file() || metadata.len() > MAX_DEVICE_FILE_BYTES {
        return Err(WorkerFailure::InvalidDevice);
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| WorkerFailure::InvalidDevice)?;
    let mut device_bytes = Zeroizing::new(Vec::with_capacity(capacity));
    let mut bounded_device_file = device_file.take(MAX_DEVICE_FILE_BYTES + 1);
    bounded_device_file
        .read_to_end(&mut device_bytes)
        .map_err(|_| WorkerFailure::InvalidDevice)?;
    if device_bytes.len() as u64 > MAX_DEVICE_FILE_BYTES {
        return Err(WorkerFailure::InvalidDevice);
    }
    let device = Device::from_bytes(&device_bytes).map_err(|_| WorkerFailure::InvalidDevice)?;
    device.verify().map_err(|_| WorkerFailure::InvalidDevice)?;

    let pssh_bytes = Zeroizing::new(
        STANDARD
            .decode(pssh_base64.as_bytes())
            .map_err(|_| WorkerFailure::InvalidPssh)?,
    );
    if pssh_bytes.is_empty() || pssh_bytes.len() > crate::drm::MAX_PSSH_BYTES {
        return Err(WorkerFailure::InvalidPssh);
    }
    crate::drm::validate_pssh(&pssh_bytes, crate::drm::ProtectionSystem::PlayReady)
        .map_err(|_| WorkerFailure::InvalidPssh)?;
    let pssh = Pssh::from_bytes(&pssh_bytes).map_err(|_| WorkerFailure::InvalidPssh)?;
    let mut headers = pssh.wrm_headers().into_iter();
    let wrm_header = headers.next().ok_or(WorkerFailure::InvalidPssh)?;
    if headers.next().is_some() {
        return Err(WorkerFailure::InvalidPssh);
    }

    let cdm = Cdm::from_device(device);
    let challenge = Zeroizing::new(
        cdm.get_license_challenge(wrm_header, None)
            .map_err(|_| WorkerFailure::ChallengeFailed)?,
    );
    if challenge.is_empty() || challenge.len() > MAX_CHALLENGE_BYTES {
        return Err(WorkerFailure::ChallengeFailed);
    }
    write_sync_frame(writer, &WorkerResponse::Challenge { body: challenge })?;

    let request: WorkerRequest = read_sync_message(reader)?;
    let WorkerRequest::LicenseResponse { body } = request else {
        return Err(WorkerFailure::Protocol);
    };
    if body.is_empty() || body.len() > MAX_LICENSE_RESPONSE_BYTES {
        return Err(WorkerFailure::LicenseResponseInvalid);
    }
    let keys = cdm
        .get_keys_from_challenge_response(body.as_str())
        .map_err(|_| WorkerFailure::LicenseResponseInvalid)?;
    // The crate returns ordinary Box allocations. Move every key into zeroizing
    // storage before count/length checks so all early-return paths wipe the keys.
    let keys: Vec<([u8; 16], Zeroizing<Box<[u8]>>)> = keys
        .into_iter()
        .map(|(kid, key)| (kid.into(), Zeroizing::new(key.into())))
        .collect();
    if keys.is_empty() || keys.len() > MAX_LICENSE_KEYS {
        return Err(WorkerFailure::KeyMaterialInvalid);
    }

    let mut output = Vec::with_capacity(keys.len());
    for (kid, key_bytes) in keys {
        let key = copy_content_key(&key_bytes)?;
        output.push(WorkerKey { kid, key });
    }
    write_sync_frame(writer, &WorkerResponse::Keys { keys: output })
}

fn copy_content_key(bytes: &[u8]) -> Result<Zeroizing<[u8; 16]>, WorkerFailure> {
    let key: [u8; 16] = bytes
        .try_into()
        .map_err(|_| WorkerFailure::KeyMaterialInvalid)?;
    Ok(Zeroizing::new(key))
}

fn read_sync_frame<R: BufRead>(reader: &mut R) -> Result<Zeroizing<Vec<u8>>, WorkerFailure> {
    let mut frame = Zeroizing::new(Vec::new());
    loop {
        let available = reader.fill_buf().map_err(|_| WorkerFailure::Protocol)?;
        if available.is_empty() {
            return Err(WorkerFailure::Protocol);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(available.len());
        if frame
            .len()
            .checked_add(length)
            .is_none_or(|length| length > MAX_WORKER_FRAME_BYTES)
        {
            return Err(WorkerFailure::Protocol);
        }
        frame.extend_from_slice(&available[..length]);
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(frame);
        }
    }
}

fn read_sync_message<R: BufRead, T: DeserializeOwned>(reader: &mut R) -> Result<T, WorkerFailure> {
    let frame = read_sync_frame(reader)?;
    serde_json::from_slice(&frame).map_err(|_| WorkerFailure::Protocol)
}

fn write_sync_frame<W: Write, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), WorkerFailure> {
    let frame = Zeroizing::new(serde_json::to_vec(message).map_err(|_| WorkerFailure::Protocol)?);
    if frame.len() > MAX_WORKER_FRAME_BYTES {
        return Err(WorkerFailure::Protocol);
    }
    writer
        .write_all(&frame)
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|_| WorkerFailure::Protocol)
}

async fn read_async_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut frame = Zeroizing::new(Vec::new());
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|_| "Could not read the PlayReady worker response".to_string())?;
        if available.is_empty() {
            return Err("The PlayReady worker closed its output unexpectedly".to_string());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(available.len());
        if frame
            .len()
            .checked_add(length)
            .is_none_or(|length| length > MAX_WORKER_FRAME_BYTES)
        {
            return Err("The PlayReady worker response exceeds its size limit".to_string());
        }
        frame.extend_from_slice(&available[..length]);
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(frame);
        }
    }
}

async fn read_async_message<R: AsyncBufRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<T, String> {
    let frame = read_async_frame(reader).await?;
    serde_json::from_slice(&frame)
        .map_err(|_| "The PlayReady worker returned an invalid message".to_string())
}

async fn write_async_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), String> {
    let frame = Zeroizing::new(
        serde_json::to_vec(message)
            .map_err(|_| "Could not encode the PlayReady worker message".to_string())?,
    );
    if frame.len() > MAX_WORKER_FRAME_BYTES {
        return Err("The PlayReady worker message exceeds its size limit".to_string());
    }
    writer
        .write_all(&frame)
        .await
        .map_err(|_| "Could not write the PlayReady worker message".to_string())?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|_| "Could not terminate the PlayReady worker message".to_string())?;
    writer
        .flush()
        .await
        .map_err(|_| "Could not flush the PlayReady worker message".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_only_aes128_content_keys_into_zeroizing_storage() {
        let key = [0x5a; 16];
        let copied = copy_content_key(&key).expect("16-byte content key should be accepted");
        assert_eq!(*copied, key);

        for invalid in [&[][..], &[0x11; 15][..], &[0x22; 17][..]] {
            assert!(matches!(
                copy_content_key(invalid),
                Err(WorkerFailure::KeyMaterialInvalid)
            ));
        }
    }

    #[test]
    fn worker_frame_reader_enforces_its_limit() {
        let input = vec![b'x'; MAX_WORKER_FRAME_BYTES + 1];
        let mut reader = BufReader::new(input.as_slice());
        assert!(matches!(
            read_sync_frame(&mut reader),
            Err(WorkerFailure::Protocol)
        ));
    }

    #[test]
    fn worker_frame_reader_requires_a_complete_line() {
        let input = b"{\"type\":\"start\"}";
        let mut reader = BufReader::new(input.as_slice());
        assert!(matches!(
            read_sync_frame(&mut reader),
            Err(WorkerFailure::Protocol)
        ));
    }

    #[test]
    fn license_response_frame_roundtrips_in_zeroizing_buffers() {
        let request = WorkerRequest::LicenseResponse {
            body: Zeroizing::new("<LicenseResponse />".to_string()),
        };
        let mut encoded = Vec::new();
        write_sync_frame(&mut encoded, &request).expect("worker frame should serialize");
        let frame = Zeroizing::new(encoded);
        let mut reader = BufReader::new(frame.as_slice());
        let decoded: WorkerRequest =
            read_sync_message(&mut reader).expect("worker frame should deserialize");
        let WorkerRequest::LicenseResponse { body } = decoded else {
            panic!("worker request type should be retained");
        };
        assert_eq!(body.as_str(), "<LicenseResponse />");
    }
}
