//! Exact-boundary ad removal for static HLS SCTE-35 date ranges.
//!
//! m3u8-rs associates each DATERANGE tag with the following media segment and
//! stores only one date range per segment. The raw playlist is checked before
//! using that model so multiple tags are never silently overwritten.

use std::collections::HashMap;

use broadcast_common::Parse;
use m3u8_rs::{DateRange, MediaPlaylist, QuotedOrUnquoted};
use scte35_splice::{
    SpliceInfoSection,
    commands::{AnyCommand, SpliceInsert},
};

use super::{
    ManifestError,
    hls_ads::{AdRemovalPlan, AdSegmentDisposition},
};

const BOUNDARY_TOLERANCE_MICROS: i64 = 50_000;

#[derive(Default)]
struct SpliceRange {
    start_micros: Option<i64>,
    class: Option<String>,
    out_signal: Option<String>,
    in_signal: Option<String>,
    out_signal_identity: Option<SpliceSignalIdentity>,
    in_signal_identity: Option<SpliceSignalIdentity>,
    end_micros: Option<i64>,
    actual_duration_micros: Option<i64>,
    attached_segments: Vec<usize>,
}

struct SegmentTimeline {
    component: usize,
    start_micros: i64,
    end_micros: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SpliceSignalIdentity {
    event_id: u32,
    unique_program_id: u16,
}

/// Detect DATERANGE tags that m3u8-rs would overwrite before parsing.
pub(super) fn validate_tag_layout(body: &[u8]) -> Result<(), ManifestError> {
    let Ok(text) = std::str::from_utf8(body) else {
        // The primary parser will return the canonical UTF-8/parse error.
        return Ok(());
    };
    let mut pending_ranges = 0_usize;
    for line in text.lines().map(str::trim) {
        if line.starts_with("#EXT-X-DATERANGE:") {
            pending_ranges += 1;
            if pending_ranges > 1 {
                return Err(unsupported(
                    "multiple DATERANGE tags precede one media segment; the parser cannot preserve all of them",
                ));
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            pending_ranges = 0;
        }
    }
    if pending_ranges != 0 {
        return Err(unsupported(
            "DATERANGE tag is not followed by a media segment",
        ));
    }
    Ok(())
}

/// Return a plan only for complete SCTE-35 splice-out/splice-in pairs.
/// DATERANGE metadata without SCTE35-OUT/IN remains ordinary playlist metadata.
pub(super) fn plan_ad_removal(
    playlist: &MediaPlaylist,
    is_live: bool,
) -> Result<Option<AdRemovalPlan>, ManifestError> {
    let mut ranges = HashMap::<String, SpliceRange>::new();
    let mut has_splice_signal = false;

    for (segment_index, segment) in playlist.segments.iter().enumerate() {
        let Some(date_range) = segment.daterange.as_ref() else {
            continue;
        };
        let out_signal = attribute(date_range, "SCTE35-OUT");
        let in_signal = attribute(date_range, "SCTE35-IN");
        if out_signal.is_none() && in_signal.is_none() {
            continue;
        }
        has_splice_signal = true;
        if date_range.id.trim().is_empty() {
            return Err(unsupported("DATERANGE ID is empty"));
        }

        if date_range.end_on_next
            || date_range
                .x_prefixed
                .as_ref()
                .is_some_and(|attributes| !attributes.is_empty())
        {
            return Err(unsupported(
                "SCTE-35 DATERANGE has unsupported extension attributes",
            ));
        }
        if date_range
            .other_attributes
            .as_ref()
            .is_some_and(|attributes| {
                attributes
                    .keys()
                    .any(|key| key != "SCTE35-OUT" && key != "SCTE35-IN")
            })
        {
            return Err(unsupported(
                "SCTE-35 DATERANGE contains unrecognized attributes",
            ));
        }
        let range = ranges.entry(date_range.id.clone()).or_default();
        if let Some(value) = out_signal {
            let identity = validate_scte35_signal(value, true)?;
            merge_identity(
                &mut range.out_signal_identity,
                identity,
                "SCTE35-OUT",
            )?;
            merge_same_string(&mut range.out_signal, value, "SCTE35-OUT")?;
        }
        if let Some(value) = in_signal {
            let identity = validate_scte35_signal(value, false)?;
            merge_identity(
                &mut range.in_signal_identity,
                identity,
                "SCTE35-IN",
            )?;
            merge_same_string(&mut range.in_signal, value, "SCTE35-IN")?;
        }

        let start_micros = date_range.start_date.timestamp_micros();
        merge_same(&mut range.start_micros, start_micros, "START-DATE")?;
        if let Some(class) = date_range.class.as_ref() {
            if range.class.as_ref().is_some_and(|current| current != class) {
                return Err(unsupported("DATERANGE CLASS changes for one ID"));
            }
            range.class = Some(class.clone());
        }
        if in_signal.is_some() {
            if let Some(end_date) = date_range.end_date.as_ref() {
                merge_same(
                    &mut range.end_micros,
                    end_date.timestamp_micros(),
                    "END-DATE",
                )?;
            }
            if let Some(duration) = date_range.duration {
                merge_same(
                    &mut range.actual_duration_micros,
                    seconds_to_micros(duration)?,
                    "DURATION",
                )?;
            }
        }
        if let Some(planned_duration) = date_range.planned_duration {
            // Validate the hint, but never use it to infer the actual ad end.
            if !planned_duration.is_finite() || planned_duration < 0.0 {
                return Err(unsupported("DATERANGE PLANNED-DURATION is invalid"));
            }
        }
        range.attached_segments.push(segment_index);
    }

    if !has_splice_signal {
        return Ok(None);
    }
    if is_live {
        return Err(unsupported(
            "live DATERANGE ad breaks require cross-snapshot splice state",
        ));
    }
    if playlist.start.is_some() {
        return Err(unsupported(
            "EXT-X-START offsets cannot be preserved after DATERANGE ad removal",
        ));
    }

    let timeline = segment_timeline(playlist)?;
    let mut mapped = Vec::with_capacity(ranges.len());
    for (id, range) in &ranges {
        let Some(out_signal) = range.out_signal.as_deref() else {
            return Err(unsupported("SCTE35-IN has no matching SCTE35-OUT"));
        };
        let Some(_in_signal) = range.in_signal.as_deref() else {
            return Err(unsupported("SCTE35-OUT has no matching SCTE35-IN"));
        };
        let out_identity = range
            .out_signal_identity
            .ok_or_else(|| unsupported("SCTE35-OUT has no parsed splice identity"))?;
        let in_identity = range
            .in_signal_identity
            .ok_or_else(|| unsupported("SCTE35-IN has no parsed splice identity"))?;
        if out_identity != in_identity {
            return Err(unsupported(
                "SCTE35-OUT and SCTE35-IN do not identify the same splice event",
            ));
        }
        let start = range
            .start_micros
            .ok_or_else(|| unsupported("DATERANGE has no START-DATE"))?;
        let end_from_duration = range
            .actual_duration_micros
            .and_then(|duration| start.checked_add(duration));
        let end = match (range.end_micros, end_from_duration) {
            (Some(end), Some(from_duration)) => {
                if end.abs_diff(from_duration) > BOUNDARY_TOLERANCE_MICROS as u64 {
                    return Err(unsupported("DATERANGE END-DATE and DURATION disagree"));
                }
                end
            }
            (Some(end), None) => end,
            (None, Some(end)) => end,
            (None, None) => {
                return Err(unsupported(
                    "SCTE35-IN does not provide an actual END-DATE or DURATION",
                ));
            }
        };
        if end <= start {
            return Err(unsupported("DATERANGE end does not follow its start"));
        }

        let starts = timeline
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                value
                    .as_ref()
                    .filter(|entry| {
                        entry.start_micros.abs_diff(start)
                            <= BOUNDARY_TOLERANCE_MICROS as u64
                    })
                    .map(|_| index)
            })
            .collect::<Vec<_>>();
        let ends = timeline
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                value
                    .as_ref()
                    .filter(|entry| {
                        entry.end_micros.abs_diff(end) <= BOUNDARY_TOLERANCE_MICROS as u64
                    })
                    .map(|_| index)
            })
            .collect::<Vec<_>>();
        if starts.len() != 1 || ends.len() != 1 {
            return Err(unsupported(
                "SCTE-35 splice boundaries do not align to exact media-segment boundaries",
            ));
        }
        let first = starts[0];
        let last = ends[0];
        let first_component = timeline[first]
            .as_ref()
            .ok_or_else(|| unsupported("SCTE-35 start has no program-time mapping"))?
            .component;
        let last_component = timeline[last]
            .as_ref()
            .ok_or_else(|| unsupported("SCTE-35 end has no program-time mapping"))?
            .component;
        if first > last || first_component != last_component {
            return Err(unsupported(
                "SCTE-35 splice interval is reversed or crosses a discontinuity",
            ));
        }
        mapped.push((id.as_str(), first, last));
    }

    mapped.sort_unstable_by_key(|(_, first, last)| (*first, *last));
    for pair in mapped.windows(2) {
        let (_, previous_start, previous_end) = pair[0];
        let (_, next_start, next_end) = pair[1];
        if next_start <= previous_end
            && (next_start != previous_start || next_end != previous_end)
        {
            return Err(unsupported(
                "SCTE-35 DATERANGE ad intervals overlap ambiguously",
            ));
        }
    }

    let mut dispositions = vec![AdSegmentDisposition::Keep; playlist.segments.len()];
    for (_, first, last) in &mapped {
        for disposition in &mut dispositions[*first..=*last] {
            *disposition = AdSegmentDisposition::Remove;
        }
    }
    for range in ranges.values() {
        for &segment_index in &range.attached_segments {
            if dispositions[segment_index] == AdSegmentDisposition::Keep {
                dispositions[segment_index] = AdSegmentDisposition::KeepAndStripMarkers;
            }
        }
    }

    Ok(Some(AdRemovalPlan {
        segments: dispositions,
    }))
}

fn segment_timeline(
    playlist: &MediaPlaylist,
) -> Result<Vec<Option<SegmentTimeline>>, ManifestError> {
    let mut component_ids = vec![0_usize; playlist.segments.len()];
    let mut offsets = vec![0_i64; playlist.segments.len()];
    let mut durations = Vec::with_capacity(playlist.segments.len());
    let mut component = 0_usize;
    let mut offset = 0_i64;

    for (index, segment) in playlist.segments.iter().enumerate() {
        if index > 0 {
            if segment.discontinuity {
                component = component
                    .checked_add(1)
                    .ok_or(ManifestError::DurationOverflow)?;
                offset = 0;
            } else {
                offset = offset
                    .checked_add(durations[index - 1])
                    .ok_or(ManifestError::DurationOverflow)?;
            }
        }
        component_ids[index] = component;
        offsets[index] = offset;
        durations.push(seconds_to_micros(f64::from(segment.duration))?);
    }

    let mut bases = HashMap::<usize, i64>::new();
    for (index, segment) in playlist.segments.iter().enumerate() {
        let Some(program_date_time) = segment.program_date_time.as_ref() else {
            continue;
        };
        let base = program_date_time
            .timestamp_micros()
            .checked_sub(offsets[index])
            .ok_or(ManifestError::DurationOverflow)?;
        let component = component_ids[index];
        if let Some(previous) = bases.get(&component) {
            if previous.abs_diff(base) > BOUNDARY_TOLERANCE_MICROS as u64 {
                return Err(unsupported(
                    "PROGRAM-DATE-TIME anchors disagree with segment durations",
                ));
            }
        } else {
            bases.insert(component, base);
        }
    }

    let mut timeline = Vec::with_capacity(playlist.segments.len());
    for index in 0..playlist.segments.len() {
        let Some(base) = bases.get(&component_ids[index]) else {
            timeline.push(None);
            continue;
        };
        let start_micros = base
            .checked_add(offsets[index])
            .ok_or(ManifestError::DurationOverflow)?;
        let end_micros = start_micros
            .checked_add(durations[index])
            .ok_or(ManifestError::DurationOverflow)?;
        timeline.push(Some(SegmentTimeline {
            component: component_ids[index],
            start_micros,
            end_micros,
        }));
    }
    Ok(timeline)
}

fn attribute<'a>(range: &'a DateRange, name: &str) -> Option<&'a str> {
    range.other_attributes.as_ref()?.get(name).map(|value| match value {
        QuotedOrUnquoted::Quoted(value) | QuotedOrUnquoted::Unquoted(value) => value.as_str(),
    })
}

/// Parse and classify an SCTE-35 splice section. The parser verifies the
/// table ID, section length, command structure, and MPEG-2 CRC before any
/// ad decision is made. Only clear program-mode splice_insert commands are
/// accepted; time_signal and unknown/encrypted commands conservatively fall
/// back until their segmentation descriptors can be mapped explicitly.
fn validate_scte35_signal(
    value: &str,
    expected_out_of_network: bool,
) -> Result<SpliceSignalIdentity, ManifestError> {
    let bytes = decode_scte35_hex(value)?;
    if bytes.len() < 3 {
        return Err(unsupported("SCTE35 signal is shorter than a section header"));
    }
    let section_length =
        usize::from((u16::from(bytes[1] & 0x0F) << 8) | u16::from(bytes[2]));
    if bytes.len() != section_length + 3 {
        return Err(unsupported("SCTE35 signal has an inconsistent section length"));
    }

    let section = SpliceInfoSection::parse(&bytes)
        .map_err(|_| unsupported("SCTE35 signal failed structural or CRC validation"))?;
    if section.protocol_version != 0 || section.encrypted_packet {
        return Err(unsupported(
            "SCTE35 signal must be a clear section using protocol version zero",
        ));
    }
    let Some(clear) = section.clear.as_ref() else {
        return Err(unsupported("SCTE35 signal has no clear splice command"));
    };
    match &clear.command {
        AnyCommand::SpliceInsert(splice)
            if !splice.splice_event_cancel_indicator
                && splice.program_splice_flag
                && splice.out_of_network_indicator == expected_out_of_network =>
        {
            Ok(SpliceSignalIdentity {
                event_id: splice.splice_event_id,
                unique_program_id: splice.unique_program_id,
            })
        }
        AnyCommand::SpliceInsert(_) => Err(unsupported(
            "SCTE35 splice_insert direction or splice mode does not match its DATERANGE attribute",
        )),
        _ => Err(unsupported(
            "only clear program-mode SCTE35 splice_insert commands are supported",
        )),
    }
}

fn decode_scte35_hex(value: &str) -> Result<Vec<u8>, ManifestError> {
    let encoded = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if encoded.len() < 2 || encoded.len() > 8_196 || encoded.len() % 2 != 0 {
        return Err(unsupported("SCTE35 signal has an invalid hexadecimal length"));
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| unsupported("SCTE35 signal is not hexadecimal"))?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| unsupported("SCTE35 signal is not hexadecimal"))?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn seconds_to_micros(seconds: f64) -> Result<i64, ManifestError> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(unsupported(
            "DATERANGE or media segment duration is not positive and finite",
        ));
    }
    let micros = seconds * 1_000_000.0;
    if !micros.is_finite() || micros > i64::MAX as f64 {
        return Err(ManifestError::DurationOverflow);
    }
    Ok(micros.round() as i64)
}

fn merge_same(
    target: &mut Option<i64>,
    value: i64,
    name: &str,
) -> Result<(), ManifestError> {
    if target.is_some_and(|current| current != value) {
        return Err(unsupported(&format!("DATERANGE {name} changes for one ID")));
    }
    *target = Some(value);
    Ok(())
}

fn merge_identity(
    target: &mut Option<SpliceSignalIdentity>,
    value: SpliceSignalIdentity,
    name: &str,
) -> Result<(), ManifestError> {
    if target.is_some_and(|current| current != value) {
        return Err(unsupported(&format!(
            "DATERANGE {name} identifies multiple splice events"
        )));
    }
    *target = Some(value);
    Ok(())
}

fn merge_same_string(
    target: &mut Option<String>,
    value: &str,
    name: &str,
) -> Result<(), ManifestError> {
    if target.as_deref().is_some_and(|current| current != value) {
        return Err(unsupported(&format!("DATERANGE {name} changes for one ID")));
    }
    *target = Some(value.to_string());
    Ok(())
}

fn unsupported(reason: &str) -> ManifestError {
    ManifestError::UnsupportedAdInsertion(reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{HlsResourceKind, plan_hls_media_playlist};

    const BASE_URL: &str = "https://cdn.example.test/path/media.m3u8";

    fn playlist(body: &str) -> Result<super::super::HlsPlan, ManifestError> {
        plan_hls_media_playlist(BASE_URL, body.as_bytes())
    }

    fn scte35_signal(out_of_network: bool) -> String {
        scte35_signal_with_event(out_of_network, 7)
    }

    fn scte35_signal_with_event(out_of_network: bool, event_id: u32) -> String {
        use broadcast_common::Serialize;

        let splice = SpliceInsert {
            splice_event_id: event_id,
            out_of_network_indicator: out_of_network,
            splice_immediate_flag: true,
            ..SpliceInsert::default()
        };
        let section =
            SpliceInfoSection::new_clear(AnyCommand::SpliceInsert(splice), &[]);
        let bytes = section.try_to_bytes().expect("serialize SCTE35 test section");
        let mut encoded = String::from("0x");
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02X}").expect("write hex");
        }
        encoded
    }

    fn date_range_start(out_signal: &str) -> String {
        format!(
            "#EXT-X-DATERANGE:ID=\"ad-break-1\",START-DATE=\"2026-01-01T00:00:04Z\",SCTE35-OUT={out_signal}\n"
        )
    }

    fn date_range_end(in_signal: &str, end_second: u8, duration: u8) -> String {
        format!(
            "#EXT-X-DATERANGE:ID=\"ad-break-1\",START-DATE=\"2026-01-01T00:00:04Z\",END-DATE=\"2026-01-01T00:00:{end_second:02}Z\",DURATION={duration},SCTE35-IN={in_signal}\n"
        )
    }

    #[test]
    fn removes_only_segments_inside_a_pdt_aligned_scte35_splice() {
        let out_signal = scte35_signal(true);
        let in_signal = scte35_signal(false);
        let body = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXTINF:4,\nprogram-before.ts\n{}#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:04Z\n#EXTINF:4,\nad-1.ts\n#EXTINF:4,\nad-2.ts\n{}#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:12Z\n#EXTINF:4,\nprogram-after.ts\n#EXT-X-ENDLIST\n",
            date_range_start(&out_signal),
            date_range_end(&in_signal, 12, 8)
        );
        let plan = playlist(&body).expect("complete aligned SCTE-35 range should be removed");
        let segment_urls = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == HlsResourceKind::Segment)
            .map(|resource| resource.url.path().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            segment_urls,
            ["/path/program-before.ts", "/path/program-after.ts"]
        );
        let local = String::from_utf8(plan.local_playlist_bytes().expect("local playlist"))
            .expect("UTF-8");
        assert!(!local.contains("ad-break-1"));
        assert!(!local.contains("ad-1.ts"));
        assert!(!local.contains("ad-2.ts"));
    }

    #[test]
    fn preserves_non_ad_daterange_metadata() {
        let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXT-X-DATERANGE:ID=\"chapter-1\",CLASS=\"chapter\",START-DATE=\"2026-01-01T00:00:00Z\",DURATION=4\n#EXTINF:4,\nchapter.ts\n#EXT-X-ENDLIST\n";
        let plan = playlist(body).expect("non-ad DATERANGE is metadata");
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == HlsResourceKind::Segment)
                .count(),
            1
        );
        let local = String::from_utf8(plan.local_playlist_bytes().expect("local playlist"))
            .expect("UTF-8");
        assert!(local.contains("#EXT-X-DATERANGE:"));
    }

    #[test]
    fn requires_exact_boundaries_and_an_actual_splice_end() {
        let out_signal = scte35_signal(true);
        let in_signal = scte35_signal(false);
        let unaligned = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXTINF:4,\nprogram.ts\n{}#EXTINF:4,\nad.ts\n{}#EXTINF:4,\nafter.ts\n#EXT-X-ENDLIST\n",
            date_range_start(&out_signal),
            date_range_end(&in_signal, 10, 6)
        );
        assert!(matches!(
            playlist(&unaligned),
            Err(ManifestError::UnsupportedAdInsertion(_))
        ));
    }

    #[test]
    fn live_and_orphaned_scte35_ranges_fall_back() {
        let out_signal = scte35_signal(true);
        let in_signal = scte35_signal(false);
        let live = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n{}#EXTINF:4,\nad.ts\n",
            date_range_start(&out_signal)
        );
        assert!(matches!(
            playlist(&live),
            Err(ManifestError::UnsupportedAdInsertion(_))
        ));

        let orphan = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXT-X-DATERANGE:ID=\"ad-break-1\",START-DATE=\"2026-01-01T00:00:00Z\",SCTE35-IN={}\n#EXTINF:4,\nad.ts\n#EXT-X-ENDLIST\n",
            in_signal
        );
        assert!(matches!(
            playlist(orphan),
            Err(ManifestError::UnsupportedAdInsertion(_))
        ));
    }

    #[test]
    fn rejects_invalid_scte35_crc_and_wrong_splice_direction() {
        let valid_out = scte35_signal(true);
        let mut corrupt = valid_out.clone().into_bytes();
        let last = corrupt.len() - 1;
        corrupt[last] = if corrupt[last] == b'0' { b'1' } else { b'0' };
        let corrupt = String::from_utf8(corrupt).expect("ASCII hex");
        let invalid = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXT-X-DATERANGE:ID=\"a\",START-DATE=\"2026-01-01T00:00:00Z\",SCTE35-OUT={corrupt}\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n"
        );
        assert!(matches!(
            playlist(&invalid),
            Err(ManifestError::UnsupportedAdInsertion(_))
        ));

        let wrong_direction = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXT-X-DATERANGE:ID=\"a\",START-DATE=\"2026-01-01T00:00:00Z\",SCTE35-OUT={}\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n",
            scte35_signal(false)
        );
        assert!(matches!(
            playlist(&wrong_direction),
            Err(ManifestError::UnsupportedAdInsertion(_))
        ));

        let mismatched_pair = format!(
            "#EXTM3U\\n#EXT-X-TARGETDURATION:4\\n#EXT-X-PLAYLIST-TYPE:VOD\\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\\n#EXTINF:4,\\nprogram.ts\\n{}#EXTINF:4,\\nad.ts\\n{}#EXTINF:4,\\nafter.ts\\n#EXT-X-ENDLIST\\n",
            date_range_start(&scte35_signal_with_event(true, 7)),
            date_range_end(&scte35_signal_with_event(false, 8), 12, 8)
        );
        assert!(matches!(
            playlist(&mismatched_pair),
            Err(ManifestError::UnsupportedAdInsertion(_))
        ));
    }

    #[test]
    fn refuses_multiple_daterange_tags_on_a_single_segment() {
        let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXT-X-DATERANGE:ID=\"a\",START-DATE=\"2026-01-01T00:00:00Z\"\n#EXT-X-DATERANGE:ID=\"b\",START-DATE=\"2026-01-01T00:00:00Z\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(matches!(
            playlist(body),
            Err(ManifestError::UnsupportedAdInsertion(_))
        ));
    }
}
