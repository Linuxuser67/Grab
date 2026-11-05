//! Strict fragmented-MP4 validation and decryption for native DASH tracks.

use std::io::{Cursor, Write};
use std::path::Path;

use crate::bounded_file::read_regular_file_bounded;
use crate::manifest::dash::{DashResource, DashResourceKind};

use super::{DashEncryption, EncryptionScheme, hex_string};

const MAX_INIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_FRAGMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SAMPLES_PER_FRAGMENT: usize = 50_000;
const MAX_TRACK_BYTES: u64 = 1 << 40;

#[derive(Clone, Copy, Debug)]
struct BoxSpan {
    kind: [u8; 4],
    start: usize,
    size: usize,
    header: usize,
}

impl BoxSpan {
    fn payload_start(self) -> Result<usize, String> {
        self.start
            .checked_add(self.header)
            .ok_or_else(|| "MP4 box payload offset overflows".to_string())
    }

    fn end(self) -> Result<usize, String> {
        self.start
            .checked_add(self.size)
            .ok_or_else(|| "MP4 box end offset overflows".to_string())
    }
}

fn parse_box_spans(bytes: &[u8], start: usize, end: usize) -> Result<Vec<BoxSpan>, String> {
    if start > end || end > bytes.len() {
        return Err("MP4 box range is outside its resource".to_string());
    }
    let mut spans = Vec::new();
    let mut offset = start;
    while offset < end {
        let remaining = end - offset;
        if remaining < 8 {
            return Err("MP4 resource ends inside a box header".to_string());
        }
        let size32 = read_u32(bytes, offset)?;
        let mut header = 8_usize;
        let size = if size32 == 1 {
            header = 16;
            usize::try_from(read_u64(bytes, offset + 8)?)
                .map_err(|_| "MP4 extended box size is out of range".to_string())?
        } else if size32 == 0 {
            return Err("MP4 boxes with size zero are not supported in encrypted resources".to_string());
        } else {
            usize::try_from(size32).map_err(|_| "MP4 box size is out of range".to_string())?
        };
        if size < header || size > remaining {
            return Err("MP4 box size is invalid or exceeds its resource".to_string());
        }
        let mut kind = [0_u8; 4];
        kind.copy_from_slice(&bytes[offset + 4..offset + 8]);
        spans.push(BoxSpan {
            kind,
            start: offset,
            size,
            header,
        });
        offset = offset
            .checked_add(size)
            .ok_or_else(|| "MP4 box offset overflows".to_string())?;
    }
    if offset != end {
        return Err("MP4 boxes do not cover their resource".to_string());
    }
    Ok(spans)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| "MP4 field offset overflows".to_string())?;
    let data = bytes
        .get(offset..end)
        .ok_or_else(|| "MP4 field is truncated".to_string())?;
    Ok(u16::from_be_bytes([data[0], data[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "MP4 field offset overflows".to_string())?;
    let data = bytes
        .get(offset..end)
        .ok_or_else(|| "MP4 field is truncated".to_string())?;
    Ok(u32::from_be_bytes([data[0], data[1], data[2], data[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| "MP4 field offset overflows".to_string())?;
    let data = bytes
        .get(offset..end)
        .ok_or_else(|| "MP4 field is truncated".to_string())?;
    Ok(u64::from_be_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, String> {
    Ok(i32::from_be_bytes(read_u32(bytes, offset)?.to_be_bytes()))
}

fn span_payload<'a>(bytes: &'a [u8], span: BoxSpan) -> Result<&'a [u8], String> {
    let start = span.payload_start()?;
    let end = span.end()?;
    bytes
        .get(start..end)
        .ok_or_else(|| "MP4 box payload exceeds its resource".to_string())
}

fn find_one<'a>(spans: &'a [BoxSpan], kind: &[u8; 4]) -> Result<&'a BoxSpan, String> {
    let mut matches = spans.iter().filter(|span| &span.kind == kind);
    let found = matches
        .next()
        .ok_or_else(|| format!("MP4 fragment is missing {} box", String::from_utf8_lossy(kind)))?;
    if matches.next().is_some() {
        return Err(format!(
            "MP4 fragment contains multiple {} boxes",
            String::from_utf8_lossy(kind)
        ));
    }
    Ok(found)
}

#[derive(Clone, Copy, Debug)]
struct InitEncryption {
    track_id: u32,
    iv_size: u8,
    constant_iv: Option<[u8; 16]>,
    crypt_blocks: u8,
    skip_blocks: u8,
}

fn collect_mp4_boxes<'a>(
    boxes: &'a [mp4box::Box],
    kind: &str,
    output: &mut Vec<&'a mp4box::Box>,
) {
    for box_ in boxes {
        if box_.typ == kind {
            output.push(box_);
        }
        if let Some(children) = box_.children.as_deref() {
            collect_mp4_boxes(children, kind, output);
        }
    }
}

fn mp4_payload<'a>(bytes: &'a [u8], box_: &mp4box::Box) -> Result<&'a [u8], String> {
    let offset = box_
        .payload_offset
        .ok_or_else(|| format!("MP4 {} box has no payload offset", box_.typ))?;
    let length = box_
        .payload_size
        .ok_or_else(|| format!("MP4 {} box has no payload size", box_.typ))?;
    let start = usize::try_from(offset)
        .map_err(|_| format!("MP4 {} payload offset is out of range", box_.typ))?;
    let size = usize::try_from(length)
        .map_err(|_| format!("MP4 {} payload size is out of range", box_.typ))?;
    let end = start
        .checked_add(size)
        .ok_or_else(|| format!("MP4 {} payload range overflows", box_.typ))?;
    bytes
        .get(start..end)
        .ok_or_else(|| format!("MP4 {} payload exceeds the initialization resource", box_.typ))
}

fn parse_init_encryption(
    init: &[u8],
    expected: DashEncryption,
) -> Result<InitEncryption, String> {
    let boxes = mp4box::get_boxes(
        &mut Cursor::new(init),
        u64::try_from(init.len()).map_err(|_| "DASH initialization size is out of range".to_string())?,
        false,
    )
    .map_err(|error| format!("Could not parse DASH encryption initialization boxes: {error}"))?;
    if !boxes.iter().any(|box_| box_.typ == "ftyp")
        || !boxes.iter().any(|box_| box_.typ == "moov")
    {
        return Err("Encrypted DASH initialization resource must contain ftyp and moov".to_string());
    }

    let mut movie_headers = Vec::new();
    let mut file_types = Vec::new();
    let mut track_boxes = Vec::new();
    let mut tenc_boxes = Vec::new();
    let mut schm_boxes = Vec::new();
    let mut tkhd_boxes = Vec::new();
    collect_mp4_boxes(&boxes, "moov", &mut movie_headers);
    collect_mp4_boxes(&boxes, "ftyp", &mut file_types);
    collect_mp4_boxes(&boxes, "trak", &mut track_boxes);
    collect_mp4_boxes(&boxes, "tenc", &mut tenc_boxes);
    collect_mp4_boxes(&boxes, "schm", &mut schm_boxes);
    collect_mp4_boxes(&boxes, "tkhd", &mut tkhd_boxes);
    if movie_headers.len() != 1
        || file_types.len() != 1
        || track_boxes.len() != 1
        || tenc_boxes.len() != 1
        || schm_boxes.len() != 1
        || tkhd_boxes.len() != 1
    {
        return Err(
            "Encrypted DASH initialization must contain one ftyp, one moov and exactly one protected track"
                .to_string(),
        );
    }

    let schm = mp4_payload(init, schm_boxes[0])?;
    if schm.len() < 12 {
        return Err("DASH scheme box is truncated".to_string());
    }
    let schm_flags = (u32::from(schm[1]) << 16) | (u32::from(schm[2]) << 8) | u32::from(schm[3]);
    if schm_flags & !0x000001 != 0 {
        return Err("DASH scheme box uses unsupported flags".to_string());
    }
    if schm_flags & 0x000001 == 0 && schm.len() != 12 {
        return Err("DASH scheme box contains unexpected trailing data".to_string());
    }
    let actual_scheme = &schm[4..8];
    let expected_scheme = match expected.scheme {
        EncryptionScheme::Cenc => b"cenc".as_slice(),
        EncryptionScheme::Cbcs => b"cbcs".as_slice(),
    };
    if actual_scheme != expected_scheme {
        return Err("DASH MPD encryption scheme does not match the MP4 initialization".to_string());
    }

    let tenc = mp4_payload(init, tenc_boxes[0])?;
    if tenc.len() < 23 {
        return Err("DASH track-encryption box is truncated".to_string());
    }
    let version = tenc[0];
    if tenc[1..4] != [0, 0, 0] {
        return Err("DASH track-encryption box uses unsupported flags".to_string());
    }
    let mut offset = 4_usize;
    offset += 1;
    let (crypt_blocks, skip_blocks) = match version {
        0 => {
            offset += 1;
            (0, 0)
        }
        1 => {
            let pattern = *tenc
                .get(offset)
                .ok_or_else(|| "DASH track-encryption pattern is missing".to_string())?;
            offset += 1;
            (pattern >> 4, pattern & 0x0f)
        }
        _ => return Err("Unsupported DASH tenc version".to_string()),
    };
    if expected.scheme == EncryptionScheme::Cenc && (crypt_blocks != 0 || skip_blocks != 0) {
        return Err("CENC initialization contains CBCS-style pattern parameters".to_string());
    }
    if expected.scheme == EncryptionScheme::Cbcs && crypt_blocks == 0 && skip_blocks != 0 {
        return Err("CBCS initialization has a skip pattern without encrypted blocks".to_string());
    }
    let is_protected = *tenc
        .get(offset)
        .ok_or_else(|| "DASH tenc protection flag is missing".to_string())?;
    offset += 1;
    let iv_size = *tenc
        .get(offset)
        .ok_or_else(|| "DASH tenc IV size is missing".to_string())?;
    offset += 1;
    let kid_end = offset
        .checked_add(16)
        .ok_or_else(|| "DASH tenc KID offset overflows".to_string())?;
    let kid_bytes = tenc
        .get(offset..kid_end)
        .ok_or_else(|| "DASH tenc KID is truncated".to_string())?;
    let mut kid = [0_u8; 16];
    kid.copy_from_slice(kid_bytes);
    offset = kid_end;
    if is_protected != 1 || kid != expected.default_kid {
        return Err("DASH tenc is not protected by the selected MPD default_KID".to_string());
    }

    let constant_iv = if iv_size == 0 {
        let size = usize::from(*tenc.get(offset).ok_or_else(|| {
            "DASH tenc uses a constant IV but does not provide one".to_string()
        })?);
        offset += 1;
        if size != 8 && size != 16 {
            return Err("DASH tenc constant IV must contain 8 or 16 bytes".to_string());
        }
        if expected.scheme == EncryptionScheme::Cenc {
            return Err("CENC initialization must use per-sample IVs".to_string());
        }
        let end = offset
            .checked_add(size)
            .ok_or_else(|| "DASH tenc constant IV range overflows".to_string())?;
        let value = tenc
            .get(offset..end)
            .ok_or_else(|| "DASH tenc constant IV is truncated".to_string())?;
        let mut iv = [0_u8; 16];
        iv[..size].copy_from_slice(value);
        offset = end;
        Some(iv)
    } else {
        if iv_size != 8 && iv_size != 16 {
            return Err("DASH per-sample IV size must be 8 or 16 bytes".to_string());
        }
        None
    };
    if offset != tenc.len() {
        return Err("DASH tenc contains unsupported trailing encryption metadata".to_string());
    }

    let tkhd = mp4_payload(init, tkhd_boxes[0])?;
    let version = *tkhd
        .first()
        .ok_or_else(|| "DASH tkhd is truncated".to_string())?;
    let track_id_offset = if version == 1 {
        20
    } else if version == 0 {
        12
    } else {
        return Err("Unsupported DASH tkhd version".to_string());
    };
    let track_id = read_u32(tkhd, track_id_offset)?;
    if track_id == 0 {
        return Err("DASH initialization contains a zero track ID".to_string());
    }

    Ok(InitEncryption {
        track_id,
        iv_size,
        constant_iv,
        crypt_blocks,
        skip_blocks,
    })
}

fn validate_fragment(bytes: &[u8], init: InitEncryption) -> Result<(), String> {
    let top = parse_box_spans(bytes, 0, bytes.len())?;
    let moofs: Vec<_> = top.iter().filter(|span| &span.kind == b"moof").collect();
    let mdats: Vec<_> = top.iter().filter(|span| &span.kind == b"mdat").collect();
    if moofs.len() != 1
        || mdats.len() != 1
        || top.iter().any(|span| {
            &span.kind != b"styp" && &span.kind != b"moof" && &span.kind != b"mdat"
        })
    {
        return Err("Each encrypted DASH segment must contain one moof and one mdat".to_string());
    }
    if top.iter().position(|span| &span.kind == b"moof")
        >= top.iter().position(|span| &span.kind == b"mdat")
    {
        return Err("DASH media data precedes its movie fragment".to_string());
    }
    let moof = moofs[0];
    let mdat = mdats[0];
    let moof_children = parse_box_spans(bytes, moof.payload_start()?, moof.end()?)?;
    let traf = find_one(&moof_children, b"traf")?;
    if moof_children.iter().filter(|span| &span.kind == b"traf").count() != 1 {
        return Err("Encrypted DASH fragments must contain exactly one traf".to_string());
    }
    let traf_children = parse_box_spans(bytes, traf.payload_start()?, traf.end()?)?;
    if traf_children.iter().any(|span| {
        matches!(&span.kind, b"saiz" | b"saio" | b"sgpd" | b"sbgp" | b"uuid")
    }) {
        return Err("Encrypted DASH fragment uses unsupported auxiliary or key-rotation metadata".to_string());
    }
    let tfhd = find_one(&traf_children, b"tfhd")?;
    let trun = find_one(&traf_children, b"trun")?;
    let senc = find_one(&traf_children, b"senc")?;
    if traf_children.iter().filter(|span| &span.kind == b"tfhd").count() != 1
        || traf_children.iter().filter(|span| &span.kind == b"trun").count() != 1
        || traf_children.iter().filter(|span| &span.kind == b"senc").count() != 1
    {
        return Err("Encrypted DASH fragment requires one tfhd, trun and senc".to_string());
    }

    let tfhd_payload = span_payload(bytes, *tfhd)?;
    if tfhd_payload.len() < 8 || tfhd_payload[0] != 0 {
        return Err("DASH tfhd is truncated or uses an unsupported version".to_string());
    }
    let tfhd_flags = (u32::from(tfhd_payload[1]) << 16)
        | (u32::from(tfhd_payload[2]) << 8)
        | u32::from(tfhd_payload[3]);
    const TFHD_SUPPORTED_FLAGS: u32 = 0x0000_0002 | 0x0000_0008 | 0x0000_0010
        | 0x0000_0020 | 0x0002_0000;
    if tfhd_flags & !TFHD_SUPPORTED_FLAGS != 0
        || tfhd_flags & 0x0002_0000 == 0
        || read_u32(tfhd_payload, 4)? != init.track_id
    {
        return Err("DASH tfhd uses unsupported flags or a different track ID".to_string());
    }
    let mut tfhd_offset = 8_usize;
    for flag in [0x0000_0002, 0x0000_0008, 0x0000_0010, 0x0000_0020] {
        if tfhd_flags & flag != 0 {
            tfhd_offset = tfhd_offset
                .checked_add(4)
                .ok_or_else(|| "DASH tfhd field offset overflows".to_string())?;
        }
    }
    if tfhd_offset != tfhd_payload.len() {
        return Err("DASH tfhd contains trailing or truncated optional fields".to_string());
    }

    let trun_payload = span_payload(bytes, *trun)?;
    if trun_payload.len() < 8 || trun_payload[0] > 1 {
        return Err("DASH trun is truncated or uses an unsupported version".to_string());
    }
    let trun_flags = (u32::from(trun_payload[1]) << 16)
        | (u32::from(trun_payload[2]) << 8)
        | u32::from(trun_payload[3]);
    const TRUN_SUPPORTED_FLAGS: u32 = 0x0000_0001 | 0x0000_0004 | 0x0000_0100
        | 0x0000_0200 | 0x0000_0400 | 0x0000_0800;
    if trun_flags & !TRUN_SUPPORTED_FLAGS != 0
        || trun_flags & 0x0000_0001 == 0
        || trun_flags & 0x0000_0200 == 0
        || (trun_flags & 0x0000_0004 != 0 && trun_flags & 0x0000_0400 != 0)
    {
        return Err("DASH trun must declare one data offset and explicit sample sizes with supported flags".to_string());
    }
    let sample_count = usize::try_from(read_u32(trun_payload, 4)?)
        .map_err(|_| "DASH trun sample count is out of range".to_string())?;
    if sample_count == 0 || sample_count > MAX_SAMPLES_PER_FRAGMENT {
        return Err("DASH fragment sample count is empty or exceeds the configured limit".to_string());
    }
    let mut cursor = 8_usize;
    let data_offset = read_i32(trun_payload, cursor)?;
    cursor += 4;
    if trun_flags & 0x000004 != 0 {
        cursor = cursor
            .checked_add(4)
            .ok_or_else(|| "DASH trun cursor overflows".to_string())?;
    }
    let mut sample_sizes = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        if trun_flags & 0x000100 != 0 {
            cursor = cursor
                .checked_add(4)
                .ok_or_else(|| "DASH trun cursor overflows".to_string())?;
        }
        let size = read_u32(trun_payload, cursor)?;
        cursor += 4;
        if size == 0 {
            return Err("DASH trun contains an empty sample".to_string());
        }
        sample_sizes.push(size);
        if trun_flags & 0x000400 != 0 {
            cursor = cursor
                .checked_add(4)
                .ok_or_else(|| "DASH trun cursor overflows".to_string())?;
        }
        if trun_flags & 0x000800 != 0 {
            cursor = cursor
                .checked_add(4)
                .ok_or_else(|| "DASH trun cursor overflows".to_string())?;
        }
    }
    if cursor != trun_payload.len() {
        return Err("DASH trun contains unsupported or trailing sample metadata".to_string());
    }

    let mdat_payload_start = mdat.payload_start()?;
    let expected_offset = i64::try_from(mdat_payload_start)
        .ok()
        .and_then(|start| {
            i64::try_from(moof.start)
                .ok()
                .and_then(|base| start.checked_sub(base))
        })
        .ok_or_else(|| "DASH fragment data offset is out of range".to_string())?;
    if i64::from(data_offset) != expected_offset {
        return Err("DASH trun data offset does not point to the mdat payload".to_string());
    }
    let sample_bytes = sample_sizes
        .iter()
        .try_fold(0_u64, |sum, size| sum.checked_add(u64::from(*size)))
        .ok_or_else(|| "DASH fragment sample sizes overflow".to_string())?;
    let mdat_payload_size = u64::try_from(mdat.size - mdat.header)
        .map_err(|_| "DASH mdat size is out of range".to_string())?;
    if sample_bytes != mdat_payload_size {
        return Err("DASH sample sizes do not exactly cover the mdat payload".to_string());
    }

    let senc_payload = span_payload(bytes, *senc)?;
    if senc_payload.len() < 8 || senc_payload[0] != 0 {
        return Err("DASH senc is truncated or uses an unsupported version".to_string());
    }
    let senc_flags = (u32::from(senc_payload[1]) << 16)
        | (u32::from(senc_payload[2]) << 8)
        | u32::from(senc_payload[3]);
    if senc_flags & !0x000002 != 0 {
        return Err("DASH senc uses unsupported override flags".to_string());
    }
    let senc_count = usize::try_from(read_u32(senc_payload, 4)?)
        .map_err(|_| "DASH senc sample count is out of range".to_string())?;
    if senc_count != sample_count {
        return Err("DASH trun and senc sample counts do not match".to_string());
    }
    let mut senc_offset = 8_usize;
    for sample_size in sample_sizes {
        if init.iv_size > 0 {
            senc_offset = senc_offset
                .checked_add(usize::from(init.iv_size))
                .ok_or_else(|| "DASH senc IV offset overflows".to_string())?;
        } else if init.constant_iv.is_none() {
            return Err("DASH senc has no per-sample or constant IV".to_string());
        }

        let mut covered = 0_u64;
        if senc_flags & 0x000002 != 0 {
            let count = usize::from(read_u16(senc_payload, senc_offset)?);
            senc_offset += 2;
            if count > MAX_SAMPLES_PER_FRAGMENT {
                return Err("DASH senc subsample count exceeds the configured limit".to_string());
            }
            for _ in 0..count {
                let clear = u64::from(read_u16(senc_payload, senc_offset)?);
                senc_offset += 2;
                let encrypted = u64::from(read_u32(senc_payload, senc_offset)?);
                senc_offset += 4;
                covered = covered
                    .checked_add(clear)
                    .and_then(|sum| sum.checked_add(encrypted))
                    .ok_or_else(|| "DASH senc subsample sizes overflow".to_string())?;
            }
        } else {
            covered = u64::from(sample_size);
        }
        if covered != u64::from(sample_size) {
            return Err("DASH senc subsamples do not exactly cover their sample".to_string());
        }
    }
    if senc_offset != senc_payload.len() {
        return Err("DASH senc contains trailing or unsupported encryption data".to_string());
    }

    Ok(())
}

/// Decrypt one independently planned fragmented-MP4 representation into a
/// private XDG-staging file. Encrypted source resources are never modified.
pub(super) fn decrypt_track(
    resources: &[DashResource],
    directory: &Path,
    output: &Path,
    encryption: DashEncryption,
    key: &[u8; 16],
) -> Result<(), String> {
    if resources.len() < 2
        || resources[0].kind != DashResourceKind::Initialization
        || resources[0].range.is_some()
        || resources[1..]
            .iter()
            .any(|resource| resource.kind != DashResourceKind::Segment || resource.range.is_some())
    {
        return Err("Encrypted native DASH currently requires one initialization resource followed by ordinary fragmented media segments".to_string());
    }

    let init_path = directory.join(&resources[0].filename);
    let init = read_regular_file_bounded(&init_path, MAX_INIT_BYTES)
        .map_err(|error| format!("Could not read encrypted DASH initialization resource: {error}"))?;
    if init.is_empty() {
        return Err("Encrypted DASH initialization resource is empty".to_string());
    }
    let init_encryption = parse_init_encryption(&init, encryption)?;
    let key_hex = hex_string(key);
    let decrypter = vsd_mp4::decrypt::CencDecrypter::with_init(&key_hex, &init)
        .map_err(|error| format!("VSD could not initialize DASH decryption: {error}"))?;

    let parent = output
        .parent()
        .ok_or_else(|| "Decrypted DASH track path has no parent directory".to_string())?;
    if let Ok(metadata) = std::fs::symlink_metadata(output) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("Decrypted DASH track path is not a regular file".to_string());
        }
        std::fs::remove_file(output)
            .map_err(|error| format!("Could not reset private decrypted DASH staging: {error}"))?;
    }

    let mut writer = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("Could not create private decrypted DASH staging: {error}"))?;
    writer
        .as_file_mut()
        .write_all(&init)
        .map_err(|error| format!("Could not write decrypted DASH initialization: {error}"))?;
    let mut total = u64::try_from(init.len())
        .map_err(|_| "DASH decrypted track size is out of range".to_string())?;

    for resource in &resources[1..] {
        let path = directory.join(&resource.filename);
        let fragment = read_regular_file_bounded(&path, MAX_FRAGMENT_BYTES)
            .map_err(|error| format!("Could not read encrypted DASH media segment: {error}"))?;
        validate_fragment(&fragment, init_encryption)?;
        let encrypted_length = fragment.len();
        let decrypted = decrypter
            .decrypt_fragment(fragment, Some(&init))
            .map_err(|error| format!("VSD could not decrypt a DASH media fragment: {error}"))?;
        if decrypted.len() != encrypted_length {
            return Err("VSD changed the length of a validated DASH fragment".to_string());
        }
        let length = u64::try_from(decrypted.len())
            .map_err(|_| "DASH decrypted fragment size is out of range".to_string())?;
        total = total
            .checked_add(length)
            .filter(|size| *size <= MAX_TRACK_BYTES)
            .ok_or_else(|| "Decrypted DASH track exceeds Grab's 1 TiB native track limit".to_string())?;
        writer
            .as_file_mut()
            .write_all(&decrypted)
            .map_err(|error| format!("Could not write decrypted DASH media: {error}"))?;
    }

    writer
        .as_file()
        .sync_all()
        .map_err(|error| format!("Could not flush decrypted DASH track: {error}"))?;
    writer.persist_noclobber(output).map_err(|error| {
        format!(
            "Could not publish private decrypted DASH track without clobbering: {}",
            error.error
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hyphenated_and_compact_hex_identifiers() {
        let value = "00112233-4455-6677-8899-aabbccddeeff";
        let expected = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        assert_eq!(parse_hex_16(value), Some(expected));
        assert_eq!(parse_hex_16("00112233445566778899aabbccddeeff"), Some(expected));
        assert_eq!(parse_hex_16("00112233445566778899aabbccddeef"), None);
        assert_eq!(parse_hex_16("zz112233445566778899aabbccddeeff"), None);
    }

    #[test]
    fn rejects_fragment_boxes_with_truncated_or_oversized_headers() {
        assert!(parse_box_spans(b"\0\0\0", 0, 3).is_err());
        assert!(parse_box_spans(b"\0\0\0\x01moof", 0, 8).is_err());
        assert!(parse_box_spans(b"\0\0\0\x04moof", 0, 8).is_err());
    }

    #[test]
    fn parses_a_complete_top_level_fragment() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&16_u32.to_be_bytes());
        bytes.extend_from_slice(b"moof");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&8_u32.to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        let spans = parse_box_spans(&bytes, 0, bytes.len()).expect("valid top-level boxes");
        assert_eq!(spans.len(), 2);
        assert_eq!(&spans[0].kind, b"moof");
        assert_eq!(&spans[1].kind, b"mdat");
    }
}
