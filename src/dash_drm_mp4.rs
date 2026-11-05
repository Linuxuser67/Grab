//! Strict fragmented-MP4 validation and decryption for native DASH tracks.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use zeroize::Zeroizing;

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
            return Err(
                "MP4 boxes with size zero are not supported in encrypted resources".to_string(),
            );
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

fn span_payload(bytes: &[u8], span: BoxSpan) -> Result<&[u8], String> {
    let start = span.payload_start()?;
    let end = span.end()?;
    bytes
        .get(start..end)
        .ok_or_else(|| "MP4 box payload exceeds its resource".to_string())
}

fn find_one<'a>(spans: &'a [BoxSpan], kind: &[u8; 4]) -> Result<&'a BoxSpan, String> {
    let mut matches = spans.iter().filter(|span| &span.kind == kind);
    let found = matches.next().ok_or_else(|| {
        format!(
            "MP4 fragment is missing {} box",
            String::from_utf8_lossy(kind)
        )
    })?;
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
    default_kid: [u8; 16],
    iv_size: u8,
    constant_iv: Option<[u8; 16]>,
    constant_iv_size: Option<u8>,
    crypt_blocks: u8,
    skip_blocks: u8,
}

fn collect_mp4_boxes<'a>(boxes: &'a [mp4box::Box], kind: &str, output: &mut Vec<&'a mp4box::Box>) {
    for box_ in boxes {
        if box_.typ == kind {
            output.push(box_);
        }
        if let Some(children) = box_.children.as_deref() {
            collect_mp4_boxes(children, kind, output);
        }
    }
}

fn mp4_children(box_: &mp4box::Box) -> Result<&[mp4box::Box], String> {
    box_.children
        .as_deref()
        .ok_or_else(|| format!("MP4 {} box does not expose its child boxes", box_.typ))
}

fn find_one_mp4_child<'a>(boxes: &'a [mp4box::Box], kind: &str) -> Result<&'a mp4box::Box, String> {
    let mut matches = boxes.iter().filter(|box_| box_.typ == kind);
    let found = matches.next().ok_or_else(|| {
        format!("MP4 initialization is missing {kind} box in its required parent")
    })?;
    if matches.next().is_some() {
        return Err(format!(
            "MP4 initialization contains multiple {kind} boxes in one parent"
        ));
    }
    Ok(found)
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
    bytes.get(start..end).ok_or_else(|| {
        format!(
            "MP4 {} payload exceeds the initialization resource",
            box_.typ
        )
    })
}

/// Rebuild the complete FullBox payload from mp4box's split metadata.
///
/// mp4box exposes the version and flags as Box fields and starts payload_offset
/// after those four bytes. The DRM validators below intentionally parse the
/// on-disk FullBox layout, so restore that prefix before validating field offsets.
fn mp4_full_box_payload(bytes: &[u8], box_: &mp4box::Box) -> Result<Vec<u8>, String> {
    let version = box_
        .version
        .ok_or_else(|| format!("MP4 {} full box has no version", box_.typ))?;
    let flags = box_
        .flags
        .ok_or_else(|| format!("MP4 {} full box has no flags", box_.typ))?;
    let flag_bytes = flags.to_be_bytes();
    if flag_bytes[0] != 0 {
        return Err(format!("MP4 {} full box flags exceed 24 bits", box_.typ));
    }

    let data = mp4_payload(bytes, box_)?;
    let capacity = data
        .len()
        .checked_add(4)
        .ok_or_else(|| format!("MP4 {} full box payload size overflows", box_.typ))?;
    let mut payload = Vec::with_capacity(capacity);
    payload.push(version);
    payload.extend_from_slice(&flag_bytes[1..]);
    payload.extend_from_slice(data);
    Ok(payload)
}

fn parse_init_encryption(init: &[u8], expected: DashEncryption) -> Result<InitEncryption, String> {
    let boxes = mp4box::get_boxes(
        &mut Cursor::new(init),
        u64::try_from(init.len())
            .map_err(|_| "DASH initialization size is out of range".to_string())?,
        false,
    )
    .map_err(|error| format!("Could not parse DASH encryption initialization boxes: {error}"))?;
    if !boxes.iter().any(|box_| box_.typ == "ftyp") || !boxes.iter().any(|box_| box_.typ == "moov")
    {
        return Err(
            "Encrypted DASH initialization resource must contain ftyp and moov".to_string(),
        );
    }

    let mut movie_headers = Vec::new();
    let mut file_types = Vec::new();
    let mut track_boxes = Vec::new();
    let mut tenc_boxes = Vec::new();
    let mut schm_boxes = Vec::new();
    let mut tkhd_boxes = Vec::new();
    let mut trex_boxes = Vec::new();
    collect_mp4_boxes(&boxes, "moov", &mut movie_headers);
    collect_mp4_boxes(&boxes, "ftyp", &mut file_types);
    collect_mp4_boxes(&boxes, "trak", &mut track_boxes);
    collect_mp4_boxes(&boxes, "tenc", &mut tenc_boxes);
    collect_mp4_boxes(&boxes, "schm", &mut schm_boxes);
    collect_mp4_boxes(&boxes, "tkhd", &mut tkhd_boxes);
    collect_mp4_boxes(&boxes, "trex", &mut trex_boxes);
    if movie_headers.len() != 1
        || file_types.len() != 1
        || track_boxes.len() != 1
        || tenc_boxes.len() != 1
        || schm_boxes.len() != 1
        || tkhd_boxes.len() != 1
        || trex_boxes.len() != 1
    {
        return Err(
            "Encrypted DASH initialization must contain one ftyp, one moov and exactly one protected fragmented track"
                .to_string(),
        );
    }

    // VSD only discovers tenc and schm through the standard encrypted sample-entry
    // hierarchy. Validate that ancestry before handing the init to its parser;
    // recursive presence checks alone could accept unrelated decoy boxes.
    let _ftyp = find_one_mp4_child(&boxes, "ftyp")?;
    let moov = find_one_mp4_child(&boxes, "moov")?;
    let trak = find_one_mp4_child(mp4_children(moov)?, "trak")?;
    let trak_children = mp4_children(trak)?;
    let tkhd_box = find_one_mp4_child(trak_children, "tkhd")?;
    let mdia = find_one_mp4_child(trak_children, "mdia")?;
    let minf = find_one_mp4_child(mp4_children(mdia)?, "minf")?;
    let stbl = find_one_mp4_child(mp4_children(minf)?, "stbl")?;
    let stsd = find_one_mp4_child(mp4_children(stbl)?, "stsd")?;
    let sample_entries = mp4_children(stsd)?;
    if sample_entries.len() != 1 || !matches!(sample_entries[0].typ.as_str(), "encv" | "enca") {
        return Err(
            "Encrypted DASH initialization must contain exactly one encv or enca sample entry"
                .to_string(),
        );
    }
    let sample_entry = &sample_entries[0];
    let sinf = find_one_mp4_child(mp4_children(sample_entry)?, "sinf")?;
    let sinf_children = mp4_children(sinf)?;
    if sinf_children.len() != 3 {
        return Err("Encrypted DASH sinf must contain only frma, schm and schi".to_string());
    }
    let frma_box = find_one_mp4_child(sinf_children, "frma")?;
    let schm_box = find_one_mp4_child(sinf_children, "schm")?;
    let schi = find_one_mp4_child(sinf_children, "schi")?;
    let schi_children = mp4_children(schi)?;
    if schi_children.len() != 1 {
        return Err("Encrypted DASH schi must contain exactly one tenc box".to_string());
    }
    let tenc_box = find_one_mp4_child(schi_children, "tenc")?;
    let frma = mp4_payload(init, frma_box)?;
    if frma.len() != 4 {
        return Err("DASH original-format box is truncated or contains trailing data".to_string());
    }

    let schm = mp4_full_box_payload(init, schm_box)?;
    if schm.len() < 12 || schm[0] != 0 {
        return Err("DASH scheme box is truncated or uses an unsupported version".to_string());
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

    let tenc = mp4_full_box_payload(init, tenc_box)?;
    if tenc.len() < 23 {
        return Err("DASH track-encryption box is truncated".to_string());
    }
    let version = tenc[0];
    if tenc[1..4] != [0, 0, 0] {
        return Err("DASH track-encryption box uses unsupported flags".to_string());
    }
    if tenc[4] != 0 {
        return Err("DASH track-encryption reserved byte is nonzero".to_string());
    }
    let mut offset = 4_usize;
    offset += 1;
    let (crypt_blocks, skip_blocks) = match version {
        0 => {
            if tenc[offset] != 0 {
                return Err(
                    "DASH version-zero track-encryption reserved byte is nonzero".to_string(),
                );
            }
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

    let mut constant_iv_size = None;
    let constant_iv = if iv_size == 0 {
        let size =
            usize::from(*tenc.get(offset).ok_or_else(|| {
                "DASH tenc uses a constant IV but does not provide one".to_string()
            })?);
        offset += 1;
        if size != 8 && size != 16 {
            return Err("DASH tenc constant IV must contain 8 or 16 bytes".to_string());
        }
        constant_iv_size = Some(u8::try_from(size).map_err(|_| {
            "DASH tenc constant IV size is out of range".to_string()
        })?);
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

    let tkhd = mp4_full_box_payload(init, tkhd_box)?;
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
    let track_id = read_u32(&tkhd, track_id_offset)?;
    if track_id == 0 {
        return Err("DASH initialization contains a zero track ID".to_string());
    }

    let mvex = find_one_mp4_child(mp4_children(moov)?, "mvex")?;
    let trex = find_one_mp4_child(mp4_children(mvex)?, "trex")?;
    let trex_payload = mp4_full_box_payload(init, trex)?;
    if trex_payload.len() != 24
        || trex_payload[0] != 0
        || trex_payload[1..4] != [0, 0, 0]
        || read_u32(&trex_payload, 4)? != track_id
        || read_u32(&trex_payload, 8)? != 1
    {
        return Err(
            "DASH trex must match the protected track and select its only sample description"
                .to_string(),
        );
    }

    Ok(InitEncryption {
        track_id,
        default_kid: kid,
        iv_size,
        constant_iv,
        constant_iv_size,
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
        || top.iter().filter(|span| &span.kind == b"sidx").count() > 1
        || top.iter().any(|span| {
            &span.kind != b"styp"
                && &span.kind != b"sidx"
                && &span.kind != b"moof"
                && &span.kind != b"mdat"
        })
    {
        return Err("Each encrypted DASH segment must contain one moof and one mdat, with only optional styp/sidx boxes".to_string());
    }
    if top.iter().position(|span| &span.kind == b"moof")
        >= top.iter().position(|span| &span.kind == b"mdat")
    {
        return Err("DASH media data precedes its movie fragment".to_string());
    }
    let moof = moofs[0];
    let mdat = mdats[0];
    let moof_children = parse_box_spans(bytes, moof.payload_start()?, moof.end()?)?;
    let mfhd = find_one(&moof_children, b"mfhd")?;
    let traf = find_one(&moof_children, b"traf")?;
    if moof_children.len() != 2
        || moof_children
            .iter()
            .any(|span| &span.kind != b"mfhd" && &span.kind != b"traf")
    {
        return Err(
            "Encrypted DASH fragments must contain exactly one mfhd and one traf".to_string(),
        );
    }
    let mfhd_payload = span_payload(bytes, *mfhd)?;
    if mfhd_payload.len() != 8 || mfhd_payload[0] != 0 || mfhd_payload[1..4] != [0, 0, 0] {
        return Err("DASH mfhd is truncated or uses unsupported version/flags".to_string());
    }

    let traf_children = parse_box_spans(bytes, traf.payload_start()?, traf.end()?)?;
    if traf_children.iter().any(|span| {
        !matches!(
            &span.kind,
            b"tfhd" | b"tfdt" | b"trun" | b"saiz" | b"saio" | b"senc" | b"sgpd" | b"sbgp"
        )
    }) {
        return Err(
            "Encrypted DASH fragment uses unsupported key-rotation or sample-group metadata"
                .to_string(),
        );
    }
    if traf_children
        .iter()
        .filter(|span| &span.kind == b"tfdt")
        .count()
        > 1
    {
        return Err("Encrypted DASH fragment contains multiple tfdt boxes".to_string());
    }
    if let Some(tfdt) = traf_children.iter().find(|span| &span.kind == b"tfdt") {
        let payload = span_payload(bytes, *tfdt)?;
        let flags = if payload.len() >= 4 {
            (u32::from(payload[1]) << 16) | (u32::from(payload[2]) << 8) | u32::from(payload[3])
        } else {
            u32::MAX
        };
        if flags != 0
            || !matches!(
                (payload.first().copied(), payload.len()),
                (Some(0), 8) | (Some(1), 12)
            )
        {
            return Err("DASH tfdt is truncated or uses an unsupported version/flags".to_string());
        }
    }
    let tfhd = find_one(&traf_children, b"tfhd")?;
    let trun = find_one(&traf_children, b"trun")?;
    let senc = find_one(&traf_children, b"senc")?;
    let saiz: Vec<_> = traf_children
        .iter()
        .filter(|span| &span.kind == b"saiz")
        .collect();
    let saio: Vec<_> = traf_children
        .iter()
        .filter(|span| &span.kind == b"saio")
        .collect();
    if traf_children
        .iter()
        .filter(|span| &span.kind == b"tfhd")
        .count()
        != 1
        || traf_children
            .iter()
            .filter(|span| &span.kind == b"trun")
            .count()
            != 1
        || traf_children
            .iter()
            .filter(|span| &span.kind == b"senc")
            .count()
            != 1
        || saiz.len() > 1
        || saio.len() > 1
        || (saiz.is_empty() != saio.is_empty())
    {
        return Err("Encrypted DASH fragment requires one tfhd, trun and senc, with paired optional saiz/saio boxes".to_string());
    }

    let tfhd_payload = span_payload(bytes, *tfhd)?;
    if tfhd_payload.len() < 8 || tfhd_payload[0] != 0 {
        return Err("DASH tfhd is truncated or uses an unsupported version".to_string());
    }
    let tfhd_flags = (u32::from(tfhd_payload[1]) << 16)
        | (u32::from(tfhd_payload[2]) << 8)
        | u32::from(tfhd_payload[3]);
    const TFHD_SUPPORTED_FLAGS: u32 =
        0x0000_0002 | 0x0000_0008 | 0x0000_0010 | 0x0000_0020 | 0x0002_0000;
    if tfhd_flags & !TFHD_SUPPORTED_FLAGS != 0
        || tfhd_flags & 0x0002_0000 == 0
        || read_u32(tfhd_payload, 4)? != init.track_id
    {
        return Err("DASH tfhd uses unsupported flags or a different track ID".to_string());
    }
    let mut tfhd_offset = 8_usize;
    if tfhd_flags & 0x0000_0002 != 0 && read_u32(tfhd_payload, tfhd_offset)? != 1 {
        return Err(
            "DASH tfhd selects a sample description other than the only validated entry"
                .to_string(),
        );
    }
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
    const TRUN_SUPPORTED_FLAGS: u32 =
        0x0000_0001 | 0x0000_0004 | 0x0000_0100 | 0x0000_0200 | 0x0000_0400 | 0x0000_0800;
    if trun_flags & !TRUN_SUPPORTED_FLAGS != 0
        || trun_flags & 0x0000_0001 == 0
        || trun_flags & 0x0000_0200 == 0
        || (trun_flags & 0x0000_0004 != 0 && trun_flags & 0x0000_0400 != 0)
    {
        return Err(
            "DASH trun must declare one data offset and explicit sample sizes with supported flags"
                .to_string(),
        );
    }
    let sample_count = usize::try_from(read_u32(trun_payload, 4)?)
        .map_err(|_| "DASH trun sample count is out of range".to_string())?;
    if sample_count == 0 || sample_count > MAX_SAMPLES_PER_FRAGMENT {
        return Err(
            "DASH fragment sample count is empty or exceeds the configured limit".to_string(),
        );
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
    let mut sample_auxiliary_sizes = Vec::with_capacity(sample_count);
    for sample_size in sample_sizes {
        let sample_auxiliary_start = senc_offset;
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
        let entry_size = senc_offset
            .checked_sub(sample_auxiliary_start)
            .ok_or_else(|| "DASH senc sample entry offset underflows".to_string())?;
        sample_auxiliary_sizes.push(
            u8::try_from(entry_size)
                .map_err(|_| "DASH senc sample auxiliary entry exceeds 255 bytes".to_string())?,
        );
    }
    if senc_offset != senc_payload.len() {
        return Err("DASH senc contains trailing or unsupported encryption data".to_string());
    }

    if let (Some(saiz), Some(saio)) = (saiz.first(), saio.first()) {
        validate_auxiliary_information(
            bytes,
            *moof,
            *senc,
            **saiz,
            **saio,
            &sample_auxiliary_sizes,
        )?;
    }

    Ok(())
}


#[derive(Clone, Debug)]
struct FragmentSample {
    data_offset: usize,
    size: usize,
    auxiliary: Vec<u8>,
    trun_record: Vec<u8>,
    kid: [u8; 16],
}

fn sample_group_kids(
    bytes: &[u8],
    traf_children: &[BoxSpan],
    sample_count: usize,
    init: InitEncryption,
) -> Result<Vec<[u8; 16]>, String> {
    let sgpd_boxes: Vec<_> = traf_children
        .iter()
        .filter(|span| &span.kind == b"sgpd")
        .collect();
    let sbgp_boxes: Vec<_> = traf_children
        .iter()
        .filter(|span| &span.kind == b"sbgp")
        .collect();
    if sgpd_boxes.len() > 1
        || sbgp_boxes.len() > 1
        || sgpd_boxes.is_empty() != sbgp_boxes.is_empty()
    {
        return Err("DASH key rotation requires one matching sgpd/sbgp seig pair".to_string());
    }
    let (Some(sgpd), Some(sbgp)) = (sgpd_boxes.first(), sbgp_boxes.first()) else {
        return Ok(vec![init.default_kid; sample_count]);
    };

    let descriptions_payload = span_payload(bytes, **sgpd)?;
    if descriptions_payload.len() < 16
        || descriptions_payload[0] != 1
        || descriptions_payload[1..4] != [0, 0, 0]
        || descriptions_payload.get(4..8) != Some(b"seig".as_slice())
    {
        return Err(
            "DASH sgpd must be a version-one seig Sample Group Description box".to_string(),
        );
    }
    let default_length = usize::try_from(read_u32(descriptions_payload, 8)?)
        .map_err(|_| "DASH sgpd default entry length is out of range".to_string())?;
    let entry_count = usize::try_from(read_u32(descriptions_payload, 12)?)
        .map_err(|_| "DASH sgpd entry count is out of range".to_string())?;
    if entry_count == 0 || entry_count > MAX_SAMPLES_PER_FRAGMENT {
        return Err("DASH sgpd entry count is empty or exceeds the configured limit".to_string());
    }

    let mut descriptions = Vec::with_capacity(entry_count);
    let mut offset = 16_usize;
    for _ in 0..entry_count {
        let entry_length = if default_length == 0 {
            let length = usize::try_from(read_u32(descriptions_payload, offset)?)
                .map_err(|_| "DASH seig entry length is out of range".to_string())?;
            offset = offset
                .checked_add(4)
                .ok_or_else(|| "DASH sgpd entry offset overflows".to_string())?;
            length
        } else {
            default_length
        };
        let end = offset
            .checked_add(entry_length)
            .ok_or_else(|| "DASH seig entry range overflows".to_string())?;
        let entry = descriptions_payload
            .get(offset..end)
            .ok_or_else(|| "DASH seig entry is truncated".to_string())?;
        if entry.len() < 20 || entry[0] != 0 || entry[2] != 1 {
            return Err(
                "DASH seig entry is truncated or does not describe a protected sample"
                    .to_string(),
            );
        }

        let pattern = entry[1];
        let iv_size = entry[3];
        if (pattern >> 4) != init.crypt_blocks
            || (pattern & 0x0f) != init.skip_blocks
            || iv_size != init.iv_size
        {
            return Err(
                "DASH seig changes IV or CBCS pattern parameters; only KID rotation is supported"
                    .to_string(),
            );
        }

        let mut kid = [0_u8; 16];
        kid.copy_from_slice(&entry[4..20]);
        let mut entry_offset = 20_usize;
        if iv_size == 0 {
            let constant_size = usize::from(*entry.get(entry_offset).ok_or_else(|| {
                "DASH seig constant IV size is missing".to_string()
            })?);
            entry_offset += 1;
            if constant_size != 8 && constant_size != 16 {
                return Err("DASH seig constant IV must contain 8 or 16 bytes".to_string());
            }
            let constant_end = entry_offset
                .checked_add(constant_size)
                .ok_or_else(|| "DASH seig constant IV range overflows".to_string())?;
            let constant = entry
                .get(entry_offset..constant_end)
                .ok_or_else(|| "DASH seig constant IV is truncated".to_string())?;
            let expected = init.constant_iv.ok_or_else(|| {
                "DASH seig introduces a constant IV absent from tenc".to_string()
            })?;
            if init.constant_iv_size.map(usize::from) != Some(constant_size)
                || constant != &expected[..constant_size]
            {
                return Err(
                    "DASH seig constant IV differs from the validated tenc IV".to_string(),
                );
            }
            entry_offset = constant_end;
        }
        if entry_offset != entry.len() {
            return Err("DASH seig entry contains unsupported trailing metadata".to_string());
        }
        descriptions.push(kid);
        offset = end;
    }
    if offset != descriptions_payload.len() {
        return Err("DASH sgpd contains trailing or unsupported data".to_string());
    }

    let groups_payload = span_payload(bytes, **sbgp)?;
    if groups_payload.len() < 12
        || groups_payload[0] != 0
        || groups_payload[1..4] != [0, 0, 0]
        || groups_payload.get(4..8) != Some(b"seig".as_slice())
    {
        return Err("DASH sbgp must be a version-zero seig Sample To Group box".to_string());
    }
    let run_count = usize::try_from(read_u32(groups_payload, 8)?)
        .map_err(|_| "DASH sbgp run count is out of range".to_string())?;
    if run_count == 0 || run_count > MAX_SAMPLES_PER_FRAGMENT {
        return Err("DASH sbgp run count is empty or exceeds the configured limit".to_string());
    }
    let expected_length = 12_usize
        .checked_add(
            run_count
                .checked_mul(8)
                .ok_or_else(|| "DASH sbgp entry size overflows".to_string())?,
        )
        .ok_or_else(|| "DASH sbgp size overflows".to_string())?;
    if groups_payload.len() != expected_length {
        return Err("DASH sbgp entries are truncated or contain trailing data".to_string());
    }

    let mut sample_kids = Vec::with_capacity(sample_count);
    let mut cursor = 12_usize;
    for _ in 0..run_count {
        let run_samples = usize::try_from(read_u32(groups_payload, cursor)?)
            .map_err(|_| "DASH sbgp sample count is out of range".to_string())?;
        let group_index = read_u32(groups_payload, cursor + 4)?;
        cursor += 8;
        if run_samples == 0 {
            return Err("DASH sbgp contains an empty sample run".to_string());
        }
        let kid = if group_index == 0 {
            init.default_kid
        } else {
            if group_index >= 0x1_0000 {
                return Err("DASH sbgp references a track-level group description".to_string());
            }
            let index = usize::try_from(group_index - 1)
                .map_err(|_| "DASH sbgp group index is out of range".to_string())?;
            *descriptions.get(index).ok_or_else(|| {
                "DASH sbgp references a missing seig description".to_string()
            })?
        };
        let next_len = sample_kids
            .len()
            .checked_add(run_samples)
            .filter(|length| *length <= sample_count)
            .ok_or_else(|| "DASH sbgp sample runs exceed the fragment sample count".to_string())?;
        sample_kids.resize(next_len, kid);
    }
    if sample_kids.len() != sample_count {
        return Err("DASH sbgp sample runs do not cover the fragment exactly".to_string());
    }
    Ok(sample_kids)
}

fn fragment_samples(
    bytes: &[u8],
    init: InitEncryption,
) -> Result<Vec<FragmentSample>, String> {
    validate_fragment(bytes, init)?;
    let top = parse_box_spans(bytes, 0, bytes.len())?;
    let moof = find_one(&top, b"moof")?;
    let mdat = find_one(&top, b"mdat")?;
    let moof_children = parse_box_spans(bytes, moof.payload_start()?, moof.end()?)?;
    let traf = find_one(&moof_children, b"traf")?;
    let traf_children = parse_box_spans(bytes, traf.payload_start()?, traf.end()?)?;
    let trun = find_one(&traf_children, b"trun")?;
    let senc = find_one(&traf_children, b"senc")?;
    let trun_payload = span_payload(bytes, *trun)?;
    let senc_payload = span_payload(bytes, *senc)?;
    let flags = (u32::from(trun_payload[1]) << 16)
        | (u32::from(trun_payload[2]) << 8)
        | u32::from(trun_payload[3]);
    let sample_count = usize::try_from(read_u32(trun_payload, 4)?)
        .map_err(|_| "DASH trun sample count is out of range".to_string())?;
    let sample_kids = sample_group_kids(bytes, &traf_children, sample_count, init)?;
    let mut cursor = 12_usize;
    if flags & 0x000004 != 0 {
        cursor += 4;
    }
    let mdat_start = mdat.payload_start()?;
    let mut data_offset = mdat_start;
    let mut senc_offset = 8_usize;
    let senc_flags = (u32::from(senc_payload[1]) << 16)
        | (u32::from(senc_payload[2]) << 8)
        | u32::from(senc_payload[3]);
    let mut samples = Vec::with_capacity(sample_count);
    for sample_index in 0..sample_count {
        let record_start = cursor;
        if flags & 0x000100 != 0 {
            cursor += 4;
        }
        let size = usize::try_from(read_u32(trun_payload, cursor)?)
            .map_err(|_| "DASH sample size is out of range".to_string())?;
        cursor += 4;
        if flags & 0x000400 != 0 {
            cursor += 4;
        }
        if flags & 0x000800 != 0 {
            cursor += 4;
        }
        let record = trun_payload
            .get(record_start..cursor)
            .ok_or_else(|| "DASH trun sample record is truncated".to_string())?
            .to_vec();

        let auxiliary_start = senc_offset;
        if init.iv_size > 0 {
            senc_offset = senc_offset
                .checked_add(usize::from(init.iv_size))
                .ok_or_else(|| "DASH senc IV offset overflows".to_string())?;
        }
        if senc_flags & 0x000002 != 0 {
            let subsample_count = usize::from(read_u16(senc_payload, senc_offset)?);
            senc_offset += 2;
            let subsample_bytes = subsample_count
                .checked_mul(6)
                .ok_or_else(|| "DASH senc subsample data size overflows".to_string())?;
            senc_offset = senc_offset
                .checked_add(subsample_bytes)
                .ok_or_else(|| "DASH senc sample data offset overflows".to_string())?;
        }
        let auxiliary = senc_payload
            .get(auxiliary_start..senc_offset)
            .ok_or_else(|| "DASH senc sample auxiliary data is truncated".to_string())?
            .to_vec();
        samples.push(FragmentSample {
            data_offset,
            size,
            auxiliary,
            trun_record: record,
            kid: *sample_kids.get(sample_index).ok_or_else(|| {
                "DASH sample-group mapping does not cover the parsed sample".to_string()
            })?,
        });
        data_offset = data_offset
            .checked_add(size)
            .ok_or_else(|| "DASH sample data offset overflows".to_string())?;
    }
    Ok(samples)
}

fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Result<Vec<u8>, String> {
    let size = payload
        .len()
        .checked_add(8)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| "Synthetic DASH fragment box exceeds the 32-bit size limit".to_string())?;
    let mut output = Vec::with_capacity(payload.len() + 8);
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(payload);
    Ok(output)
}

fn decrypt_one_sample(
    fragment: &[u8],
    init: &[u8],
    sample: &FragmentSample,
    decrypter: &vsd_mp4::decrypt::CencDecrypter,
) -> Result<Vec<u8>, String> {
    let top = parse_box_spans(fragment, 0, fragment.len())?;
    let moof = find_one(&top, b"moof")?;
    let moof_children = parse_box_spans(fragment, moof.payload_start()?, moof.end()?)?;
    let mfhd = find_one(&moof_children, b"mfhd")?;
    let traf = find_one(&moof_children, b"traf")?;
    let traf_children = parse_box_spans(fragment, traf.payload_start()?, traf.end()?)?;
    let tfhd = find_one(&traf_children, b"tfhd")?;
    let trun = find_one(&traf_children, b"trun")?;
    let senc = find_one(&traf_children, b"senc")?;
    let tfdt = traf_children.iter().find(|span| &span.kind == b"tfdt");

    let trun_payload = span_payload(fragment, *trun)?;
    let original_flags = (u32::from(trun_payload[1]) << 16)
        | (u32::from(trun_payload[2]) << 8)
        | u32::from(trun_payload[3]);
    let trun_flags = original_flags & !0x000004;
    let flag_bytes = trun_flags.to_be_bytes();
    let mut trun_fields = vec![
        trun_payload[0],
        flag_bytes[1],
        flag_bytes[2],
        flag_bytes[3],
    ];
    trun_fields.extend_from_slice(&1_u32.to_be_bytes());
    trun_fields.extend_from_slice(&0_i32.to_be_bytes());
    trun_fields.extend_from_slice(&sample.trun_record);
    let trun_box = box_bytes(b"trun", &trun_fields)?;

    let senc_payload = span_payload(fragment, *senc)?;
    let mut senc_fields = senc_payload[..4].to_vec();
    senc_fields.extend_from_slice(&1_u32.to_be_bytes());
    senc_fields.extend_from_slice(&sample.auxiliary);
    let senc_box = box_bytes(b"senc", &senc_fields)?;

    let mut traf_fields = fragment
        .get(tfhd.start..tfhd.end()?)
        .ok_or_else(|| "DASH tfhd range exceeds the fragment".to_string())?
        .to_vec();
    if let Some(tfdt) = tfdt {
        traf_fields.extend_from_slice(
            fragment
                .get(tfdt.start..tfdt.end()?)
                .ok_or_else(|| "DASH tfdt range exceeds the fragment".to_string())?,
        );
    }
    traf_fields.extend_from_slice(&trun_box);
    traf_fields.extend_from_slice(&senc_box);
    let traf_box = box_bytes(b"traf", &traf_fields)?;
    let mut moof_fields = fragment
        .get(mfhd.start..mfhd.end()?)
        .ok_or_else(|| "DASH mfhd range exceeds the fragment".to_string())?
        .to_vec();
    moof_fields.extend_from_slice(&traf_box);
    let mut moof_box = box_bytes(b"moof", &moof_fields)?;

    let data_offset = moof_box
        .len()
        .checked_add(8)
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or_else(|| "Synthetic DASH sample data offset exceeds i32".to_string())?;
    let trun_data_offset = mfhd.size
        .checked_add(8)
        .and_then(|offset| offset.checked_add(tfhd.size))
        .and_then(|offset| offset.checked_add(tfdt.map_or(0, |span| span.size)))
        .and_then(|offset| offset.checked_add(8 + 8))
        .ok_or_else(|| "Synthetic DASH trun offset overflows".to_string())?;
    let patch_start = trun_data_offset
        .checked_add(8)
        .ok_or_else(|| "Synthetic DASH trun data-offset position overflows".to_string())?;
    let patch_end = patch_start
        .checked_add(4)
        .ok_or_else(|| "Synthetic DASH trun data-offset range overflows".to_string())?;
    moof_box
        .get_mut(patch_start..patch_end)
        .ok_or_else(|| "Synthetic DASH trun data-offset field is missing".to_string())?
        .copy_from_slice(&data_offset.to_be_bytes());

    let sample_end = sample
        .data_offset
        .checked_add(sample.size)
        .ok_or_else(|| "DASH sample data range overflows".to_string())?;
    let sample_bytes = fragment
        .get(sample.data_offset..sample_end)
        .ok_or_else(|| "DASH sample data range exceeds its fragment".to_string())?;
    let mdat_box = box_bytes(b"mdat", sample_bytes)?;
    let mut single_sample = moof_box;
    single_sample.extend_from_slice(&mdat_box);

    let decrypted = decrypter
        .decrypt_fragment(single_sample, Some(init))
        .map_err(|error| format!("VSD could not decrypt a rotated DASH sample: {error}"))?;
    let boxes = parse_box_spans(&decrypted, 0, decrypted.len())?;
    let mdat = find_one(&boxes, b"mdat")?;
    let start = mdat.payload_start()?;
    let end = mdat.end()?;
    Ok(decrypted
        .get(start..end)
        .ok_or_else(|| "Decrypted DASH sample payload is truncated".to_string())?
        .to_vec())
}

fn decrypt_fragment_with_keys(
    fragment: &[u8],
    init: &[u8],
    init_encryption: InitEncryption,
    keys: &super::DashKeyMap,
) -> Result<Vec<u8>, String> {
    let samples = fragment_samples(fragment, init_encryption)?;
    let sample_kids: BTreeSet<_> = samples.iter().map(|sample| sample.kid).collect();
    if sample_kids.len() == 1 {
        let kid = *sample_kids
            .iter()
            .next()
            .ok_or_else(|| "DASH fragment has no sample KID".to_string())?;
        let key = keys.key_for_kid(&kid)?;
        let key_hex = Zeroizing::new(hex_string(&key));
        let decrypter = vsd_mp4::decrypt::CencDecrypter::with_init(&key_hex, init)
            .map_err(|error| format!("VSD could not initialize DASH decryption: {error}"))?;
        return decrypter
            .decrypt_fragment(fragment.to_vec(), Some(init))
            .map_err(|error| format!("VSD could not decrypt a DASH media fragment: {error}"));
    }

    let mut samples_by_kid: BTreeMap<[u8; 16], Vec<&FragmentSample>> = BTreeMap::new();
    for sample in &samples {
        samples_by_kid.entry(sample.kid).or_default().push(sample);
    }

    let mut output = fragment.to_vec();
    for (kid, kid_samples) in samples_by_kid {
        let key = keys.key_for_kid(&kid)?;
        let key_hex = Zeroizing::new(hex_string(&key));
        let decrypter = vsd_mp4::decrypt::CencDecrypter::with_init(&key_hex, init)
            .map_err(|error| format!("VSD could not initialize DASH sample decryption: {error}"))?;
        for sample in kid_samples {
            let plaintext = decrypt_one_sample(fragment, init, sample, &decrypter)?;
            if plaintext.len() != sample.size {
                return Err("VSD changed the length of a validated DASH sample".to_string());
            }
            let end = sample
                .data_offset
                .checked_add(sample.size)
                .ok_or_else(|| "DASH sample output range overflows".to_string())?;
            output
                .get_mut(sample.data_offset..end)
                .ok_or_else(|| "DASH sample output range exceeds its fragment".to_string())?
                .copy_from_slice(&plaintext);
        }
    }
    Ok(output)
}

pub(crate) fn required_kids(
    resources: &[DashResource],
    directory: &Path,
    encryption: DashEncryption,
) -> Result<BTreeSet<[u8; 16]>, String> {
    if resources.len() < 2
        || resources[0].kind != DashResourceKind::Initialization
        || resources[1..]
            .iter()
            .any(|resource| resource.kind != DashResourceKind::Segment)
    {
        return Err(
            "Encrypted native DASH requires one initialization resource followed by media segments"
                .to_string(),
        );
    }
    let init_path = directory.join(&resources[0].filename);
    let init = read_regular_file_bounded(&init_path, MAX_INIT_BYTES)
        .map_err(|error| format!("Could not read encrypted DASH initialization resource: {error}"))?;
    let init_encryption = parse_init_encryption(&init, encryption)?;
    let mut required = BTreeSet::new();
    for resource in &resources[1..] {
        let path = directory.join(&resource.filename);
        let fragment = read_regular_file_bounded(&path, MAX_FRAGMENT_BYTES)
            .map_err(|error| format!("Could not read encrypted DASH media segment: {error}"))?;
        for sample in fragment_samples(&fragment, init_encryption)? {
            required.insert(sample.kid);
            if required.len() > super::MAX_LICENSE_KEYS {
                return Err(format!(
                    "Encrypted DASH requires more than {} distinct ClearKey KIDs",
                    super::MAX_LICENSE_KEYS
                ));
            }
        }
    }
    if required.is_empty() {
        return Err("Encrypted DASH media contains no protected sample KIDs".to_string());
    }
    Ok(required)
}

fn validate_auxiliary_information(
    bytes: &[u8],
    moof: BoxSpan,
    senc: BoxSpan,
    saiz: BoxSpan,
    saio: BoxSpan,
    expected_sizes: &[u8],
) -> Result<(), String> {
    let sizes = span_payload(bytes, saiz)?;
    if sizes.len() < 9 || sizes[0] != 0 || sizes[1..4] != [0, 0, 0] {
        return Err("DASH saiz is truncated or uses unsupported flags/version".to_string());
    }
    let default_size = sizes[4];
    let sample_count = usize::try_from(read_u32(sizes, 5)?)
        .map_err(|_| "DASH saiz sample count is out of range".to_string())?;
    if sample_count != expected_sizes.len() {
        return Err("DASH saiz sample count does not match senc".to_string());
    }
    if default_size == 0 {
        let entries = sizes
            .get(9..)
            .ok_or_else(|| "DASH saiz per-sample entries are truncated".to_string())?;
        if entries != expected_sizes {
            return Err("DASH saiz per-sample sizes do not match senc entries".to_string());
        }
    } else if sizes.len() != 9 || expected_sizes.iter().any(|size| *size != default_size) {
        return Err("DASH saiz default size does not match senc entries".to_string());
    }

    let offsets = span_payload(bytes, saio)?;
    if offsets.len() < 8 || offsets[0] > 1 || offsets[1..4] != [0, 0, 0] {
        return Err("DASH saio is truncated or uses unsupported flags/version".to_string());
    }
    let entry_count = usize::try_from(read_u32(offsets, 4)?)
        .map_err(|_| "DASH saio entry count is out of range".to_string())?;
    if entry_count != 1 {
        return Err("DASH saio must contain one in-band encryption-data offset".to_string());
    }
    let actual_offset = if offsets[0] == 0 {
        if offsets.len() != 12 {
            return Err("DASH saio version 0 has an invalid length".to_string());
        }
        u64::from(read_u32(offsets, 8)?)
    } else {
        if offsets.len() != 16 {
            return Err("DASH saio version 1 has an invalid length".to_string());
        }
        read_u64(offsets, 8)?
    };
    let absolute_offset = senc
        .payload_start()?
        .checked_add(8)
        .ok_or_else(|| "DASH senc auxiliary-data offset overflows".to_string())?;
    let relative_offset = absolute_offset
        .checked_sub(moof.start)
        .ok_or_else(|| "DASH senc auxiliary-data offset precedes its moof".to_string())?;
    let expected_offset = u64::try_from(relative_offset)
        .map_err(|_| "DASH senc auxiliary-data offset is out of range".to_string())?;
    if actual_offset != expected_offset {
        return Err(
            "DASH saio does not point to the in-band senc sample auxiliary data".to_string(),
        );
    }

    Ok(())
}

/// Decrypt one independently planned fragmented-MP4 representation into a
/// private XDG-staging file. Encrypted source resources are never modified.
pub(crate) fn decrypt_track(
    resources: &[DashResource],
    directory: &Path,
    output: &Path,
    encryption: DashEncryption,
    keys: &super::DashKeyMap,
    cancelled: &AtomicBool,
) -> Result<bool, String> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(false);
    }
    if resources.len() < 2
        || resources[0].kind != DashResourceKind::Initialization
        || resources[1..]
            .iter()
            .any(|resource| resource.kind != DashResourceKind::Segment)
    {
        return Err("Encrypted native DASH requires one initialization resource followed by fragmented media segments".to_string());
    }

    let init_path = directory.join(&resources[0].filename);
    let init = read_regular_file_bounded(&init_path, MAX_INIT_BYTES).map_err(|error| {
        format!("Could not read encrypted DASH initialization resource: {error}")
    })?;
    if init.is_empty() {
        return Err("Encrypted DASH initialization resource is empty".to_string());
    }
    let init_encryption = parse_init_encryption(&init, encryption)?;
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
        if cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        let path = directory.join(&resource.filename);
        let fragment = read_regular_file_bounded(&path, MAX_FRAGMENT_BYTES)
            .map_err(|error| format!("Could not read encrypted DASH media segment: {error}"))?;
        let encrypted_length = fragment.len();
        let decrypted = decrypt_fragment_with_keys(&fragment, &init, init_encryption, keys)?;
        if decrypted.len() != encrypted_length {
            return Err("VSD changed the length of a validated DASH fragment".to_string());
        }
        if cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        let length = u64::try_from(decrypted.len())
            .map_err(|_| "DASH decrypted fragment size is out of range".to_string())?;
        total = total
            .checked_add(length)
            .filter(|size| *size <= MAX_TRACK_BYTES)
            .ok_or_else(|| {
                "Decrypted DASH track exceeds Grab's 1 TiB native track limit".to_string()
            })?;
        writer
            .as_file_mut()
            .write_all(&decrypted)
            .map_err(|error| format!("Could not write decrypted DASH media: {error}"))?;
    }

    if cancelled.load(Ordering::Acquire) {
        return Ok(false);
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
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(payload.len() + 8).expect("test box size fits u32");
        let mut output = Vec::with_capacity(payload.len() + 8);
        output.extend_from_slice(&size.to_be_bytes());
        output.extend_from_slice(kind);
        output.extend_from_slice(payload);
        output
    }

    fn full_box(version: u8, flags: u32, fields: &[u8]) -> Vec<u8> {
        let mut output = vec![
            version,
            u8::try_from((flags >> 16) & 0xff).expect("flag byte"),
            u8::try_from((flags >> 8) & 0xff).expect("flag byte"),
            u8::try_from(flags & 0xff).expect("flag byte"),
        ];
        output.extend_from_slice(fields);
        output
    }

    #[derive(Default)]
    struct TestFragmentOptions {
        sample_description_index: Option<u32>,
        extra_traf: bool,
    }

    fn test_fragment(
        track_id: u32,
        trun_flags: u32,
        senc_count: u32,
        offset_adjustment: i32,
        use_subsamples: bool,
        subsample_encrypted_bytes: u32,
        extra_traf: bool,
    ) -> Vec<u8> {
        test_fragment_with_options(
            track_id,
            trun_flags,
            senc_count,
            offset_adjustment,
            use_subsamples,
            subsample_encrypted_bytes,
            TestFragmentOptions {
                extra_traf,
                ..TestFragmentOptions::default()
            },
        )
    }

    fn test_fragment_with_options(
        track_id: u32,
        trun_flags: u32,
        senc_count: u32,
        offset_adjustment: i32,
        use_subsamples: bool,
        subsample_encrypted_bytes: u32,
        options: TestFragmentOptions,
    ) -> Vec<u8> {
        let mut tfhd_fields = track_id.to_be_bytes().to_vec();
        let mut tfhd_flags = 0x020000;
        if let Some(index) = options.sample_description_index {
            tfhd_flags |= 0x000002;
            tfhd_fields.extend_from_slice(&index.to_be_bytes());
        }
        let tfhd_payload = full_box(0, tfhd_flags, &tfhd_fields);
        let tfhd_box = box_bytes(b"tfhd", &tfhd_payload);

        let mut senc_fields = senc_count.to_be_bytes().to_vec();
        senc_fields.extend_from_slice(&[0x11; 16]);
        if use_subsamples {
            senc_fields.extend_from_slice(&1_u16.to_be_bytes());
            senc_fields.extend_from_slice(&4_u16.to_be_bytes());
            senc_fields.extend_from_slice(&subsample_encrypted_bytes.to_be_bytes());
        }
        let senc_payload = full_box(0, if use_subsamples { 2 } else { 0 }, &senc_fields);
        let senc_box = box_bytes(b"senc", &senc_payload);

        let make_trun = |data_offset: i32| {
            let mut fields = 1_u32.to_be_bytes().to_vec();
            fields.extend_from_slice(&data_offset.to_be_bytes());
            fields.extend_from_slice(&16_u32.to_be_bytes());
            box_bytes(b"trun", &full_box(0, trun_flags, &fields))
        };

        let make_moof = |trun_box: &[u8]| {
            let mfhd_fields = 1_u32.to_be_bytes().to_vec();
            let mfhd = box_bytes(b"mfhd", &full_box(0, 0, &mfhd_fields));
            let mut traf_fields = tfhd_box.clone();
            traf_fields.extend_from_slice(trun_box);
            traf_fields.extend_from_slice(&senc_box);
            let traf = box_bytes(b"traf", &traf_fields);
            let mut moof_fields = mfhd;
            moof_fields.extend_from_slice(&traf);
            if options.extra_traf {
                moof_fields.extend_from_slice(&traf);
            }
            box_bytes(b"moof", &moof_fields)
        };

        let first_moof = make_moof(&make_trun(0));
        let data_offset = i32::try_from(first_moof.len() + 8)
            .expect("test data offset fits i32")
            .saturating_add(offset_adjustment);
        let moof = make_moof(&make_trun(data_offset));
        let mut output = moof;
        output.extend_from_slice(&box_bytes(b"mdat", &[0x55; 16]));
        output
    }

    fn test_init_encryption() -> InitEncryption {
        InitEncryption {
            track_id: 7,
            default_kid: [0x33; 16],
            iv_size: 16,
            constant_iv: None,
            constant_iv_size: None,
            crypt_blocks: 0,
            skip_blocks: 0,
        }
    }


    fn add_fragment_sample_groups(
        fragment: &[u8],
        sgpd: &[u8],
        sbgp: &[u8],
    ) -> Vec<u8> {
        let top = parse_box_spans(fragment, 0, fragment.len()).expect("valid synthetic fragment");
        let moof = find_one(&top, b"moof").expect("synthetic moof");
        let moof_children = parse_box_spans(
            fragment,
            moof.payload_start().expect("moof payload"),
            moof.end().expect("moof end"),
        ).expect("valid synthetic moof children");
        let mfhd = find_one(&moof_children, b"mfhd").expect("synthetic mfhd");
        let traf = find_one(&moof_children, b"traf").expect("synthetic traf");
        let traf_children = parse_box_spans(
            fragment,
            traf.payload_start().expect("traf payload"),
            traf.end().expect("traf end"),
        ).expect("valid synthetic traf children");
        let trun = find_one(&traf_children, b"trun").expect("synthetic trun");

        let make_moof = |data_offset: i32| {
            let mut traf_fields = Vec::new();
            for child in &traf_children {
                if &child.kind == b"trun" {
                    let mut payload = span_payload(fragment, *trun).expect("trun payload").to_vec();
                    payload[8..12].copy_from_slice(&data_offset.to_be_bytes());
                    traf_fields.extend_from_slice(&box_bytes(b"trun", &payload));
                } else {
                    traf_fields.extend_from_slice(
                        fragment
                            .get(child.start..child.end().expect("child end"))
                            .expect("child bytes"),
                    );
                }
            }
            traf_fields.extend_from_slice(sgpd);
            traf_fields.extend_from_slice(sbgp);
            let traf_box = box_bytes(b"traf", &traf_fields);
            let mut moof_fields = fragment
                .get(mfhd.start..mfhd.end().expect("mfhd end"))
                .expect("mfhd bytes")
                .to_vec();
            moof_fields.extend_from_slice(&traf_box);
            box_bytes(b"moof", &moof_fields)
        };

        let initial = make_moof(0);
        let offset = i32::try_from(initial.len() + 8).expect("sample data offset fits i32");
        let moof = make_moof(offset);
        let mdat = top.iter().find(|span| &span.kind == b"mdat").expect("synthetic mdat");
        let mut output = moof;
        output.extend_from_slice(
            fragment
                .get(mdat.start..mdat.end().expect("mdat end"))
                .expect("mdat bytes"),
        );
        output
    }


    fn synthetic_two_sample_fragment(
        first_ciphertext: &[u8; 16],
        first_iv: &[u8; 16],
        second_ciphertext: &[u8; 16],
        second_iv: &[u8; 16],
    ) -> Vec<u8> {
        let tfhd = box_bytes(b"tfhd", &full_box(0, 0x020000, &7_u32.to_be_bytes()));
        let tfdt = box_bytes(b"tfdt", &full_box(0, 0, &0_u32.to_be_bytes()));
        let mut senc_fields = 2_u32.to_be_bytes().to_vec();
        senc_fields.extend_from_slice(first_iv);
        senc_fields.extend_from_slice(second_iv);
        let senc = box_bytes(b"senc", &full_box(0, 0, &senc_fields));
        let make_trun = |data_offset: i32| {
            let mut fields = 2_u32.to_be_bytes().to_vec();
            fields.extend_from_slice(&data_offset.to_be_bytes());
            fields.extend_from_slice(&16_u32.to_be_bytes());
            fields.extend_from_slice(&16_u32.to_be_bytes());
            box_bytes(b"trun", &full_box(0, 0x000201, &fields))
        };
        let make_moof = |trun: &[u8]| {
            let mfhd = box_bytes(b"mfhd", &full_box(0, 0, &1_u32.to_be_bytes()));
            let mut traf_fields = tfhd.clone();
            traf_fields.extend_from_slice(&tfdt);
            traf_fields.extend_from_slice(trun);
            traf_fields.extend_from_slice(&senc);
            let traf = box_bytes(b"traf", &traf_fields);
            let mut moof_fields = mfhd;
            moof_fields.extend_from_slice(&traf);
            box_bytes(b"moof", &moof_fields)
        };
        let initial_moof = make_moof(&make_trun(0));
        let offset = i32::try_from(initial_moof.len() + 8).expect("two-sample offset fits i32");
        let mut ciphertext = Vec::with_capacity(32);
        ciphertext.extend_from_slice(first_ciphertext);
        ciphertext.extend_from_slice(second_ciphertext);
        let mut fragment = make_moof(&make_trun(offset));
        fragment.extend_from_slice(&box_bytes(b"mdat", &ciphertext));
        fragment
    }

    #[test]
    fn decrypts_samples_with_distinct_seig_kids_in_one_fragment() {
        let first_kid = [0x44; 16];
        let second_kid = [0x55; 16];
        let first_iv = [
            0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27,
            0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
        ];
        let second_iv = [
            0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
            0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
        ];
        let cenc_first = [
            0xb8, 0x78, 0xd8, 0x9b, 0xed, 0xdb, 0x8e, 0x7e,
            0xfb, 0x0c, 0xde, 0xfb, 0xb0, 0x92, 0x5c, 0xb5,
        ];
        let cenc_second = [
            0x1c, 0x8d, 0xa0, 0xcb, 0x06, 0x69, 0xfc, 0x76,
            0xf2, 0xba, 0x1e, 0x78, 0xc8, 0x03, 0x4c, 0x6e,
        ];
        let cbcs_first = [
            0x13, 0x13, 0xf2, 0xa7, 0x28, 0x0f, 0x62, 0xb8,
            0xde, 0x96, 0x01, 0x95, 0xf9, 0x83, 0xb3, 0x2a,
        ];
        let cbcs_second = [
            0x0e, 0xbc, 0xde, 0x5e, 0x8c, 0x8c, 0x3e, 0xfd,
            0x57, 0x3d, 0x0e, 0x8b, 0x91, 0x5d, 0x88, 0x82,
        ];
        let expected = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
        ];

        for (
            scheme,
            fourcc,
            version,
            pattern,
            pattern_byte,
            first_ciphertext,
            second_ciphertext,
        ) in [
            (EncryptionScheme::Cenc, b"cenc", 0, (0, 0), 0, cenc_first, cenc_second),
            (EncryptionScheme::Cbcs, b"cbcs", 1, (1, 9), 0x19, cbcs_first, cbcs_second),
        ] {
            let (init, default_kid) = synthetic_init(fourcc, version, pattern, false);
            let mut first_entry = vec![0, pattern_byte, 1, 16];
            first_entry.extend_from_slice(&first_kid);
            let mut second_entry = vec![0, pattern_byte, 1, 16];
            second_entry.extend_from_slice(&second_kid);
            let mut sgpd_fields = b"seig".to_vec();
            sgpd_fields.extend_from_slice(&20_u32.to_be_bytes());
            sgpd_fields.extend_from_slice(&2_u32.to_be_bytes());
            sgpd_fields.extend_from_slice(&first_entry);
            sgpd_fields.extend_from_slice(&second_entry);
            let sgpd = box_bytes(b"sgpd", &full_box(1, 0, &sgpd_fields));
            let mut sbgp_fields = b"seig".to_vec();
            sbgp_fields.extend_from_slice(&2_u32.to_be_bytes());
            sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
            sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
            sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
            sbgp_fields.extend_from_slice(&2_u32.to_be_bytes());
            let sbgp = box_bytes(b"sbgp", &full_box(0, 0, &sbgp_fields));
            let fragment = add_fragment_sample_groups(
                &synthetic_two_sample_fragment(
                    &first_ciphertext,
                    &first_iv,
                    &second_ciphertext,
                    &second_iv,
                ),
                &sgpd,
                &sbgp,
            );
            let parsed_init = parse_init_encryption(
                &init,
                DashEncryption { scheme, default_kid },
            )
            .expect("parse synthetic init");
            let keys = super::super::DashKeyMap {
                keys: BTreeMap::from([
                    (first_kid, Zeroizing::new([0x10; 16])),
                    (second_kid, Zeroizing::new([0x11; 16])),
                ]),
            };
            let decrypted = decrypt_fragment_with_keys(&fragment, &init, parsed_init, &keys)
                .expect("decrypt both KID groups in one fragment");
            let boxes = parse_box_spans(&decrypted, 0, decrypted.len())
                .expect("decrypted fragment boxes");
            let mdat = boxes
                .iter()
                .find(|span| &span.kind == b"mdat")
                .copied()
                .expect("decrypted mdat");
            let start = mdat.payload_start().expect("mdat payload start");
            let end = mdat.end().expect("mdat end");
            assert_eq!(decrypted.get(start..end), Some(expected.as_slice()));
        }
    }

    #[test]
    fn resolves_fragment_local_seig_kid_rotation_and_decrypts_with_rotated_key() {
        let (init, default_kid) = synthetic_init(b"cenc", 0, (0, 0), false);
        let rotated_kid = [0x44; 16];
        let mut entry = vec![0, 0, 1, 16];
        entry.extend_from_slice(&rotated_kid);
        let mut sgpd_fields = b"seig".to_vec();
        sgpd_fields.extend_from_slice(&20_u32.to_be_bytes());
        sgpd_fields.extend_from_slice(&1_u32.to_be_bytes());
        sgpd_fields.extend_from_slice(&entry);
        let sgpd = box_bytes(b"sgpd", &full_box(1, 0, &sgpd_fields));
        let mut sbgp_fields = b"seig".to_vec();
        sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
        sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
        sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
        let sbgp = box_bytes(b"sbgp", &full_box(0, 0, &sbgp_fields));

        let ciphertext = [
            0xb8, 0x78, 0xd8, 0x9b, 0xed, 0xdb, 0x8e, 0x7e,
            0xfb, 0x0c, 0xde, 0xfb, 0xb0, 0x92, 0x5c, 0xb5,
        ];
        let iv = [
            0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27,
            0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
        ];
        let fragment = add_fragment_sample_groups(
            &synthetic_fragment(&ciphertext, &iv),
            &sgpd,
            &sbgp,
        );
        let parsed_init = parse_init_encryption(
            &init,
            DashEncryption { scheme: EncryptionScheme::Cenc, default_kid },
        ).expect("parse synthetic init");
        let samples = fragment_samples(&fragment, parsed_init)
            .expect("parse rotated sample group");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].kid, rotated_kid);
        let key_hex = Zeroizing::new(hex_string(&[0x10; 16]));
        let decrypter = vsd_mp4::decrypt::CencDecrypter::with_init(&key_hex, &init)
            .expect("initialize rotated sample decrypter");
        let plaintext = decrypt_one_sample(&fragment, &init, &samples[0], &decrypter)
            .expect("decrypt rotated sample using its selected KID key");
        assert_eq!(plaintext, [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        ]);
    }

    #[test]
    fn rejects_incomplete_seig_sample_group_coverage() {
        let init = test_init_encryption();
        let rotated_kid = [0x44; 16];
        let mut entry = vec![0, 0, 1, 16];
        entry.extend_from_slice(&rotated_kid);
        let mut sgpd_fields = b"seig".to_vec();
        sgpd_fields.extend_from_slice(&20_u32.to_be_bytes());
        sgpd_fields.extend_from_slice(&1_u32.to_be_bytes());
        sgpd_fields.extend_from_slice(&entry);
        let sgpd = box_bytes(b"sgpd", &full_box(1, 0, &sgpd_fields));
        let mut sbgp_fields = b"seig".to_vec();
        sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
        sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
        sbgp_fields.extend_from_slice(&1_u32.to_be_bytes());
        let sbgp = box_bytes(b"sbgp", &full_box(0, 0, &sbgp_fields));
        let mut groups = sgpd;
        groups.extend_from_slice(&sbgp);
        let spans = parse_box_spans(&groups, 0, groups.len()).expect("sample group boxes");
        assert!(sample_group_kids(&groups, &spans, 2, init).is_err());
    }

    fn synthetic_init(
        scheme: &[u8; 4],
        tenc_version: u8,
        pattern: (u8, u8),
        misplaced_sinf: bool,
    ) -> (Vec<u8>, [u8; 16]) {
        let kid = [0x33; 16];

        let mut tenc_fields = Vec::new();
        tenc_fields.push(0); // reserved
        if tenc_version == 0 {
            tenc_fields.push(0); // reserved
        } else {
            tenc_fields.push((pattern.0 << 4) | pattern.1);
        }
        tenc_fields.push(1); // is_protected
        tenc_fields.push(16); // per_sample_iv_size
        tenc_fields.extend_from_slice(&kid);
        let tenc = box_bytes(b"tenc", &full_box(tenc_version, 0, &tenc_fields));

        let frma = box_bytes(b"frma", b"avc1");
        let mut schm_fields = scheme.to_vec();
        schm_fields.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
        let schm = box_bytes(b"schm", &full_box(0, 0, &schm_fields));
        let schi = box_bytes(b"schi", &tenc);
        let mut sinf_fields = frma;
        sinf_fields.extend_from_slice(&schm);
        sinf_fields.extend_from_slice(&schi);
        let sinf = box_bytes(b"sinf", &sinf_fields);

        let mut encv_fields = vec![0_u8; 78];
        if !misplaced_sinf {
            encv_fields.extend_from_slice(&sinf);
        }
        let encv = box_bytes(b"encv", &encv_fields);
        let mut stsd_fields = 1_u32.to_be_bytes().to_vec();
        stsd_fields.extend_from_slice(&encv);
        let stsd = box_bytes(b"stsd", &full_box(0, 0, &stsd_fields));
        let stbl = box_bytes(b"stbl", &stsd);
        let mut minf_fields = stbl;
        if misplaced_sinf {
            minf_fields.extend_from_slice(&sinf);
        }
        let minf = box_bytes(b"minf", &minf_fields);
        let mdia = box_bytes(b"mdia", &minf);

        let mut tkhd_fields = Vec::new();
        tkhd_fields.extend_from_slice(&0_u32.to_be_bytes()); // creation_time
        tkhd_fields.extend_from_slice(&0_u32.to_be_bytes()); // modification_time
        tkhd_fields.extend_from_slice(&7_u32.to_be_bytes()); // track_ID
        tkhd_fields.extend_from_slice(&0_u32.to_be_bytes()); // reserved
        tkhd_fields.extend_from_slice(&0_u32.to_be_bytes()); // duration
        tkhd_fields.extend_from_slice(&[0_u8; 8]); // reserved
        tkhd_fields.extend_from_slice(&0_u16.to_be_bytes()); // layer
        tkhd_fields.extend_from_slice(&0_u16.to_be_bytes()); // alternate_group
        tkhd_fields.extend_from_slice(&0_u16.to_be_bytes()); // volume
        tkhd_fields.extend_from_slice(&0_u16.to_be_bytes()); // reserved
        for matrix_value in [0x0001_0000_u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            tkhd_fields.extend_from_slice(&matrix_value.to_be_bytes());
        }
        tkhd_fields.extend_from_slice(&0_u32.to_be_bytes()); // width
        tkhd_fields.extend_from_slice(&0_u32.to_be_bytes()); // height
        let tkhd = box_bytes(b"tkhd", &full_box(0, 7, &tkhd_fields));

        let mut trak_fields = tkhd;
        trak_fields.extend_from_slice(&mdia);
        let trak = box_bytes(b"trak", &trak_fields);

        let mut trex_fields = Vec::new();
        trex_fields.extend_from_slice(&7_u32.to_be_bytes()); // track_ID
        trex_fields.extend_from_slice(&1_u32.to_be_bytes()); // default_sample_description_index
        trex_fields.extend_from_slice(&0_u32.to_be_bytes()); // default_sample_duration
        trex_fields.extend_from_slice(&0_u32.to_be_bytes()); // default_sample_size
        trex_fields.extend_from_slice(&0_u32.to_be_bytes()); // default_sample_flags
        let trex = box_bytes(b"trex", &full_box(0, 0, &trex_fields));
        let mvex = box_bytes(b"mvex", &trex);
        let mut moov_fields = trak;
        moov_fields.extend_from_slice(&mvex);
        let moov = box_bytes(b"moov", &moov_fields);

        let mut ftyp_fields = b"isom".to_vec();
        ftyp_fields.extend_from_slice(&0_u32.to_be_bytes());
        ftyp_fields.extend_from_slice(b"isom");
        let ftyp = box_bytes(b"ftyp", &ftyp_fields);

        let mut init = ftyp;
        init.extend_from_slice(&moov);
        (init, kid)
    }

    fn synthetic_fragment(ciphertext: &[u8], iv: &[u8; 16]) -> Vec<u8> {
        let tfhd = box_bytes(b"tfhd", &full_box(0, 0x020000, &7_u32.to_be_bytes()));
        let tfdt = box_bytes(b"tfdt", &full_box(0, 0, &0_u32.to_be_bytes()));
        let mut senc_fields = 1_u32.to_be_bytes().to_vec();
        senc_fields.extend_from_slice(iv);
        let senc = box_bytes(b"senc", &full_box(0, 0, &senc_fields));

        let make_trun = |data_offset: i32| {
            let mut fields = 1_u32.to_be_bytes().to_vec();
            fields.extend_from_slice(&data_offset.to_be_bytes());
            fields.extend_from_slice(
                &u32::try_from(ciphertext.len())
                    .expect("synthetic sample size fits u32")
                    .to_be_bytes(),
            );
            box_bytes(b"trun", &full_box(0, 0x000201, &fields))
        };
        let make_moof = |trun: &[u8]| {
            let mfhd = box_bytes(b"mfhd", &full_box(0, 0, &1_u32.to_be_bytes()));
            let mut traf_fields = tfhd.clone();
            traf_fields.extend_from_slice(&tfdt);
            traf_fields.extend_from_slice(trun);
            traf_fields.extend_from_slice(&senc);
            let traf = box_bytes(b"traf", &traf_fields);
            let mut moof_fields = mfhd;
            moof_fields.extend_from_slice(&traf);
            box_bytes(b"moof", &moof_fields)
        };

        let initial_moof = make_moof(&make_trun(0));
        let offset =
            i32::try_from(initial_moof.len() + 8).expect("synthetic fragment offset fits i32");
        let moof = make_moof(&make_trun(offset));
        let mut fragment = moof;
        fragment.extend_from_slice(&box_bytes(b"mdat", ciphertext));
        fragment
    }

    #[derive(Clone, Copy)]
    struct SyntheticTrackOptions {
        pattern: (u8, u8),
        use_byte_ranges: bool,
    }

    #[test]
    fn restores_full_box_headers_before_validating_encryption_metadata() {
        let (init, kid) = synthetic_init(b"cenc", 0, (0, 0), false);
        let parsed = parse_init_encryption(
            &init,
            DashEncryption {
                scheme: EncryptionScheme::Cenc,
                default_kid: kid,
            },
        )
        .expect("synthetic FullBox encryption metadata should parse");

        assert_eq!(parsed.track_id, 7);
        assert_eq!(parsed.iv_size, 16);
        assert_eq!(parsed.constant_iv, None);
    }

    fn decrypt_synthetic_track(
        scheme: EncryptionScheme,
        scheme_fourcc: &[u8; 4],
        tenc_version: u8,
        iv: [u8; 16],
        ciphertext: &[u8],
        options: SyntheticTrackOptions,
    ) -> Vec<u8> {
        let (init, kid) = synthetic_init(scheme_fourcc, tenc_version, options.pattern, false);
        let directory = tempfile::tempdir().expect("create encrypted DASH test directory");
        let init_name = "init.mp4";
        let segment_name = "segment.m4s";
        std::fs::write(directory.path().join(init_name), &init)
            .expect("write synthetic DASH initialization");
        std::fs::write(
            directory.path().join(segment_name),
            synthetic_fragment(ciphertext, &iv),
        )
        .expect("write synthetic DASH fragment");

        let init_range = options
            .use_byte_ranges
            .then_some(crate::manifest::ManifestByteRange {
                offset: 0,
                length: u64::try_from(init.len()).expect("synthetic init length fits u64"),
            });
        let segment_range = options
            .use_byte_ranges
            .then_some(crate::manifest::ManifestByteRange {
                offset: 100,
                length: u64::try_from(synthetic_fragment(ciphertext, &iv).len())
                    .expect("synthetic fragment length fits u64"),
            });
        let resources = [
            DashResource {
                url: url::Url::parse("https://example.invalid/init.mp4")
                    .expect("valid test initialization URL"),
                range: init_range,
                filename: init_name.to_string(),
                kind: DashResourceKind::Initialization,
                segment_number: None,
            },
            DashResource {
                url: url::Url::parse("https://example.invalid/segment.m4s")
                    .expect("valid test segment URL"),
                range: segment_range,
                filename: segment_name.to_string(),
                kind: DashResourceKind::Segment,
                segment_number: Some(1),
            },
        ];
        let output = directory.path().join("decrypted.mp4");
        let keys = super::super::DashKeyMap {
            keys: BTreeMap::from([(kid, Zeroizing::new([0x10; 16]))]),
        };
        decrypt_track(
            &resources,
            directory.path(),
            &output,
            DashEncryption {
                scheme,
                default_kid: kid,
            },
            &keys,
            &AtomicBool::new(false),
        )
        .expect("decrypt synthetic DASH track");
        std::fs::read(output).expect("read decrypted DASH track")
    }

    #[test]
    fn decrypts_cenc_and_cbcs_fragmented_tracks_end_to_end() {
        let plaintext = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let iv = [
            0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d,
            0x2e, 0x2f,
        ];
        let cenc_ciphertext = [
            0xb8, 0x78, 0xd8, 0x9b, 0xed, 0xdb, 0x8e, 0x7e, 0xfb, 0x0c, 0xde, 0xfb, 0xb0, 0x92,
            0x5c, 0xb5,
        ];
        let cbcs_ciphertext = [
            0x13, 0x13, 0xf2, 0xa7, 0x28, 0x0f, 0x62, 0xb8, 0xde, 0x96, 0x01, 0x95, 0xf9, 0x83,
            0xb3, 0x2a,
        ];

        for (scheme, fourcc, version, ciphertext) in [
            (EncryptionScheme::Cenc, b"cenc", 0, cenc_ciphertext),
            (EncryptionScheme::Cbcs, b"cbcs", 1, cbcs_ciphertext),
        ] {
            let decrypted = decrypt_synthetic_track(
                scheme,
                fourcc,
                version,
                iv,
                &ciphertext,
                SyntheticTrackOptions {
                    pattern: (0, 0),
                    use_byte_ranges: false,
                },
            );
            let boxes = parse_box_spans(&decrypted, 0, decrypted.len())
                .expect("decrypted file has complete MP4 boxes");
            let mdat = boxes
                .iter()
                .find(|span| &span.kind == b"mdat")
                .copied()
                .expect("decrypted track contains mdat");
            let start = mdat.payload_start().expect("mdat payload start");
            let end = mdat.end().expect("mdat end");
            assert_eq!(
                decrypted.get(start..end),
                Some(plaintext.as_slice()),
                "{scheme:?} sample should be plaintext after VSD decryption",
            );
        }
    }

    #[test]
    fn decrypts_cbcs_crypt_skip_patterns_without_touching_clear_blocks() {
        let plaintext: [u8; 192] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29,
            0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45,
            0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53,
            0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x61,
            0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f,
            0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d,
            0x7e, 0x7f, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b,
            0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
            0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
            0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5,
            0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf,
        ];
        let ciphertext: [u8; 192] = [
            0x13, 0x13, 0xf2, 0xa7, 0x28, 0x0f, 0x62, 0xb8, 0xde, 0x96, 0x01, 0x95, 0xf9, 0x83,
            0xb3, 0x2a, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29,
            0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45,
            0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53,
            0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x61,
            0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f,
            0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d,
            0x7e, 0x7f, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b,
            0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
            0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0x31, 0x94, 0x9c, 0x67, 0x78, 0x30, 0x31, 0x45,
            0x52, 0xa9, 0xfc, 0x3c, 0x71, 0x05, 0xa9, 0x3f, 0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5,
            0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf,
        ];
        let iv = [
            0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d,
            0x2e, 0x2f,
        ];
        let decrypted = decrypt_synthetic_track(
            EncryptionScheme::Cbcs,
            b"cbcs",
            1,
            iv,
            &ciphertext,
            SyntheticTrackOptions {
                pattern: (1, 9),
                use_byte_ranges: false,
            },
        );
        let boxes = parse_box_spans(&decrypted, 0, decrypted.len())
            .expect("decrypted CBCS file has complete MP4 boxes");
        let mdat = boxes
            .iter()
            .find(|span| &span.kind == b"mdat")
            .copied()
            .expect("decrypted CBCS track contains mdat");
        let start = mdat.payload_start().expect("mdat payload start");
        let end = mdat.end().expect("mdat end");
        assert_eq!(decrypted.get(start..end), Some(plaintext.as_slice()));
    }

    #[test]
    fn decrypts_indexed_segment_base_resources_with_byte_ranges() {
        let plaintext = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let ciphertext = [
            0xb8, 0x78, 0xd8, 0x9b, 0xed, 0xdb, 0x8e, 0x7e, 0xfb, 0x0c, 0xde, 0xfb, 0xb0, 0x92,
            0x5c, 0xb5,
        ];
        let iv = [
            0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d,
            0x2e, 0x2f,
        ];
        let decrypted = decrypt_synthetic_track(
            EncryptionScheme::Cenc,
            b"cenc",
            0,
            iv,
            &ciphertext,
            SyntheticTrackOptions {
                pattern: (0, 0),
                use_byte_ranges: true,
            },
        );
        let boxes = parse_box_spans(&decrypted, 0, decrypted.len())
            .expect("decrypted SegmentBase track has complete MP4 boxes");
        let mdat = boxes
            .iter()
            .find(|span| &span.kind == b"mdat")
            .copied()
            .expect("decrypted SegmentBase track contains mdat");
        let start = mdat.payload_start().expect("mdat payload start");
        let end = mdat.end().expect("mdat end");
        assert_eq!(decrypted.get(start..end), Some(plaintext.as_slice()));
    }

    #[test]
    fn parses_hyphenated_and_compact_hex_identifiers() {
        let value = "00112233-4455-6677-8899-aabbccddeeff";
        let expected = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(super::super::parse_hex_16(value), Some(expected));
        assert_eq!(
            super::super::parse_hex_16("00112233445566778899aabbccddeeff"),
            Some(expected)
        );
        assert_eq!(
            super::super::parse_hex_16("00112233445566778899aabbccddeef"),
            None
        );
        assert_eq!(
            super::super::parse_hex_16("zz112233445566778899aabbccddeeff"),
            None
        );
    }

    #[test]
    fn rejects_tenc_outside_the_encrypted_sample_entry_hierarchy() {
        let (init, kid) = synthetic_init(b"cenc", 0, (0, 0), true);
        let result = parse_init_encryption(
            &init,
            DashEncryption {
                scheme: EncryptionScheme::Cenc,
                default_kid: kid,
            },
        );
        assert!(
            result.is_err(),
            "a misplaced decoy tenc must not enable decryption"
        );
    }

    #[test]
    fn cancelled_decryption_does_not_publish_a_track() {
        let directory = tempfile::tempdir().expect("create cancelled DRM staging directory");
        let output = directory.path().join("decrypted.mp4");
        let keys = super::super::DashKeyMap {
            keys: BTreeMap::from([([0x33; 16], Zeroizing::new([0x10; 16]))]),
        };
        let result = decrypt_track(
            &[],
            directory.path(),
            &output,
            DashEncryption {
                scheme: EncryptionScheme::Cenc,
                default_kid: [0x33; 16],
            },
            &keys,
            &AtomicBool::new(true),
        );
        assert!(matches!(result, Ok(false)));
        assert!(!output.exists());
    }

    #[test]
    fn rejects_fragment_boxes_with_truncated_or_oversized_headers() {
        assert!(parse_box_spans(b"\0\0\0", 0, 3).is_err());
        assert!(parse_box_spans(b"\0\0\0\x01moof", 0, 8).is_err());
        assert!(parse_box_spans(b"\0\0\0\x04moof", 0, 8).is_err());
    }

    #[test]
    fn accepts_only_the_validated_sample_description_index() {
        let valid = test_fragment_with_options(
            7,
            0x000201,
            1,
            0,
            false,
            0,
            TestFragmentOptions {
                sample_description_index: Some(1),
                ..TestFragmentOptions::default()
            },
        );
        let invalid = test_fragment_with_options(
            7,
            0x000201,
            1,
            0,
            false,
            0,
            TestFragmentOptions {
                sample_description_index: Some(2),
                ..TestFragmentOptions::default()
            },
        );
        assert!(validate_fragment(&valid, test_init_encryption()).is_ok());
        assert!(validate_fragment(&invalid, test_init_encryption()).is_err());
    }

    #[test]
    fn accepts_one_track_fragment_with_exact_sample_coverage() {
        let fragment = test_fragment(7, 0x000201, 1, 0, false, 0, false);
        assert!(validate_fragment(&fragment, test_init_encryption()).is_ok());
    }

    #[test]
    fn accepts_exact_subsample_encryption_coverage() {
        let fragment = test_fragment(7, 0x000201, 1, 0, true, 12, false);
        assert!(validate_fragment(&fragment, test_init_encryption()).is_ok());
    }

    #[test]
    fn rejects_wrong_track_ids_and_fragment_data_offsets() {
        let wrong_track = test_fragment(8, 0x000201, 1, 0, false, 0, false);
        let wrong_offset = test_fragment(7, 0x000201, 1, 1, false, 0, false);
        assert!(validate_fragment(&wrong_track, test_init_encryption()).is_err());
        assert!(validate_fragment(&wrong_offset, test_init_encryption()).is_err());
    }

    #[test]
    fn rejects_count_mismatches_and_unknown_sample_flags() {
        let wrong_senc_count = test_fragment(7, 0x000201, 2, 0, false, 0, false);
        let unknown_trun_flag = test_fragment(7, 0x000241, 1, 0, false, 0, false);
        assert!(validate_fragment(&wrong_senc_count, test_init_encryption()).is_err());
        assert!(validate_fragment(&unknown_trun_flag, test_init_encryption()).is_err());
    }

    #[test]
    fn rejects_multiple_track_fragments_and_bad_subsample_sizes() {
        let multiple_tracks = test_fragment(7, 0x000201, 1, 0, false, 0, true);
        let short_subsample = test_fragment(7, 0x000201, 1, 0, true, 11, false);
        assert!(validate_fragment(&multiple_tracks, test_init_encryption()).is_err());
        assert!(validate_fragment(&short_subsample, test_init_encryption()).is_err());
    }
}
