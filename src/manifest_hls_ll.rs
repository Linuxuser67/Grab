//! Low-Latency HLS playlist parsing and part-resource planning.
//!
//! Grab owns fetching, staging, ad policy, and recovery. This module uses the
//! broadcast-hls parser only for the typed RFC 8216bis playlist representation.

use super::*;
use broadcast_hls::{MediaPlaylist as LlMediaPlaylist, PartSpec as LlPartSpec};
use m3u8_rs::{Map as LocalMap, MediaSegment as LocalSegment};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HlsLiveUnitId {
    Segment(u64),
    Part {
        media_sequence: u64,
        part_index: u32,
    },
}

impl HlsLiveUnitId {
    fn is_successor_of(&self, previous: &Self) -> bool {
        match (self, previous) {
            (Self::Segment(current), Self::Segment(previous))
            | (
                Self::Part {
                    media_sequence: current,
                    part_index: 0,
                },
                Self::Segment(previous),
            )
            | (
                Self::Segment(current),
                Self::Part {
                    media_sequence: previous,
                    ..
                },
            )
            | (
                Self::Part {
                    media_sequence: current,
                    part_index: 0,
                },
                Self::Part {
                    media_sequence: previous,
                    ..
                },
            ) if previous.checked_add(1) == Some(*current) => true,
            (
                Self::Part {
                    media_sequence: current_sequence,
                    part_index: current_index,
                },
                Self::Part {
                    media_sequence: previous_sequence,
                    part_index: previous_index,
                },
            ) if current_sequence == previous_sequence => {
                previous_index.checked_add(1) == Some(*current_index)
            }
            _ => false,
        }
    }

    pub(crate) fn from_staged_name(filename: &str) -> Option<Self> {
        let stem = filename.strip_prefix("segment-ll-")?.strip_suffix(".bin")?;
        let mut fields = stem.split('-');
        let media_sequence = fields.next()?.parse().ok()?;
        let id = match fields.next()? {
            "full" => Self::Segment(media_sequence),
            "part" => Self::Part {
                media_sequence,
                part_index: fields.next()?.parse().ok()?,
            },
            _ => return None,
        };
        let fingerprint = fields.next()?;
        if fields.next().is_some()
            || fingerprint.len() != 32
            || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        Some(id)
    }
}

#[derive(Debug, Default)]
pub(crate) struct HlsLiveIndex {
    positions: HashMap<HlsLiveUnitId, usize>,
    part_runs: HashMap<u64, HlsPartRun>,
}

#[derive(Debug)]
struct HlsPartRun {
    last_index: usize,
    first_part_index: u32,
    next_part_index: Option<u32>,
    duration: f64,
    first_discontinuity: bool,
    map: Option<LocalMap>,
    contiguous: bool,
}

impl HlsLiveIndex {
    pub(crate) fn from_units(ids: &[HlsLiveUnitId], segments: &[LocalSegment]) -> Self {
        let mut index = Self::default();
        index.extend(ids, segments, 0);
        index
    }

    pub(crate) fn extend(
        &mut self,
        ids: &[HlsLiveUnitId],
        segments: &[LocalSegment],
        first_index: usize,
    ) {
        for (offset, (id, segment)) in ids.iter().zip(segments).enumerate() {
            let index = first_index + offset;
            self.positions.insert(id.clone(), index);
            let HlsLiveUnitId::Part {
                media_sequence,
                part_index,
            } = id
            else {
                continue;
            };

            match self.part_runs.entry(*media_sequence) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(HlsPartRun {
                        last_index: index,
                        first_part_index: *part_index,
                        next_part_index: part_index.checked_add(1),
                        duration: f64::from(segment.duration),
                        first_discontinuity: segment.discontinuity,
                        map: segment.map.clone(),
                        contiguous: *part_index == 0
                            && segment.duration.is_finite()
                            && segment.duration > 0.0,
                    });
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let run = entry.get_mut();
                    if run.last_index.checked_add(1) != Some(index)
                        || run.next_part_index != Some(*part_index)
                        || run.map != segment.map
                        || !segment.duration.is_finite()
                        || segment.duration <= 0.0
                    {
                        run.contiguous = false;
                    }
                    run.last_index = index;
                    run.next_part_index = part_index.checked_add(1);
                    run.duration += f64::from(segment.duration);
                }
            }
        }
    }

    pub(crate) fn position(&self, id: &HlsLiveUnitId) -> Option<usize> {
        self.positions.get(id).copied()
    }

    pub(crate) fn completed_parent_alias(
        &self,
        id: &HlsLiveUnitId,
        parent: &LocalSegment,
    ) -> Option<usize> {
        let HlsLiveUnitId::Segment(sequence) = id else {
            return None;
        };
        let run = self.part_runs.get(sequence)?;
        let duration_matches =
            (run.duration - f64::from(parent.duration)).abs() <= PART_DURATION_TOLERANCE;
        (run.contiguous
            && run.first_part_index == 0
            && run.first_discontinuity == parent.discontinuity
            && run.map == parent.map
            && duration_matches)
            .then_some(run.last_index)
    }
}

pub(crate) fn validate_live_unit_ids(ids: &[HlsLiveUnitId]) -> bool {
    // A sliding live window may begin midway through a partial segment.
    // Validate continuity within the visible window, not the unseen prefix.
    ids.windows(2).all(|pair| pair[1].is_successor_of(&pair[0]))
}

fn has_ll_hls_partial_segments(body: &[u8]) -> bool {
    body.split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"#EXT-X-PART:"))
}

fn trim_ascii_whitespace(value: &[u8]) -> &[u8] {
    value.trim_ascii()
}

const PART_DURATION_TOLERANCE: f64 = 0.02;

#[expect(
    clippy::cast_precision_loss,
    reason = "HLS target durations are integer seconds compared with fractional durations"
)]
fn target_duration_seconds(seconds: u64) -> f64 {
    seconds as f64
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "m3u8-rs stores segment durations as f32; validation is performed in f64 first"
)]
fn media_segment_duration(duration: Duration) -> f32 {
    duration.as_secs_f64() as f32
}

fn has_required_unknown_attributes<T>(attributes: &[(String, T)]) -> bool {
    attributes.iter().any(|(name, _)| name.starts_with("REQ-"))
}

fn hls_key_method(attributes: &[u8]) -> Option<&[u8]> {
    let mut attribute_start = 0;
    let mut inside_quotes = false;

    for index in 0..=attributes.len() {
        let at_end = index == attributes.len();
        if at_end || (attributes[index] == b',' && !inside_quotes) {
            let attribute = trim_ascii_whitespace(&attributes[attribute_start..index]);
            if let Some(separator) = attribute.iter().position(|byte| *byte == b'=')
                && trim_ascii_whitespace(&attribute[..separator]).eq_ignore_ascii_case(b"METHOD")
            {
                return Some(trim_ascii_whitespace(&attribute[separator + 1..]));
            }
            attribute_start = index.saturating_add(1);
        } else if attributes[index] == b'"' {
            inside_quotes = !inside_quotes;
        }
    }

    None
}

fn has_hls_encrypted_media_key_tag(body: &[u8]) -> bool {
    body.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(attributes) = line.strip_prefix(b"#EXT-X-KEY:") else {
            return false;
        };

        // Missing or malformed METHOD values fail closed. METHOD=NONE only
        // disables encryption; it must not divert an unencrypted LL-HLS
        // playlist away from the low-latency planner.
        hls_key_method(attributes).is_none_or(|method| !method.eq_ignore_ascii_case(b"NONE"))
    })
}

fn is_hls_method_none_key_tag(tag: &str) -> bool {
    let Some(attributes) = tag.as_bytes().strip_prefix(b"#EXT-X-KEY:") else {
        return false;
    };
    // RFC 8216 permits no attributes other than METHOD when its value is
    // NONE. Reject malformed or extended forms instead of silently ignoring
    // an unexpected key declaration.
    !attributes.contains(&b',')
        && hls_key_method(attributes).is_some_and(|method| method.eq_ignore_ascii_case(b"NONE"))
}

fn has_ll_hls_delta_update(body: &[u8]) -> bool {
    body.split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"#EXT-X-SKIP:"))
}

pub(crate) fn has_ll_hls_tags(body: &[u8]) -> bool {
    body.split(|byte| *byte == b'\n').any(|line| {
        line.starts_with(b"#EXT-X-SERVER-CONTROL:")
            || line.starts_with(b"#EXT-X-PART-INF:")
            || line.starts_with(b"#EXT-X-PART:")
            || line.starts_with(b"#EXT-X-PRELOAD-HINT:")
            || line.starts_with(b"#EXT-X-RENDITION-REPORT:")
            || line.starts_with(b"#EXT-X-SKIP:")
    })
}

pub(crate) fn plan_hls_media_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<Option<HlsPlan>, ManifestError> {
    let has_partial_segments = has_ll_hls_partial_segments(body);
    let use_standard_encryption_planner = !has_partial_segments
        && has_hls_encrypted_media_key_tag(body)
        && !has_ll_hls_delta_update(body);
    if !has_ll_hls_tags(body) || use_standard_encryption_planner {
        // Keep complete-segment LL-HLS on the live planner unless it actually
        // carries media encryption. Only complete encrypted segments are
        // delegated to the ordinary HLS planner, which already owns key
        // fetching, decryption metadata, and output encryption signaling.
        // Partial segments and delta updates stay on the guarded LL-HLS path.
        return Ok(None);
    }
    let text = std::str::from_utf8(body)
        .map_err(|error| ManifestError::Parse(format!("LL-HLS playlist is not UTF-8: {error}")))?;
    let playlist = LlMediaPlaylist::parse(text).map_err(|error| {
        ManifestError::Parse(format!("could not parse LL-HLS playlist: {error}"))
    })?;
    // broadcast-hls 0.4 retains segment-scoped tags verbatim. Reuse the
    // already-pinned m3u8-rs parser for RFC 3339 validation and its typed PDT
    // value when a completed parent segment carries a wall-clock anchor.
    let source_segments = if playlist.segments.iter().any(|segment| {
        segment
            .pre_tags
            .iter()
            .any(|tag| tag.starts_with("#EXT-X-PROGRAM-DATE-TIME:"))
    }) {
        let parsed = m3u8_rs::parse_media_playlist_res(body)
            .map_err(|error| hls_parse_error(body, error))?;
        if parsed.segments.len() != playlist.segments.len() {
            return Err(ManifestError::UnsupportedPlaylist);
        }
        Some(parsed.segments)
    } else {
        None
    };
    let open_segment_discontinuity = has_trailing_open_discontinuity(body);
    build_plan(
        manifest_url,
        playlist,
        open_segment_discontinuity,
        source_segments.as_deref(),
    )
    .map(Some)
}

fn parse_program_date_time_tag(tag: &str) -> Result<m3u8_rs::MediaSegment, ManifestError> {
    if tag.len() > 128 || tag.contains(['\r', '\n']) {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    let probe = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n{tag}\n#EXTINF:1,\npdt-validation-probe.ts\n#EXT-X-ENDLIST\n"
    );
    let parsed = m3u8_rs::parse_media_playlist_res(probe.as_bytes())
        .map_err(|error| hls_parse_error(probe.as_bytes(), error))?;
    let segment = parsed
        .segments
        .into_iter()
        .next()
        .ok_or(ManifestError::UnsupportedPlaylist)?;
    if segment.program_date_time.is_none() {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    Ok(segment)
}

fn open_segment_program_date_time(
    open: &broadcast_hls::OpenSegment,
) -> Result<Option<m3u8_rs::MediaSegment>, ManifestError> {
    let mut date_tags = open
        .pre_tags
        .iter()
        .filter(|tag| tag.starts_with("#EXT-X-PROGRAM-DATE-TIME:"));
    let Some(tag) = date_tags.next() else {
        return Ok(None);
    };
    if date_tags.next().is_some() {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    parse_program_date_time_tag(tag).map(Some)
}

fn has_trailing_open_discontinuity(body: &[u8]) -> bool {
    const EXTINF: &[u8] = b"#EXTINF:";
    let tail_start = body
        .windows(EXTINF.len())
        .rposition(|window| window == EXTINF)
        .unwrap_or(0);
    body[tail_start..]
        .split(|byte| *byte == b'\n')
        .any(|raw_line| {
            raw_line
                .strip_suffix(b"\r")
                .unwrap_or(raw_line)
                .trim_ascii_start()
                == b"#EXT-X-DISCONTINUITY"
        })
}

fn build_plan(
    manifest_url: &str,
    playlist: LlMediaPlaylist,
    open_segment_discontinuity: bool,
    source_segments: Option<&[m3u8_rs::MediaSegment]>,
) -> Result<HlsPlan, ManifestError> {
    let base_url = parse_manifest_url(manifest_url)?;
    let source_target_duration = u64::from(playlist.target_duration);
    if source_target_duration == 0 || playlist.iframes_only {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    // VOD is immutable even when an origin omits the redundant ENDLIST marker.
    // Keep LL-HLS VOD out of the live reload path, as with the standard planner.
    let declared_vod = matches!(
        playlist.playlist_type.as_ref(),
        Some(broadcast_hls::PlaylistType::Vod)
    );
    let end_list = playlist.endlist || declared_vod;
    let skipped_segments = playlist
        .skip
        .as_ref()
        .map_or(0, |skip| skip.skipped_segments);
    if let Some(skip) = playlist.skip.as_ref()
        && (low_latency_can_skip_until(&playlist).is_none()
            || !skip.recently_removed_daterange_ids.is_empty()
            || has_required_unknown_attributes(&skip.extra_attrs))
    {
        return Err(ManifestError::UnsupportedPlaylistFeature(
            "#EXT-X-SKIP metadata cannot be reconstructed safely".to_string(),
        ));
    }
    if !playlist.defines.is_empty() {
        return Err(ManifestError::UnsupportedPlaylistFeature(
            "#EXT-X-DEFINE variables must be expanded before LL-HLS planning".to_string(),
        ));
    }
    let low_latency = playlist
        .low_latency
        .as_ref()
        .ok_or(ManifestError::UnsupportedPlaylist)?;
    if has_required_unknown_attributes(&low_latency.sc_extra_attrs)
        || has_required_unknown_attributes(&low_latency.pi_extra_attrs)
        || has_required_unknown_attributes(&low_latency.ph_extra_attrs)
    {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    let part_target = low_latency
        .part_target
        .map(|target| {
            let seconds = target.get();
            if !seconds.is_finite()
                || seconds <= 0.0
                || seconds >= target_duration_seconds(source_target_duration)
            {
                return Err(ManifestError::InvalidDuration);
            }
            Duration::try_from_secs_f64(seconds).map_err(|_| ManifestError::InvalidDuration)
        })
        .transpose()?;
    let can_skip_until = low_latency
        .can_skip_until
        .map(|duration| {
            let seconds = duration.get();
            if !seconds.is_finite() || seconds <= 0.0 {
                return Err(ManifestError::InvalidDuration);
            }
            Duration::try_from_secs_f64(seconds).map_err(|_| ManifestError::InvalidDuration)
        })
        .transpose()?;
    if let Some(part_target) = part_target {
        let part_hold_back = low_latency
            .part_hold_back
            .ok_or(ManifestError::InvalidDuration)?
            .get();
        // The HLS 2nd Edition draft sets a two-Part-Target minimum and
        // recommends three; Apple's device authoring profile requires three.
        // Grab accepts draft-compliant LL-HLS servers, not only Apple-profile
        // streams.
        if !part_hold_back.is_finite() || part_hold_back < 2.0 * part_target.as_secs_f64() {
            return Err(ManifestError::InvalidDuration);
        }
    }
    if let Some(hold_back) = low_latency.hold_back {
        let seconds = hold_back.get();
        if !seconds.is_finite() || seconds < 3.0 * target_duration_seconds(source_target_duration) {
            return Err(ManifestError::InvalidDuration);
        }
    }
    if can_skip_until.is_some_and(|skip| {
        skip.as_secs_f64() < 6.0 * target_duration_seconds(source_target_duration)
    }) {
        return Err(ManifestError::InvalidDuration);
    }
    validate_start_point(&playlist)?;
    // EXT-X-START is a playback-position hint, not a resource-selection
    // directive. A recording retains every unit in the advertised live window.
    let has_parts = playlist
        .segments
        .iter()
        .any(|segment| !segment.parts.is_empty())
        || playlist
            .open_segment
            .as_ref()
            .is_some_and(|segment| !segment.parts.is_empty());
    if has_parts && part_target.is_none() {
        return Err(ManifestError::UnsupportedPlaylist);
    }

    validate_playlist_tags(&playlist)?;
    if end_list && playlist.open_segment.is_some() {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    if playlist.segments.is_empty()
        && !playlist
            .open_segment
            .as_ref()
            .is_some_and(|segment| !segment.parts.is_empty())
    {
        return Err(ManifestError::EmptyPlaylist);
    }

    let mut plan = PlanAcc::default();
    let mut map_filenames: HashMap<(String, Option<ManifestByteRange>), String> = HashMap::new();
    let mut active_map: Option<broadcast_hls::MapTag> = None;
    let first_visible_sequence = playlist
        .media_sequence
        .checked_add(skipped_segments)
        .ok_or(ManifestError::DurationOverflow)?;

    let mut pending_discontinuity = false;
    for (segment_index, segment) in playlist.segments.iter().enumerate() {
        let program_date_time = source_segments
            .and_then(|segments| segments.get(segment_index))
            .and_then(|source| source.program_date_time);
        let offset = u64::try_from(segment_index).map_err(|_| ManifestError::DurationOverflow)?;
        let media_sequence = first_visible_sequence
            .checked_add(offset)
            .ok_or(ManifestError::DurationOverflow)?;
        if let Some(map) = segment.map.as_ref() {
            active_map = Some(map.clone());
        }
        if segment.gap {
            // A completed presentation can omit a declared gap while retaining
            // the source sequence and timestamp progression. A live snapshot
            // needs a persistent gap unit in the recovery journal, which the
            // current writer cannot represent safely.
            if !end_list || !segment.parts.is_empty() {
                return Err(ManifestError::UnsupportedPlaylistFeature(
                    "#EXT-X-GAP in a live or partial LL-HLS segment cannot be reconciled safely"
                        .to_string(),
                ));
            }
            checked_duration(segment.duration.get(), source_target_duration)?;
            let url = resolve_hls_media_resource_url(&base_url, &segment.uri)?;
            plan_range(segment.byte_range.as_ref(), &url, &mut plan.previous_range)?;
            pending_discontinuity |= segment.discontinuous;
            continue;
        }
        let discontinuity = segment.discontinuous || pending_discontinuity;
        pending_discontinuity = false;
        let map = plan_map(
            &base_url,
            active_map.as_ref(),
            &mut plan.resources,
            &mut map_filenames,
        )?;
        if segment.parts.is_empty() {
            let duration = checked_duration(segment.duration.get(), source_target_duration)?;
            let url = resolve_hls_media_resource_url(&base_url, &segment.uri)?;
            let range = plan_range(segment.byte_range.as_ref(), &url, &mut previous_range)?;
            let filename = ll_resource_filename(media_sequence, None, &url, range);
            plan.resources.push(HlsResource {
                url,
                range,
                filename: filename.clone(),
                kind: HlsResourceKind::Segment,
            });
            plan.local_segments.push(LocalSegment {
                uri: filename,
                duration: media_segment_duration(duration),
                discontinuity,
                map,
                program_date_time,
                ..Default::default()
            });
            plan.unit_ids.push(HlsLiveUnitId::Segment(media_sequence));
            continue;
        }

        let parent_duration = checked_duration(segment.duration.get(), source_target_duration)?;
        let parts_duration = segment.parts.iter().try_fold(0.0_f64, |total, part| {
            let duration = part.duration.get();
            if !duration.is_finite() || duration <= 0.0 {
                return Err(ManifestError::InvalidDuration);
            }
            Ok(total + duration)
        })?;
        if (parts_duration - parent_duration.as_secs_f64()).abs() > PART_DURATION_TOLERANCE {
            return Err(ManifestError::LivePartDurationMismatch);
        }
        let first_part_position = plan.plan.local_segments.len();
        plan.append_parts(
            &PartPlanContext {
                base_url: &base_url,
                media_sequence,
                source_target_duration,
                map,
                discontinuity,
                independent_segments: playlist.independent_segments,
            },
            &segment.parts,
        )?;
        if let Some(program_date_time) = program_date_time {
            plan.local_segments[first_part_position].program_date_time = Some(program_date_time);
        }
    }

    if let Some(open) = playlist.open_segment.as_ref() {
        let offset =
            u64::try_from(playlist.segments.len()).map_err(|_| ManifestError::DurationOverflow)?;
        let media_sequence = first_visible_sequence
            .checked_add(offset)
            .ok_or(ManifestError::DurationOverflow)?;
        if let Some(map) = open.map.as_ref() {
            active_map = Some(map.clone());
        }
        let map = plan_map(
            &base_url,
            active_map.as_ref(),
            &mut resources,
            &mut map_filenames,
        )?;
        let discontinuity = open_segment_discontinuity;
        let open_date_time =
            open_segment_program_date_time(open)?.and_then(|segment| segment.program_date_time);
        let first_part_position = plan.plan.local_segments.len();
        plan.append_parts(
            &PartPlanContext {
                base_url: &base_url,
                media_sequence,
                source_target_duration,
                map,
                discontinuity,
                independent_segments: playlist.independent_segments,
            },
            &open.parts,
        )?;
        if let Some(program_date_time) = open_date_time {
            plan.local_segments[first_part_position].program_date_time = Some(program_date_time);
        }
    }

    if plan.local_segments.is_empty()
        || plan.local_segments.len() > MAX_HLS_LIVE_UNITS
        || (!end_list && !validate_live_unit_ids(&unit_ids))
        || plan.local_segments
            .iter()
            .any(|segment| !segment.duration.is_finite() || segment.duration <= 0.0)
    {
        return Err(ManifestError::UnsupportedPlaylist);
    }

    let blocking_reload = if !end_list && low_latency.can_block_reload {
        let next_msn = first_visible_sequence
            .checked_add(
                u64::try_from(playlist.segments.len())
                    .map_err(|_| ManifestError::DurationOverflow)?,
            )
            .ok_or(ManifestError::DurationOverflow)?;
        let next_part = playlist
            .open_segment
            .as_ref()
            .map(|segment| {
                u64::try_from(segment.parts.len()).map_err(|_| ManifestError::DurationOverflow)
            })
            .transpose()?;
        Some((next_msn, next_part))
    } else {
        None
    };

    let mut local_playlist = MediaPlaylist {
        version: None,
        target_duration: source_target_duration,
        media_sequence: 0,
        discontinuity_sequence: playlist.discontinuity_sequence,
        independent_segments: false,
        segments: plan.local_segments,
        end_list: true,
        playlist_type: Some(MediaPlaylistType::Vod),
        ..Default::default()
    };
    local_playlist.version = Some(minimum_hls_playlist_version(&local_playlist));

    let rendition_reports = parse_rendition_reports(&base_url, &playlist.rendition_reports)?;

    Ok(HlsPlan {
        resources: plan.resources.into_boxed_slice(),
        requested_playlist_url: base_url.clone(),
        playlist_url: base_url,
        is_live: !end_list,
        end_list,
        source_target_duration,
        local_playlist,
        ll_unit_ids: Some(plan.unit_ids.into_boxed_slice()),
        part_target,
        can_skip_until,
        blocking_reload,
        ll_first_part_independent: plan.first_part_independent,
        rendition_reports,
        inherited_variables: Vec::new().into_boxed_slice(),
    })
}

#[derive(Clone, Debug)]
pub(crate) struct HlsRenditionReport {
    pub(crate) playlist_url: Url,
    pub(crate) last_msn: u64,
    pub(crate) last_part: Option<u64>,
}

impl HlsRenditionReport {
    /// The first media position the peer playlist should be asked to expose.
    /// LAST-PART identifies the last available partial segment, so request its
    /// successor; otherwise request the next media sequence.
    pub(crate) fn next_reload(&self) -> Option<(u64, Option<u64>)> {
        match self.last_part {
            Some(part) => Some((self.last_msn, Some(part.checked_add(1)?))),
            None => Some((self.last_msn.checked_add(1)?, None)),
        }
    }
}

#[derive(Default)]
struct PlanAcc {
    resources: Vec<HlsResource>,
    previous_range: Option<(String, u64)>,
    local_segments: Vec<LocalSegment>,
    unit_ids: Vec<HlsLiveUnitId>,
    first_part_independent: Option<bool>,
}

struct PartPlanContext<'a> {
    base_url: &'a Url,
    media_sequence: u64,
    source_target_duration: u64,
    map: Option<LocalMap>,
    discontinuity: bool,
    independent_segments: bool,
}

impl PlanAcc {
    fn append_parts(
        &mut self,
        context: &PartPlanContext<'_>,
        parts: &[LlPartSpec],
    ) -> Result<(), ManifestError> {
        for (index, part) in parts.iter().enumerate() {
            let part_index = u32::try_from(index).map_err(|_| ManifestError::DurationOverflow)?;
            if self.local_segments.is_empty() {
                self.first_part_independent =
                    Some(part.independent || context.independent_segments);
            }
            validate_part(part)?;
            let duration = checked_duration(part.duration.get(), context.source_target_duration)?;
            let url = resolve_hls_media_resource_url(context.base_url, &part.uri)?;
            let range = plan_range(
                part.byte_range.as_ref(),
                &url,
                &mut self.previous_range,
            )?;
            let filename =
                ll_resource_filename(context.media_sequence, Some(part_index), &url, range);
            self.resources.push(HlsResource {
                url,
                range,
                filename: filename.clone(),
                kind: HlsResourceKind::Segment,
            });
            self.local_segments.push(LocalSegment {
                uri: filename,
                duration: media_segment_duration(duration),
                discontinuity: context.discontinuity && part_index == 0,
                map: context.map.clone(),
                ..Default::default()
            });
            self.unit_ids.push(HlsLiveUnitId::Part {
                media_sequence: context.media_sequence,
                part_index,
            });
        }
        Ok(())
    }
}

fn parse_rendition_reports(
    base_url: &Url,
    reports: &[broadcast_hls::RenditionReport],
) -> Result<Box<[HlsRenditionReport]>, ManifestError> {
    reports
        .iter()
        .map(|report| {
            if has_required_unknown_attributes(&report.extra_attrs) {
                return Err(ManifestError::UnsupportedPlaylist);
            }
            Ok(HlsRenditionReport {
                playlist_url: resolve_hls_media_resource_url(base_url, &report.uri)?,
                last_msn: report.last_msn,
                last_part: report.last_part,
            })
        })
        .collect::<Result<Vec<_>, ManifestError>>()
        .map(Vec::into_boxed_slice)
}

fn low_latency_can_skip_until(playlist: &LlMediaPlaylist) -> Option<Duration> {
    let seconds = playlist.low_latency.as_ref()?.can_skip_until?.get();
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(seconds).ok()
}

fn validate_playlist_tags(playlist: &LlMediaPlaylist) -> Result<(), ManifestError> {
    if playlist
        .extra_tags
        .iter()
        .any(|tag| tag.starts_with("#EXT-X-PROGRAM-DATE-TIME:"))
    {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    if playlist.segments.iter().any(|segment| {
        let mut tags = segment
            .pre_tags
            .iter()
            .filter(|tag| tag.starts_with("#EXT-X-PROGRAM-DATE-TIME:"));
        tags.next().is_some() && tags.next().is_some()
    }) {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    for tag in playlist.extra_tags.iter().chain(
        playlist
            .segments
            .iter()
            .flat_map(|segment| segment.pre_tags.iter())
            .chain(
                playlist
                    .open_segment
                    .iter()
                    .flat_map(|segment| segment.pre_tags.iter()),
            ),
    ) {
        if tag.starts_with("#EXT-X-KEY:") {
            if is_hls_method_none_key_tag(tag) {
                continue;
            }
            return Err(ManifestError::UnsupportedEncryption);
        }
        if tag.starts_with("#EXT-X-PROGRAM-DATE-TIME:") {
            parse_program_date_time_tag(tag)?;
            continue;
        }
        // Ad-insertion markers are inert metadata in native planning. Keep
        // DATERANGE/interstitial tags on fallback because their timing semantics
        // are not mapped to exact media units.
        if [
            "#EXT-X-CUE-OUT",
            "#EXT-X-CUE-OUT-CONT",
            "#EXT-X-CUE-IN",
            "#EXT-OATCLS-SCTE35",
            "#EXT-X-ASSET:",
        ]
        .iter()
        .any(|marker| tag.starts_with(marker))
            || tag.starts_with("#EXT-X-ALLOW-CACHE:")
            || tag.starts_with("#EXT-X-BITRATE:")
        {
            continue;
        }
        let feature = tag.split_once(':').map_or(tag.as_str(), |(name, _)| name);
        let feature = feature.chars().take(64).collect::<String>();
        return Err(ManifestError::UnsupportedPlaylistFeature(feature));
    }
    Ok(())
}

/// EXT-X-START is a playback preference, not a media-resource selection
/// instruction. Grab captures the entire advertised live window and writes a
/// new VOD playlist, so the source-only preference is intentionally omitted.
fn validate_start_point(playlist: &LlMediaPlaylist) -> Result<(), ManifestError> {
    let Some(start) = playlist.start.as_ref() else {
        return Ok(());
    };
    if !start.time_offset.get().is_finite() {
        return Err(ManifestError::InvalidDuration);
    }
    if has_required_unknown_attributes(&start.extra_attrs) {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    Ok(())
}

fn validate_part(part: &LlPartSpec) -> Result<(), ManifestError> {
    if part.gap {
        return Err(ManifestError::UnsupportedPlaylistFeature(
            "#EXT-X-PART GAP resources cannot be fetched".to_string(),
        ));
    }
    if has_required_unknown_attributes(&part.extra_attrs) {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    Ok(())
}

fn checked_duration(seconds: f64, target_duration: u64) -> Result<Duration, ManifestError> {
    if !seconds.is_finite()
        || seconds <= 0.0
        || seconds.round() > target_duration_seconds(target_duration)
    {
        return Err(ManifestError::InvalidDuration);
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| ManifestError::InvalidDuration)
}

fn ll_resource_filename(
    media_sequence: u64,
    part_index: Option<u32>,
    url: &Url,
    range: Option<ManifestByteRange>,
) -> String {
    let prefix = match part_index {
        Some(part_index) => format!("segment-ll-{media_sequence:020}-part-{part_index:06}"),
        None => format!("segment-ll-{media_sequence:020}-full"),
    };
    resource_filename(&prefix, None, url, range)
}

fn plan_range(
    byte_range: Option<&broadcast_hls::ByteRange>,
    url: &Url,
    previous_range: &mut Option<(String, u64)>,
) -> Result<Option<ManifestByteRange>, ManifestError> {
    let Some(byte_range) = byte_range else {
        *previous_range = None;
        return Ok(None);
    };
    let key = resource_cache_key(url);
    let offset = byte_range.offset.or_else(|| {
        previous_range
            .as_ref()
            .filter(|(previous_url, _)| previous_url == &key)
            .map(|(_, next_offset)| *next_offset)
    });
    let offset = offset.ok_or(ManifestError::MissingByteRangeOffset)?;
    let range = checked_byte_range(byte_range.length, offset)?;
    let next_offset = offset
        .checked_add(byte_range.length)
        .ok_or(ManifestError::InvalidByteRange)?;
    *previous_range = Some((key, next_offset));
    Ok(Some(range))
}

fn plan_map(
    base_url: &Url,
    map: Option<&broadcast_hls::MapTag>,
    resources: &mut Vec<HlsResource>,
    filenames: &mut HashMap<(String, Option<ManifestByteRange>), String>,
) -> Result<Option<LocalMap>, ManifestError> {
    let Some(map) = map else {
        return Ok(None);
    };
    if has_required_unknown_attributes(&map.extra_attrs) {
        return Err(ManifestError::UnsupportedPlaylist);
    }
    let url = resolve_hls_media_resource_url(base_url, &map.uri)?;
    let range = map
        .byte_range
        .as_ref()
        .map(|range| {
            let offset = range.offset.ok_or(ManifestError::MissingByteRangeOffset)?;
            checked_byte_range(range.length, offset)
        })
        .transpose()?;
    let cache_key = (resource_cache_key(&url), range);
    let filename = match filenames.get(&cache_key) {
        Some(filename) => filename.clone(),
        None => {
            let filename = resource_filename("init", None, &url, range);
            resources.push(HlsResource {
                url,
                range,
                filename: filename.clone(),
                kind: HlsResourceKind::InitMap,
            });
            filenames.insert(cache_key, filename.clone());
            filename
        }
    };
    Ok(Some(LocalMap {
        uri: filename,
        byte_range: None,
        other_attributes: HashMap::new(),
    }))
}

pub(super) fn prepare_live_update(
    accumulator: &HlsLiveAccumulator,
    snapshot: &HlsPlan,
) -> Result<HlsLiveUpdate, ManifestError> {
    let ids = snapshot
        .ll_unit_ids
        .as_deref()
        .ok_or(ManifestError::UnsupportedPlaylist)?;
    if ids.len() != snapshot.local_playlist.segments.len() || !validate_live_unit_ids(ids) {
        return Err(ManifestError::LiveUnitIdentityInvalid);
    }
    if accumulator.local_playlist.is_none()
        && let Some(HlsLiveUnitId::Part { part_index, .. }) = ids.first()
        && (*part_index != 0 || snapshot.ll_first_part_independent != Some(true))
    {
        return Err(ManifestError::UnsupportedPlaylistFeature(
            "the first captured LL-HLS part must be part zero and independently decodable"
                .to_string(),
        ));
    }
    if accumulator.local_playlist.is_some() && accumulator.ll_unit_ids.is_none() {
        return Err(ManifestError::LiveUnitIdentityInvalid);
    }
    if accumulator
        .target_duration
        .is_some_and(|target| target != snapshot.source_target_duration)
    {
        return Err(ManifestError::LiveTargetDurationChanged);
    }
    if accumulator
        .ll_unit_ids
        .as_ref()
        .is_some_and(|existing| existing.len() != accumulator.segment_identities.len())
    {
        return Err(ManifestError::LiveUnitIdentityInvalid);
    }

    let existing_ids = accumulator.ll_unit_ids.as_deref().unwrap_or_default();
    let existing_segments = accumulator
        .local_playlist
        .as_ref()
        .map_or(&[][..], |playlist| playlist.segments.as_slice());
    let existing_index = accumulator.ll_index.as_ref();
    if !existing_ids.is_empty() && existing_index.is_none() {
        return Err(ManifestError::LiveUnitIdentityInvalid);
    }

    // Delta and sliding-window playlists may omit the MAP line that established
    // the current initialization section. Carry the last committed map forward
    // before deriving identities and local resource references.
    let mut active_map = existing_segments
        .last()
        .and_then(|segment| segment.map.clone());
    let mut resolved_segments = Vec::with_capacity(snapshot.local_playlist.segments.len());
    for segment in &snapshot.local_playlist.segments {
        let mut resolved = segment.clone();
        if let Some(map) = resolved.map.as_ref() {
            active_map = Some(map.clone());
        } else if active_map.is_some() {
            resolved.map = active_map.clone();
        }
        resolved_segments.push(resolved);
    }

    let parent_aliases = ids
        .iter()
        .zip(&resolved_segments)
        .map(|(id, segment)| {
            existing_index.and_then(|index| index.completed_parent_alias(id, segment))
        })
        .collect::<Vec<_>>();
    let first_overlap = ids.iter().enumerate().find_map(|(index, id)| {
        existing_index
            .is_some_and(|existing| existing.position(id).is_some())
            .then_some(index)
            .or_else(|| parent_aliases[index].map(|_| index))
    });

    let mut saw_new = false;
    let mut last_existing_index = None;
    let mut new_segments = Vec::new();
    let mut new_ids = Vec::new();
    let mut referenced_resources = Vec::new();
    let mut referenced_resource_set = HashSet::new();

    // Ignore a stale prefix that predates the first segment this recording
    // retained. If the snapshot does not overlap, it must start exactly after
    // the retained tail; otherwise media would be silently skipped.
    let first_index = if existing_ids.is_empty() {
        0
    } else if let Some(index) = first_overlap {
        index
    } else {
        let first_id = ids.first().ok_or(ManifestError::LiveSequenceGap)?;
        if !first_id.is_successor_of(existing_ids.last().ok_or(ManifestError::LiveSequenceGap)?) {
            return Err(ManifestError::LiveSequenceGap);
        }
        0
    };

    for (id, segment, parent_alias) in ids
        .iter()
        .zip(&resolved_segments)
        .zip(parent_aliases.iter())
        .map(|((id, segment), alias)| (id, segment, alias))
        .skip(first_index)
    {
        if let Some(index) = existing_index.and_then(|existing| existing.position(id)) {
            if saw_new || last_existing_index.is_some_and(|previous| index <= previous) {
                return Err(ManifestError::LiveUnitOrderChanged);
            }
            if accumulator.segment_identities.get(index) != Some(&hls_segment_identity(segment)) {
                return Err(ManifestError::LiveUnitIdentityChanged);
            }
            last_existing_index = Some(index);
        } else if let Some(alias_end) = *parent_alias {
            if saw_new || last_existing_index.is_some_and(|previous| alias_end <= previous) {
                return Err(ManifestError::LiveUnitOrderChanged);
            }
            last_existing_index = Some(alias_end);
            // The complete parent URI is a replacement for the parts, not an
            // additional resource. They are already present in the recording.
            continue;
        } else {
            let predecessor = new_ids
                .last()
                .or_else(|| last_existing_index.and_then(|index| existing_ids.get(index)))
                .or_else(|| existing_ids.last());
            if !existing_ids.is_empty()
                && last_existing_index.is_some_and(|index| index + 1 != existing_ids.len())
            {
                return Err(ManifestError::LiveUnitOrderChanged);
            }
            if let Some(predecessor) = predecessor
                && !id.is_successor_of(predecessor)
            {
                return Err(ManifestError::LiveSequenceGap);
            }
            saw_new = true;
            new_ids.push(id.clone());
            new_segments.push(segment.clone());
        }

        for filename in segment_resource_filenames(segment) {
            let filename = filename.to_string();
            if referenced_resource_set.insert(filename.clone()) {
                referenced_resources.push(filename);
            }
        }
    }

    if existing_ids.is_empty() && new_segments.len() != ids.len() {
        return Err(ManifestError::LiveUnitIdentityInvalid);
    }
    if existing_ids.len().saturating_add(new_segments.len()) > MAX_HLS_LIVE_UNITS {
        return Err(ManifestError::LiveCaptureLimit);
    }

    let resources_by_name: HashMap<&str, &HlsResource> = snapshot
        .resources
        .iter()
        .map(|resource| (resource.filename.as_str(), resource))
        .collect();
    let mut resources = Vec::new();
    let mut new_resource_names = Vec::new();
    for filename in &referenced_resources {
        if let Some(resource) = resources_by_name.get(filename.as_str()) {
            resources.push((*resource).clone());
            if !accumulator.seen_resources.contains(filename) {
                new_resource_names.push(filename.clone());
            }
        } else if !accumulator.seen_resources.contains(filename) {
            return Err(ManifestError::UnsupportedPlaylist);
        }
    }

    Ok(HlsLiveUpdate {
        resources,
        changed: !new_segments.is_empty(),
        end_list: snapshot.end_list,
        initial_playlist: accumulator
            .local_playlist
            .is_none()
            .then(|| snapshot.local_playlist.clone()),
        new_segments,
        referenced_resources,
        new_resource_names,
        media_sequence: 0,
        target_duration: snapshot.source_target_duration,
        ll_update: true,
        ll_new_unit_ids: new_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://cdn.example.test/live/index.m3u8";

    #[test]
    fn incremental_index_resolves_parent_alias_without_searching_older_sequences() {
        let mut ids = Vec::new();
        let mut segments = Vec::new();
        for sequence in 1..=2_000 {
            for part_index in 0..2 {
                ids.push(HlsLiveUnitId::Part {
                    media_sequence: sequence,
                    part_index,
                });
                segments.push(LocalSegment {
                    uri: format!("{sequence}-{part_index}.bin"),
                    duration: 0.5,
                    discontinuity: false,
                    map: None,
                    ..Default::default()
                });
            }
        }
        let index = HlsLiveIndex::from_units(&ids, &segments);
        assert_eq!(
            index.position(&HlsLiveUnitId::Part {
                media_sequence: 1_999,
                part_index: 1,
            }),
            Some(3_997)
        );
        let parent = LocalSegment {
            uri: "parent.bin".to_string(),
            duration: 1.0,
            discontinuity: false,
            map: None,
            ..Default::default()
        };
        assert_eq!(
            index.completed_parent_alias(&HlsLiveUnitId::Segment(1_999), &parent),
            Some(3_997)
        );
        assert_eq!(
            index.completed_parent_alias(&HlsLiveUnitId::Segment(2_000), &parent),
            Some(3_999)
        );
    }

    #[test]
    fn accepts_the_rfc_two_part_target_floor_and_rejects_values_below_it() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:10
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="10.0.m4s",INDEPENDENT=YES
"#;
        assert!(
            plan_hls_media_playlist(BASE, body).is_ok(),
            "the recommended three-times Part Target value is valid"
        );

        let at_protocol_floor = body
            .windows(b"PART-HOLD-BACK=1.5".len())
            .position(|window| window == b"PART-HOLD-BACK=1.5")
            .map(|offset| {
                let mut valid = body.to_vec();
                valid.splice(
                    offset..offset + b"PART-HOLD-BACK=1.5".len(),
                    b"PART-HOLD-BACK=1.0".iter().copied(),
                );
                valid
            })
            .expect("fixture contains PART-HOLD-BACK");
        assert!(
            plan_hls_media_playlist(BASE, &at_protocol_floor).is_ok(),
            "the RFC two-times Part Target floor is valid"
        );

        let below_floor = body
            .windows(b"PART-HOLD-BACK=1.5".len())
            .position(|window| window == b"PART-HOLD-BACK=1.5")
            .map(|offset| {
                let mut invalid = body.to_vec();
                invalid.splice(
                    offset..offset + b"PART-HOLD-BACK=1.5".len(),
                    b"PART-HOLD-BACK=0.999".iter().copied(),
                );
                invalid
            })
            .expect("fixture contains PART-HOLD-BACK");
        assert!(
            plan_hls_media_playlist(BASE, &below_floor).is_err(),
            "values below the RFC two-times Part Target minimum are invalid"
        );
    }

    #[test]
    fn declared_vod_ll_hls_playlist_without_end_list_is_finite() {
        let body = br#"#EXTM3U
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="100.0.ts",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="100.1.ts"
#EXTINF:1.0,
100.ts
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("declared LL-HLS VOD playlist")
            .expect("LL-HLS planner should handle partial segments");

        assert!(!plan.is_live);
        assert!(plan.end_list);
        assert_eq!(plan.blocking_reload, None);
        assert_eq!(plan.resources.len(), 2);
        assert_eq!(plan.ll_unit_ids.as_ref().map(|ids| ids.len()), Some(2));
    }

    #[test]
    fn retains_the_entire_live_window_when_start_offset_is_present() {
        let body = br#"#EXTM3U
#EXT-X-START:TIME-OFFSET=-2.0,PRECISE=YES
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:10
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="10.0.ts",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="10.1.ts"
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("LL-HLS planner")
            .expect("LL-HLS playlist is detected");

        assert!(plan.is_live);
        assert_eq!(plan.resources.len(), 2);
        assert_eq!(plan.local_playlist.segments.len(), 2);
        assert_eq!(plan.ll_unit_ids.as_ref().map(|ids| ids.len()), Some(2));
        assert!(
            plan.local_playlist.start.is_none(),
            "the local recording playlist must not retain the source playback hint"
        );
    }

    #[test]
    fn plans_parts_and_open_segment_without_fetching_preload_hint_or_parent() {
        let body = br#"#EXTM3U
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0
#EXT-X-PART-INF:PART-TARGET=0.333
#EXT-X-PART:DURATION=0.333,URI="100.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.333,URI="100.1.m4s"
#EXTINF:0.666,
100.m4s
#EXT-X-PART:DURATION=0.333,URI="101.0.m4s",INDEPENDENT=YES
#EXT-X-PRELOAD-HINT:TYPE=PART,URI="101.1.m4s"
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("LL-HLS planner")
            .expect("LL-HLS playlist is detected");
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == HlsResourceKind::Segment)
                .count(),
            3
        );
        assert_eq!(plan.local_playlist.segments.len(), 3);
        assert_eq!(plan.ll_unit_ids.as_ref().map(|ids| ids.len()), Some(3));
        assert_eq!(plan.blocking_reload, Some((101, Some(1))));
        assert!(
            !plan
                .resources
                .iter()
                .any(|resource| resource.url.path().ends_with("100.m4s"))
        );
        assert!(
            !plan
                .resources
                .iter()
                .any(|resource| resource.url.path().ends_with("101.1.m4s"))
        );
    }

    #[test]
    fn preserves_program_date_time_on_complete_ll_hls_segments() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:30
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO
#EXT-X-PROGRAM-DATE-TIME:2026-11-10T10:00:00.000Z
#EXTINF:1.0,
30.m4s
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("LL-HLS plan")
            .expect("LL-HLS detected");
        let local = m3u8_rs::parse_media_playlist_res(
            &plan
                .local_playlist_bytes()
                .expect("serialize local playlist"),
        )
        .expect("local LL-HLS playlist parses");
        assert!(local.segments[0].program_date_time.is_some());
    }

    #[test]
    fn preserves_program_date_time_on_the_first_part_of_a_completed_parent() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:40
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PROGRAM-DATE-TIME:2026-11-10T10:00:00.000Z
#EXT-X-PART:DURATION=0.5,URI="40.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="40.1.m4s"
#EXTINF:1.0,
40.m4s
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("LL-HLS plan")
            .expect("LL-HLS detected");
        let local = m3u8_rs::parse_media_playlist_res(
            &plan
                .local_playlist_bytes()
                .expect("serialize local playlist"),
        )
        .expect("local LL-HLS playlist parses");
        assert_eq!(local.segments.len(), 2);
        assert!(local.segments[0].program_date_time.is_some());
        assert!(local.segments[1].program_date_time.is_none());
    }

    #[test]
    fn preserves_program_date_time_on_open_ll_hls_parts() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:50
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXTINF:1.0,
50.m4s
#EXT-X-PROGRAM-DATE-TIME:2026-11-10T10:00:01.000Z
#EXT-X-PART:DURATION=0.5,URI="51.0.m4s",INDEPENDENT=YES
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("LL-HLS plan")
            .expect("LL-HLS detected");
        let local = m3u8_rs::parse_media_playlist_res(
            &plan
                .local_playlist_bytes()
                .expect("serialize local playlist"),
        )
        .expect("local LL-HLS playlist parses");
        assert_eq!(local.segments.len(), 2);
        assert!(local.segments[1].program_date_time.is_some());
    }

    #[test]
    fn malformed_program_date_time_falls_back_instead_of_being_discarded() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:60
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PROGRAM-DATE-TIME:not-a-date
#EXT-X-PART:DURATION=0.5,URI="60.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="60.1.m4s"
#EXTINF:1.0,
60.m4s
"#;
        assert!(plan_hls_media_playlist(BASE, body).is_err());
    }

    #[test]
    fn ext_x_start_required_attributes_remain_unsupported() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-START:TIME-OFFSET=1.0,REQ-FOO="bar"
#EXT-X-PART:DURATION=0.5,URI="7.0.m4s",INDEPENDENT=YES
"#;
        assert!(matches!(
            plan_hls_media_playlist(BASE, body),
            Err(ManifestError::UnsupportedPlaylist)
        ));
    }

    #[test]
    fn discontinuity_before_open_parts_is_attached_to_the_first_part() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:20
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXTINF:1.0,
20.m4s
#EXT-X-DISCONTINUITY
#EXT-X-PART:DURATION=0.5,URI="21.0.m4s",INDEPENDENT=YES
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("open parts after a discontinuity should be planned")
            .expect("LL-HLS detected");
        assert_eq!(plan.local_playlist.segments.len(), 2);
        assert!(!plan.local_playlist.segments[0].discontinuity);
        assert!(
            plan.local_playlist.segments[1].discontinuity,
            "the first open part carries the pending discontinuity"
        );
    }

    #[test]
    fn allows_mapless_partial_segments_for_runtime_mpegts_validation() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:5
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="5.0.ts",INDEPENDENT=YES
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("mapless partial playlist")
            .expect("LL-HLS detected");
        assert_eq!(plan.resources.len(), 1);
        assert_eq!(plan.resources[0].kind, HlsResourceKind::Segment);
        assert!(plan.local_playlist.segments[0].map.is_none());
        assert_eq!(
            plan.ll_parts_without_init_map().collect::<Vec<_>>(),
            vec![plan.local_playlist.segments[0].uri.as_str()]
        );
    }

    #[test]
    fn initialization_map_remains_active_for_later_segments_in_one_snapshot() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:20
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="20.0.m4s"
#EXTINF:0.5,
20.m4s
#EXT-X-PART:DURATION=0.5,URI="21.0.m4s"
#EXTINF:0.5,
21.m4s
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("two-segment partial playlist")
            .expect("LL-HLS detected");
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == HlsResourceKind::InitMap)
                .count(),
            1
        );
        assert_eq!(plan.local_playlist.segments.len(), 2);
        assert!(
            plan.local_playlist
                .segments
                .iter()
                .all(|segment| segment.map.is_some())
        );
    }

    #[test]
    fn completed_ll_hls_playlist_omits_gap_and_carries_its_discontinuity() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXTINF:1.0,
7.m4s
#EXT-X-DISCONTINUITY
#EXT-X-GAP
#EXTINF:1.0,
8.m4s
#EXTINF:1.0,
9.m4s
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("completed LL-HLS playlist with a declared gap")
            .expect("LL-HLS detected");
        let segments = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == HlsResourceKind::Segment)
            .collect::<Vec<_>>();
        assert_eq!(segments.len(), 2);
        assert!(segments[0].url.path().ends_with("/7.m4s"));
        assert!(segments[1].url.path().ends_with("/9.m4s"));
        assert_eq!(plan.local_playlist.segments.len(), 2);
        assert!(!plan.local_playlist.segments[0].discontinuity);
        assert!(plan.local_playlist.segments[1].discontinuity);
        assert_eq!(plan.ll_unit_ids.as_ref().map(|ids| ids.len()), Some(2));
    }

    #[test]
    fn live_ll_hls_parent_gap_still_falls_back() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXTINF:1.0,
7.m4s
#EXT-X-GAP
#EXTINF:1.0,
8.m4s
"#;
        assert!(matches!(
            plan_hls_media_playlist(BASE, body),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn complete_encrypted_segments_are_delegated_to_the_standard_planner() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-KEY:METHOD=AES-128,URI="key.bin"
#EXTINF:4.0,
segment.ts
"#;
        assert!(
            super::plan_hls_media_playlist(BASE, body)
                .expect("complete-segment LL-HLS playlist")
                .is_none(),
            "complete encrypted segments are handled by the standard HLS planner"
        );
    }

    #[test]
    fn method_none_keeps_unencrypted_complete_segments_on_the_ll_hls_planner() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-KEY:METHOD=NONE
#EXTINF:4.0,
segment.ts
"#;
        let plan = super::plan_hls_media_playlist(BASE, body)
            .expect("unencrypted complete-segment LL-HLS playlist")
            .expect("METHOD=NONE is not media encryption");
        assert!(plan.is_live);
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == HlsResourceKind::Segment)
                .count(),
            1
        );
        assert!(
            plan.local_playlist.segments[0].key.is_none(),
            "METHOD=NONE must not be serialized as an active encryption key"
        );
    }

    #[test]
    fn preload_hint_without_partial_segments_is_not_downloaded_speculatively() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES
#EXT-X-PRELOAD-HINT:TYPE=PART,URI="future.part"
#EXTINF:4.0,
segment.ts
"#;
        let plan = super::plan_hls_media_playlist(BASE, body)
            .expect("full-segment live playlist with preload hint")
            .expect("full-segment HLS plan");
        assert!(plan.is_live);
        assert!(
            plan.ll_unit_ids
                .as_ref()
                .is_some_and(|unit_ids| unit_ids.len() == 1),
            "the confirmed complete segment retains its live identity"
        );
        assert!(
            plan.resources
                .iter()
                .all(|resource| !resource.url.path().ends_with("future.part")),
            "preload hints are not committed until they appear as a confirmed part"
        );
    }

    #[test]
    fn rejects_delta_gap_and_encrypted_part_playlists() {
        let prefix = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5\n#EXT-X-PART-INF:PART-TARGET=0.5\n";
        let delta = format!(
            "{prefix}#EXT-X-SKIP:SKIPPED-SEGMENTS=2\n#EXT-X-PART:DURATION=0.5,URI=\"7.0.m4s\"\n"
        );
        assert!(matches!(
            plan_hls_media_playlist(BASE, delta.as_bytes()),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
        let gap = format!("{prefix}#EXT-X-PART:DURATION=0.5,URI=\"7.0.m4s\",GAP=YES\n");
        assert!(matches!(
            plan_hls_media_playlist(BASE, gap.as_bytes()),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
        let ad = format!("{prefix}#EXT-X-CUE-OUT:8\n#EXT-X-PART:DURATION=0.5,URI=\"7.0.m4s\"\n");
        assert!(
            plan_hls_media_playlist(BASE, ad.as_bytes()).is_ok(),
            "ad-insertion cue markers must be inert in LL-HLS media planning"
        );
        let encrypted = format!(
            "{prefix}#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXT-X-PART:DURATION=0.5,URI=\"7.0.m4s\"\n"
        );
        assert_eq!(
            plan_hls_media_playlist(BASE, encrypted.as_bytes()).err(),
            Some(ManifestError::UnsupportedEncryption)
        );
    }

    #[test]
    fn restored_parts_are_not_refetched_when_the_parent_segment_closes() {
        let first_body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="7.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="7.1.m4s"
#EXTINF:1.0,
7.m4s
"#;
        let first_plan = plan_hls_media_playlist(BASE, first_body)
            .expect("first LL-HLS plan")
            .expect("LL-HLS is detected");
        let mut accumulator = HlsLiveAccumulator::new();
        let first_update = accumulator
            .prepare_update(&first_plan)
            .expect("prepare first part window");
        assert_eq!(first_update.resources.len(), 3);
        accumulator
            .commit_update_with_budget(first_update, 0, 0)
            .expect("commit first part window");

        let staged = accumulator
            .local_playlist_bytes()
            .expect("serialize staged part playlist");
        let mut restored = HlsLiveAccumulator::from_local_playlist_bytes(&staged)
            .expect("restore part-aware accumulator");

        let next_body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXTINF:1.0,
7.m4s
#EXT-X-PART:DURATION=0.5,URI="8.0.m4s",INDEPENDENT=YES
"#;
        let next_plan = plan_hls_media_playlist(BASE, next_body)
            .expect("next LL-HLS plan")
            .expect("LL-HLS is detected");
        let next_update = restored
            .prepare_update(&next_plan)
            .expect("recognize the completed parent as already captured parts");

        assert_eq!(
            next_update
                .resources
                .iter()
                .filter(|resource| resource.kind == HlsResourceKind::Segment)
                .count(),
            1
        );
        assert!(next_update.resources.iter().any(|resource| {
            resource.kind == HlsResourceKind::Segment && resource.url.path().ends_with("/8.0.m4s")
        }));
        assert!(
            next_update
                .resources
                .iter()
                .any(|resource| resource.kind == HlsResourceKind::InitMap)
        );
        assert_eq!(
            next_update.ll_new_unit_ids,
            vec![HlsLiveUnitId::Part {
                media_sequence: 8,
                part_index: 0,
            }]
        );
        restored
            .commit_update_with_budget(next_update, 0, 0)
            .expect("commit the next part without duplicating the parent");
        assert_eq!(restored.segment_count(), 3);
    }

    #[test]
    fn overlapping_parts_are_included_for_missing_file_repair() {
        let initial = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="7.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="7.1.m4s"
"#;
        let initial_plan = plan_hls_media_playlist(BASE, initial)
            .expect("initial LL-HLS playlist")
            .expect("LL-HLS detected");
        let mut accumulator = HlsLiveAccumulator::new();
        let first_update = accumulator
            .prepare_update(&initial_plan)
            .expect("prepare first parts");
        accumulator
            .commit_update_with_budget(first_update, 0, 0)
            .expect("commit first parts");

        let next = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="7.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="7.1.m4s"
#EXT-X-PART:DURATION=0.5,URI="7.2.m4s"
"#;
        let next_plan = plan_hls_media_playlist(BASE, next)
            .expect("overlapping part snapshot")
            .expect("LL-HLS detected");
        let update = accumulator
            .prepare_update(&next_plan)
            .expect("prepare overlapping parts for missing-file repair");
        assert_eq!(
            update
                .resources
                .iter()
                .filter(|resource| resource.kind == HlsResourceKind::Segment)
                .count(),
            3
        );
        assert_eq!(
            update
                .resources
                .iter()
                .filter(|resource| resource.kind == HlsResourceKind::InitMap)
                .count(),
            1
        );
    }

    #[test]
    fn incomplete_parts_do_not_silently_replace_a_closed_parent() {
        let first_body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="7.0.m4s",INDEPENDENT=YES
"#;
        let first_plan = plan_hls_media_playlist(BASE, first_body)
            .expect("first LL-HLS plan")
            .expect("LL-HLS detected");
        let mut accumulator = HlsLiveAccumulator::new();
        let update = accumulator
            .prepare_update(&first_plan)
            .expect("prepare initial part");
        accumulator
            .commit_update_with_budget(update, 0, 0)
            .expect("commit initial part");

        let closed_parent_body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXTINF:1.0,
7.m4s
"#;
        let parent_plan = plan_hls_media_playlist(BASE, closed_parent_body)
            .expect("closed-parent plan")
            .expect("LL-HLS detected");
        assert_eq!(
            accumulator.prepare_update(&parent_plan).err(),
            Some(ManifestError::LiveSequenceGap)
        );
    }

    #[test]
    fn delta_playlist_reconstructs_its_skipped_prefix_from_committed_units() {
        let initial = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-SERVER-CONTROL:CAN-SKIP-UNTIL=24.0
#EXTINF:4,
100.ts
#EXTINF:4,
101.ts
"#;
        let initial_plan = plan_hls_media_playlist(BASE, initial)
            .expect("initial LL-HLS plan")
            .expect("LL-HLS detected");
        assert_eq!(
            initial_plan.ll_unit_ids.as_ref().expect("unit ids")[0],
            HlsLiveUnitId::Segment(100)
        );
        assert!(initial_plan.can_skip_until.is_some());

        let mut accumulator = HlsLiveAccumulator::new();
        let first_update = accumulator
            .prepare_update(&initial_plan)
            .expect("prepare initial playlist");
        accumulator
            .commit_update_with_budget(first_update, 0, 0)
            .expect("commit initial playlist");

        let delta = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-SERVER-CONTROL:CAN-SKIP-UNTIL=24.0
#EXT-X-SKIP:SKIPPED-SEGMENTS=2
#EXTINF:4,
102.ts
#EXTINF:4,
103.ts
"#;
        let delta_plan = plan_hls_media_playlist(BASE, delta)
            .expect("delta playlist plan")
            .expect("LL-HLS detected");
        assert_eq!(
            delta_plan.ll_unit_ids.as_ref().expect("unit ids")[0],
            HlsLiveUnitId::Segment(102)
        );
        let update = accumulator
            .prepare_update(&delta_plan)
            .expect("restore the omitted prefix from the committed capture");
        assert_eq!(update.new_segments.len(), 2);
        assert_eq!(
            update.ll_new_unit_ids,
            vec![HlsLiveUnitId::Segment(102), HlsLiveUnitId::Segment(103)]
        );
    }

    #[test]
    fn delta_playlist_carries_forward_the_last_committed_initialization_map() {
        let initial = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-SERVER-CONTROL:CAN-SKIP-UNTIL=24.0,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-MAP:URI="init.mp4"
#EXT-X-PART:DURATION=0.5,URI="7.0.m4s",INDEPENDENT=YES
"#;
        let initial_plan = plan_hls_media_playlist(BASE, initial)
            .expect("initial LL-HLS plan")
            .expect("LL-HLS detected");
        let mut accumulator = HlsLiveAccumulator::new();
        let initial_update = accumulator
            .prepare_update(&initial_plan)
            .expect("prepare initial part");
        accumulator
            .commit_update_with_budget(initial_update, 0, 0)
            .expect("commit initial part");

        let delta = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-SERVER-CONTROL:CAN-SKIP-UNTIL=24.0,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-SKIP:SKIPPED-SEGMENTS=1
#EXT-X-PART:DURATION=0.5,URI="8.0.m4s",INDEPENDENT=YES
"#;
        let delta_plan = plan_hls_media_playlist(BASE, delta)
            .expect("delta part plan")
            .expect("LL-HLS detected");
        let update = accumulator
            .prepare_update(&delta_plan)
            .expect("reuse initialization map from the committed prefix");
        assert_eq!(update.resources.len(), 1);
        assert!(update.resources[0].url.path().ends_with("/8.0.m4s"));
        assert_eq!(
            update.new_segments[0]
                .map
                .as_ref()
                .map(|map| map.uri.as_str()),
            Some(
                accumulator
                    .local_playlist
                    .as_ref()
                    .expect("committed playlist")
                    .segments[0]
                    .map
                    .as_ref()
                    .expect("committed map")
                    .uri
                    .as_str()
            )
        );
    }

    #[test]
    fn delta_playlist_with_an_uncaptured_gap_is_rejected() {
        let initial = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-SERVER-CONTROL:CAN-SKIP-UNTIL=24.0
#EXTINF:4,
100.ts
#EXTINF:4,
101.ts
"#;
        let initial_plan = plan_hls_media_playlist(BASE, initial)
            .expect("initial LL-HLS plan")
            .expect("LL-HLS detected");
        let mut accumulator = HlsLiveAccumulator::new();
        let update = accumulator
            .prepare_update(&initial_plan)
            .expect("prepare initial playlist");
        accumulator
            .commit_update_with_budget(update, 0, 0)
            .expect("commit initial playlist");

        let delta = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-SERVER-CONTROL:CAN-SKIP-UNTIL=24.0
#EXT-X-SKIP:SKIPPED-SEGMENTS=3
#EXTINF:4,
103.ts
"#;
        let delta_plan = plan_hls_media_playlist(BASE, delta)
            .expect("delta playlist plan")
            .expect("LL-HLS detected");
        assert_eq!(
            accumulator.prepare_update(&delta_plan).err(),
            Some(ManifestError::LiveSequenceGap)
        );
    }

    #[test]
    fn first_captured_part_must_be_independently_decodable() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="7.0.m4s"
"#;
        let plan = plan_hls_media_playlist(BASE, body)
            .expect("first-part plan")
            .expect("LL-HLS detected");
        let accumulator = HlsLiveAccumulator::new();
        assert!(matches!(
            accumulator.prepare_update(&plan),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn parses_rendition_reports_and_calculates_the_next_peer_reload() {
        let with_part = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:20
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES
#EXTINF:1.0,
20.ts
#EXT-X-RENDITION-REPORT:URI="audio/track.m3u8",LAST-MSN=31,LAST-PART=2
"#;
        let plan = plan_hls_media_playlist(BASE, with_part)
            .expect("rendition report plan")
            .expect("LL-HLS detected");
        let report = plan
            .rendition_reports
            .first()
            .expect("one rendition report");
        assert_eq!(
            report.playlist_url.as_str(),
            "https://cdn.example.test/live/audio/track.m3u8"
        );
        assert_eq!(report.next_reload(), Some((31, Some(3))));

        let without_part = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:20
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES
#EXTINF:1.0,
20.ts
#EXT-X-RENDITION-REPORT:URI="audio/track.m3u8",LAST-MSN=31
"#;
        let plan = plan_hls_media_playlist(BASE, without_part)
            .expect("rendition report without a partial segment")
            .expect("LL-HLS detected");
        assert_eq!(plan.rendition_reports[0].next_reload(), Some((32, None)));
    }

    #[test]
    fn staged_names_recover_part_identity_and_order() {
        let first = ll_resource_filename(
            7,
            Some(3),
            &Url::parse("https://cdn.example.test/7.3.m4s").expect("fixture URL"),
            None,
        );
        let second = ll_resource_filename(
            7,
            Some(4),
            &Url::parse("https://cdn.example.test/7.4.m4s").expect("fixture URL"),
            None,
        );
        let ids = [
            HlsLiveUnitId::from_staged_name(&first).expect("first staged part"),
            HlsLiveUnitId::from_staged_name(&second).expect("second staged part"),
        ];
        assert!(validate_live_unit_ids(&ids));
        assert_eq!(
            ids[0],
            HlsLiveUnitId::Part {
                media_sequence: 7,
                part_index: 3,
            }
        );
    }
}
