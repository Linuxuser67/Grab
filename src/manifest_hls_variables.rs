//! Bounded RFC 8216bis HLS playlist variable substitution.
//!
//! broadcast-hls provides the typed EXT-X-DEFINE representation. Grab applies
//! the cross-line substitutions before handing the expanded playlist to the
//! existing HLS planners; it does not replace their URL, resource, or timeline
//! validation.

use std::collections::HashMap;

use broadcast_hls::{Define, MediaPlaylist};
use percent_encoding::percent_decode_str;
use url::Url;

use super::{MAX_MANIFEST_BYTES, ManifestError};

const MAX_VARIABLES: usize = 128;

type ExpandedMasterPlaylist = (Option<Vec<u8>>, Box<[(String, String)]>);

#[cfg(test)]
pub(super) fn expand_media_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<Option<Vec<u8>>, ManifestError> {
    expand_media_playlist_with_imports(manifest_url, body, &[])
}

pub(super) fn expand_media_playlist_with_imports(
    manifest_url: &str,
    body: &[u8],
    inherited_variables: &[(String, String)],
) -> Result<Option<Vec<u8>>, ManifestError> {
    let has_definitions = body
        .windows(b"#EXT-X-DEFINE:".len())
        .any(|window| window == b"#EXT-X-DEFINE:");
    let text = std::str::from_utf8(body)
        .map_err(|error| ManifestError::Parse(format!("HLS playlist is not UTF-8: {error}")))?;
    if !has_definitions {
        return if contains_variable_reference(text) {
            Err(unsupported(
                "variable reference has no EXT-X-DEFINE declaration",
            ))
        } else {
            Ok(None)
        };
    }
    let parsed = MediaPlaylist::parse(text).map_err(|error| {
        ManifestError::Parse(format!(
            "could not parse HLS variable declarations: {error}"
        ))
    })?;
    if parsed.defines.len() > MAX_VARIABLES {
        return Err(unsupported("playlist has too many variable declarations"));
    }

    let playlist_url = Url::parse(manifest_url).map_err(|_| ManifestError::InvalidManifestUrl)?;
    let variables = resolve_definitions(parsed.defines, &playlist_url, Some(inherited_variables))?;

    let expanded = expand_playlist_lines(text, &variables, false)?;
    Ok(Some(expanded.into_bytes()))
}

/// Read one URL query parameter using URI percent-decoding, not HTML form
/// decoding. A '+' is literal unless percent-encoded; it must not be turned
/// into a space. Bare keys, empty values, invalid UTF-8 and duplicate keys fail.
fn query_parameter(url: &Url, name: &str) -> Result<String, ManifestError> {
    let mut found = None;
    for pair in url.query().unwrap_or_default().split('&') {
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (pair, None),
        };
        let Ok(key) = percent_decode_str(raw_key).decode_utf8() else {
            // An unrelated opaque query key cannot match the UTF-8 DEFINE name.
            continue;
        };
        if key != name {
            continue;
        }
        let raw_value = raw_value
            .ok_or_else(|| unsupported("EXT-X-DEFINE query parameter has no associated value"))?;
        let value = percent_decode_str(raw_value)
            .decode_utf8()
            .map_err(|_| unsupported("EXT-X-DEFINE query parameter value is not valid UTF-8"))?
            .into_owned();
        if value.is_empty() {
            return Err(unsupported(
                "EXT-X-DEFINE query parameter has an empty value",
            ));
        }
        if found.replace(value).is_some() {
            return Err(unsupported("EXT-X-DEFINE query parameter is ambiguous"));
        }
    }
    found.ok_or_else(|| unsupported("EXT-X-DEFINE query parameter is missing"))
}

fn contains_variable_reference(text: &str) -> bool {
    text.lines().any(|line| {
        if line.starts_with("#EXT") {
            line.split('"')
                .enumerate()
                .any(|(index, part)| index % 2 == 1 && part.contains("{$"))
        } else {
            !line.starts_with('#') && !line.is_empty() && line.contains("{$")
        }
    })
}

fn substitute_playlist_line(
    line: &str,
    variables: &HashMap<String, String>,
) -> Result<String, ManifestError> {
    if !line.starts_with("#EXT") {
        return if line.starts_with('#') || line.is_empty() {
            Ok(line.to_string())
        } else {
            substitute(line, variables)
        };
    }

    let mut output = String::with_capacity(line.len());
    let mut remaining = line;
    while let Some(start) = remaining.find('"') {
        output.push_str(&remaining[..=start]);
        let after_quote = &remaining[start + 1..];
        let Some(end) = after_quote.find('"') else {
            output.push_str(after_quote);
            return Ok(output);
        };
        output.push_str(&substitute(&after_quote[..end], variables)?);
        output.push('"');
        remaining = &after_quote[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
}

fn substitute(input: &str, variables: &HashMap<String, String>) -> Result<String, ManifestError> {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(start) = remaining.find("{$") {
        output.push_str(&remaining[..start]);
        let reference = &remaining[start + 2..];
        let end = reference
            .find('}')
            .ok_or_else(|| unsupported("unterminated HLS variable reference"))?;
        let name = &reference[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(unsupported("invalid HLS variable reference"));
        }
        let value = variables
            .get(name)
            .ok_or_else(|| unsupported("HLS variable is not defined"))?;
        validate_value(value)?;
        output.push_str(value);
        if output.len() > MAX_MANIFEST_BYTES {
            return Err(unsupported(
                "expanded playlist exceeds its supported size limit",
            ));
        }
        remaining = &reference[end + 1..];
    }
    output.push_str(remaining);
    if output.len() > MAX_MANIFEST_BYTES {
        return Err(unsupported(
            "expanded playlist exceeds its supported size limit",
        ));
    }
    Ok(output)
}

fn validate_value(value: &str) -> Result<(), ManifestError> {
    if value.chars().any(|character| {
        character == '"' || character == '\r' || character == '\n' || character.is_control()
    }) {
        return Err(unsupported("HLS variable contains a forbidden character"));
    }
    Ok(())
}

fn resolve_definitions(
    definitions: Vec<Define>,
    playlist_url: &Url,
    inherited_variables: Option<&[(String, String)]>,
) -> Result<HashMap<String, String>, ManifestError> {
    let mut variables = HashMap::with_capacity(definitions.len());
    let mut total_variable_bytes = 0_usize;
    for definition in definitions {
        let (name, value) = match definition {
            Define::Name {
                name,
                value,
                extra_attrs,
            } => {
                if !extra_attrs.is_empty() {
                    return Err(unsupported(
                        "EXT-X-DEFINE has unsupported required attributes",
                    ));
                }
                (name, substitute(&value, &variables)?)
            }
            Define::QueryParam { name, extra_attrs } => {
                if !extra_attrs.is_empty() {
                    return Err(unsupported(
                        "EXT-X-DEFINE has unsupported required attributes",
                    ));
                }
                (name.clone(), query_parameter(playlist_url, &name)?)
            }
            Define::Import { name, extra_attrs } => {
                if !extra_attrs.is_empty() {
                    return Err(unsupported(
                        "EXT-X-DEFINE IMPORT has unsupported required attributes",
                    ));
                }
                let inherited_variables = inherited_variables.ok_or_else(|| {
                    unsupported("master-playlist IMPORT declarations have no parent playlist")
                })?;
                let value = inherited_variables
                    .iter()
                    .find_map(|(parent_name, value)| (parent_name == &name).then_some(value))
                    .cloned()
                    .ok_or_else(|| {
                        unsupported(
                            "IMPORT variable is not defined by the parent multivariant playlist",
                        )
                    })?;
                (name, value)
            }
            _ => return Err(unsupported("unsupported EXT-X-DEFINE declaration")),
        };
        validate_value(&value)?;
        total_variable_bytes = total_variable_bytes
            .checked_add(name.len())
            .and_then(|total| total.checked_add(value.len()))
            .ok_or_else(|| unsupported("aggregate HLS variable size overflow"))?;
        if total_variable_bytes > MAX_MANIFEST_BYTES {
            return Err(unsupported(
                "aggregate HLS variable values exceed their supported size limit",
            ));
        }
        if variables.insert(name, value).is_some() {
            return Err(unsupported("duplicate EXT-X-DEFINE variable"));
        }
    }
    Ok(variables)
}

fn expand_playlist_lines(
    text: &str,
    variables: &HashMap<String, String>,
    trim_define_indent: bool,
) -> Result<String, ManifestError> {
    let mut expanded = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let line_content = line.trim_end_matches(['\r', '\n']);
        let define_line = if trim_define_indent {
            line_content.trim_start().starts_with("#EXT-X-DEFINE:")
        } else {
            line_content.starts_with("#EXT-X-DEFINE:")
        };
        if define_line {
            expanded.push_str(&line[line_content.len()..]);
            continue;
        }
        expanded.push_str(&substitute_playlist_line(line_content, variables)?);
        expanded.push_str(&line[line_content.len()..]);
        if expanded.len() > MAX_MANIFEST_BYTES {
            return Err(unsupported(
                "expanded playlist exceeds its supported size limit",
            ));
        }
    }
    if expanded.len() > MAX_MANIFEST_BYTES {
        return Err(unsupported(
            "expanded playlist exceeds its supported size limit",
        ));
    }
    Ok(expanded)
}

fn unsupported(reason: &str) -> ManifestError {
    ManifestError::UnsupportedPlaylistFeature(format!("#EXT-X-DEFINE: {reason}"))
}

fn contains_master_variable_reference(text: &str) -> bool {
    text.lines().any(|raw_line| {
        let line = raw_line.trim_start();
        (line.starts_with("#EXT") || (!line.is_empty() && !line.starts_with('#')))
            && line.contains("{$")
    })
}

#[cfg(test)]
pub(super) fn expand_master_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<Option<Vec<u8>>, ManifestError> {
    expand_master_playlist_with_variables(manifest_url, body).map(|(body, _)| body)
}

pub(super) fn expand_master_playlist_with_variables(
    manifest_url: &str,
    body: &[u8],
) -> Result<ExpandedMasterPlaylist, ManifestError> {
    let text = std::str::from_utf8(body)
        .map_err(|error| ManifestError::Parse(format!("HLS playlist is not UTF-8: {error}")))?;
    let definitions = text
        .lines()
        .filter(|line| line.trim_start().starts_with("#EXT-X-DEFINE:"))
        .collect::<Vec<_>>();
    if definitions.is_empty() {
        return if contains_master_variable_reference(text) {
            Err(unsupported(
                "variable reference has no EXT-X-DEFINE declaration",
            ))
        } else {
            Ok((None, Vec::new().into_boxed_slice()))
        };
    }
    if definitions.len() > MAX_VARIABLES {
        return Err(unsupported("playlist has too many variable declarations"));
    }

    // Reuse broadcast-hls's typed DEFINE parser rather than reimplementing
    // HLS attribute-list quoting and comma handling. The synthetic media
    // playlist contains only the master playlist's declarations.
    let mut definition_playlist =
        String::from("#EXTM3U\n#EXT-X-VERSION:8\n#EXT-X-TARGETDURATION:1\n");
    for definition in &definitions {
        definition_playlist.push_str(definition.trim());
        definition_playlist.push('\n');
    }
    definition_playlist.push_str("#EXTINF:1,\nplaceholder.ts\n#EXT-X-ENDLIST\n");
    let parsed = MediaPlaylist::parse(&definition_playlist).map_err(|error| {
        ManifestError::Parse(format!(
            "could not parse HLS master-playlist variable declarations: {error}"
        ))
    })?;
    if parsed.defines.len() != definitions.len() {
        return Err(unsupported(
            "master-playlist DEFINE declarations could not be represented exactly",
        ));
    }

    let playlist_url = Url::parse(manifest_url).map_err(|_| ManifestError::InvalidManifestUrl)?;
    let variables = resolve_definitions(parsed.defines, &playlist_url, None)?;

    let expanded = expand_playlist_lines(text, &variables, true)?;
    if contains_master_variable_reference(&expanded) {
        return Err(unsupported(
            "master-playlist variable reference was not expanded",
        ));
    }
    let mut inherited_variables = variables.into_iter().collect::<Vec<_>>();
    inherited_variables.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok((
        Some(expanded.into_bytes()),
        inherited_variables.into_boxed_slice(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://cdn.example.test/live/index.m3u8";

    #[test]
    fn expands_master_variables_in_variant_and_rendition_uris() {
        let body = br#"#EXTM3U
#EXT-X-DEFINE:NAME="base",VALUE="streams"
#EXT-X-DEFINE:NAME="root",VALUE="{$base}/v1"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",DEFAULT=YES,AUTOSELECT=YES,URI="{$root}/audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO="audio"
{$root}/video/main.m3u8
"#;
        let expanded = expand_master_playlist("https://cdn.example.test/master.m3u8", body)
            .expect("master variables should expand")
            .expect("master contains DEFINE tags");
        let text = String::from_utf8(expanded).expect("expanded master is UTF-8");
        assert!(!text.contains("#EXT-X-DEFINE:"));
        assert!(text.contains("streams/v1/video/main.m3u8"));
        assert!(text.contains("URI=\"streams/v1/audio/en.m3u8\""));

        let plan = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            text.as_bytes(),
        )
        .expect("expanded master should parse");
        assert_eq!(
            plan.variants[0].url.as_str(),
            "https://cdn.example.test/streams/v1/video/main.m3u8"
        );
        assert_eq!(
            plan.audio_renditions[0].url.as_ref().map(Url::as_str),
            Some("https://cdn.example.test/streams/v1/audio/en.m3u8")
        );
    }

    #[test]
    fn expands_master_query_variables_from_the_final_manifest_url() {
        let body = br#"#EXTM3U
#EXT-X-DEFINE:QUERYPARAM="token"
#EXT-X-STREAM-INF:BANDWIDTH=1000000
https://cdn.example.test/{$token}/video.m3u8
"#;
        let expanded = expand_master_playlist(
            "https://origin.example.test/master.m3u8?token=signed%2Fvalue",
            body,
        )
        .expect("query variable should expand")
        .expect("master contains DEFINE tags");
        let text = String::from_utf8(expanded).expect("expanded master is UTF-8");
        assert!(text.contains("https://cdn.example.test/signed/value/video.m3u8"));
    }

    #[test]
    fn master_variables_fail_closed_for_import_missing_duplicate_and_injection() {
        let import = br#"#EXTM3U
#EXT-X-DEFINE:IMPORT="token"
#EXT-X-STREAM-INF:BANDWIDTH=1000000
video.m3u8
"#;
        assert!(expand_master_playlist(BASE, import).is_err());

        let missing = br#"#EXTM3U
#EXT-X-DEFINE:QUERYPARAM="token"
#EXT-X-STREAM-INF:BANDWIDTH=1000000
{$token}.m3u8
"#;
        assert!(expand_master_playlist(BASE, missing).is_err());

        assert!(
            expand_master_playlist(
                "https://cdn.example.test/master.m3u8?token=a&token=b",
                missing,
            )
            .is_err()
        );

        assert!(
            expand_master_playlist(
                "https://cdn.example.test/master.m3u8?token=%0A%23EXT-X-STREAM-INF",
                missing,
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unquoted_master_variable_references_instead_of_leaving_them_literal() {
        let body = br#"#EXTM3U
#EXT-X-DEFINE:NAME="root",VALUE="streams"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",URI={$root}/audio.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1000000
video.m3u8
"#;
        assert!(matches!(
            expand_master_playlist(BASE, body),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn media_playlist_import_reaches_media_fallback_not_master_expansion() {
        let media = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="token"
#EXTINF:4,
segment-{$token}.ts
#EXT-X-ENDLIST
"#;
        assert_eq!(
            crate::manifest::parse_hls_master_playlist(BASE, media).err(),
            Some(ManifestError::ExpectedMasterPlaylist)
        );
        assert!(matches!(
            crate::manifest::plan_hls_media_playlist(BASE, media),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn master_variable_references_without_definitions_fall_back() {
        let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000
{$missing}.m3u8
"#;
        assert!(matches!(
            expand_master_playlist(BASE, body),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn expands_name_value_and_query_parameter_variables() {
        let body = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:NAME="host",VALUE="cdn.example.test"
#EXT-X-DEFINE:QUERYPARAM="token"
#EXTINF:4,
https://{$host}/segment.ts?token={$token}
#EXT-X-ENDLIST
"#;
        let expanded = expand_media_playlist(
            "https://cdn.example.test/live/index.m3u8?token=abc%2F123",
            body,
        )
        .expect("valid variable declarations")
        .expect("variable tags are present");
        let text = String::from_utf8(expanded).expect("expanded playlist is UTF-8");
        assert!(!text.contains("#EXT-X-DEFINE:"));
        assert!(text.contains("https://cdn.example.test/segment.ts?token=abc/123"));
    }

    #[test]
    fn only_expands_uri_lines_and_quoted_attribute_values() {
        let body = br#"#EXTM3U
# comment {$not_a_variable}
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:NAME="folder",VALUE="media"
#EXT-X-UNKNOWN:VALUE={$not_a_variable}
#EXT-X-KEY:METHOD=AES-128,URI="keys/{$folder}.key"
#EXTINF:4,
{$folder}/segment.ts
#EXT-X-ENDLIST
"#;
        let expanded = expand_media_playlist(BASE, body)
            .expect("supported variable substitutions")
            .expect("definition is present");
        let text = String::from_utf8(expanded).expect("expanded playlist is UTF-8");
        assert!(text.contains("# comment {$not_a_variable}"));
        assert!(text.contains("#EXT-X-UNKNOWN:VALUE={$not_a_variable}"));
        assert!(text.contains("URI=\"keys/media.key\""));
        assert!(text.contains("media/segment.ts"));
    }

    #[test]
    fn expands_variables_before_building_the_native_fetch_plan() {
        let body = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:NAME="folder",VALUE="media"
#EXTINF:4,
{$folder}/segment.ts
#EXT-X-ENDLIST
"#;
        let plan = crate::manifest::plan_hls_media_playlist(BASE, body)
            .expect("variable-bearing media playlist should be planned");
        assert_eq!(
            plan.resources[0].url.as_str(),
            "https://cdn.example.test/live/media/segment.ts"
        );
    }

    #[test]
    fn expands_media_variables_before_ll_hls_planning() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-DEFINE:NAME="folder",VALUE="parts"
#EXT-X-PART:DURATION=0.5,URI="{$folder}/7.0.ts",INDEPENDENT=YES
"#;
        assert!(matches!(
            crate::manifest::hls_ll::plan_hls_media_playlist(BASE, body),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
        let plan = crate::manifest::plan_hls_media_playlist(BASE, body)
            .expect("variable-bearing LL-HLS playlist should be planned");
        assert_eq!(
            plan.resources[0].url.as_str(),
            "https://cdn.example.test/live/parts/7.0.ts"
        );
    }

    #[test]
    fn imports_explicit_master_variables_before_child_local_definitions() {
        let master = br#"#EXTM3U
#EXT-X-DEFINE:QUERYPARAM="token"
#EXT-X-DEFINE:NAME="root",VALUE="media/{$token}"
#EXT-X-STREAM-INF:BANDWIDTH=1000000
video.m3u8
"#;
        let (expanded_master, inherited) = expand_master_playlist_with_variables(
            "https://cdn.example.test/master.m3u8?token=signed-value",
            master,
        )
        .expect("master variables should expand");
        assert!(expanded_master.is_some());
        assert_eq!(
            inherited.as_ref(),
            &[
                ("root".to_string(), "media/signed-value".to_string()),
                ("token".to_string(), "signed-value".to_string()),
            ]
        );

        let child = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="root"
#EXT-X-DEFINE:NAME="segment",VALUE="{$root}/part"
#EXTINF:4,
{$segment}.ts
#EXT-X-ENDLIST
"#;
        let expanded = expand_media_playlist_with_imports(
            "https://cdn.example.test/child/index.m3u8",
            child,
            &inherited,
        )
        .expect("child IMPORT should resolve")
        .expect("child declares variables");
        let expanded = String::from_utf8(expanded).expect("expanded playlist is UTF-8");
        assert!(!expanded.contains("#EXT-X-DEFINE:"));
        assert!(expanded.contains("media/signed-value/part.ts"));

        let plan = crate::manifest::plan_hls_media_playlist_with_variables(
            "https://cdn.example.test/child/index.m3u8",
            child,
            &inherited,
        )
        .expect("child playlist should plan with inherited variables");
        assert_eq!(
            plan.resources[0].url.as_str(),
            "https://cdn.example.test/child/media/signed-value/part.ts"
        );
        assert_eq!(plan.inherited_variables.as_ref(), inherited.as_ref());
    }

    #[test]
    fn master_import_scope_survives_selected_video_audio_and_subtitle_planning() {
        let body = br#"#EXTM3U
#EXT-X-DEFINE:NAME="root",VALUE="media"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,URI="{$root}/audio.m3u8",STABLE-RENDITION-ID="audio-en"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,URI="{$root}/subs.m3u8",STABLE-RENDITION-ID="subs-en"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO="audio",SUBTITLES="subs",STABLE-VARIANT-ID="video-main"
{$root}/video.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("master with variable-backed rendition URLs");
        assert_eq!(
            master.inherited_variables.as_ref(),
            &[("root".to_string(), "media".to_string())]
        );

        let video_key = crate::media_types::ManifestRepresentationKey::from(
            crate::manifest_plan::hls_variant_keys(&master.variants)[0]
                .as_deref()
                .expect("master video key"),
        );
        let audio_key = crate::media_types::ManifestAudioKey::from(
            crate::manifest_plan::hls_audio_rendition_keys(&master.audio_renditions)[0]
                .as_deref()
                .expect("master audio key"),
        );
        let subtitle_key = crate::media_types::ManifestSubtitleKey::from(
            crate::manifest_plan::hls_subtitle_rendition_keys(&master.subtitle_renditions)[0]
                .as_deref()
                .expect("master subtitle key"),
        );
        let lookup = crate::manifest_types::ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: crate::manifest_types::NativeManifest::HlsMaster(Box::new(master)),
        };
        let source = lookup
            .selected_source(&video_key, Some(&audio_key), Some(&subtitle_key))
            .expect("all three rendition selections are associated with the variant");
        let plan = crate::manifest_plan::download_plan(lookup, &source)
            .expect("selected master download plan");
        let crate::manifest_plan::ManifestDownloadPlan::HlsMaster(plan) = plan else {
            panic!("expected selected HLS master plan");
        };
        assert_eq!(
            plan.inherited_variables.as_ref(),
            &[("root".to_string(), "media".to_string())]
        );
        assert_eq!(
            plan.video_url.as_str(),
            "https://cdn.example.test/media/video.m3u8"
        );
        assert_eq!(
            plan.audio_url.as_ref().map(Url::as_str),
            Some("https://cdn.example.test/media/audio.m3u8")
        );
        assert_eq!(
            plan.subtitle_url.as_ref().map(Url::as_str),
            Some("https://cdn.example.test/media/subs.m3u8")
        );
    }

    #[test]
    fn imported_variables_are_expanded_before_low_latency_planning_and_retained_for_reload() {
        let inherited = [("root".to_string(), "parts".to_string())];
        let body = br#"#EXTM3U
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-DEFINE:IMPORT="root"
#EXT-X-PART:DURATION=0.5,URI="{$root}/7.0.ts",INDEPENDENT=YES
"#;
        let plan = crate::manifest::plan_hls_media_playlist_with_variables(BASE, body, &inherited)
            .expect("LL-HLS child playlist should resolve its imported URI");
        assert!(plan.is_live);
        assert_eq!(
            plan.resources[0].url.as_str(),
            "https://cdn.example.test/live/parts/7.0.ts"
        );
        assert_eq!(plan.inherited_variables.as_ref(), inherited.as_slice());
        assert_eq!(
            plan.low_latency
                .as_ref()
                .map(|low_latency| low_latency.unit_ids.len()),
            Some(1),
            "the part remains represented as a live unit after expansion"
        );
    }

    #[test]
    fn imported_variables_must_exist_and_must_not_be_redeclared_or_extended() {
        let child = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="missing"
#EXTINF:4,
{$missing}.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            expand_media_playlist_with_imports(BASE, child, &[]),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));

        let duplicate = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="token"
#EXT-X-DEFINE:NAME="token",VALUE="shadow"
#EXTINF:4,
{$token}.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            expand_media_playlist_with_imports(
                BASE,
                duplicate,
                &[("token".into(), "parent".into())]
            ),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));

        let required_attribute = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="token",REQ-FOO="bar"
#EXTINF:4,
{$token}.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            expand_media_playlist_with_imports(
                BASE,
                required_attribute,
                &[("token".into(), "parent".into())]
            ),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn rejects_import_without_parent_playlist_context() {
        let body = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="token"
#EXTINF:4,
segment-{$token}.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            expand_media_playlist(BASE, body),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn query_parameter_decodes_percent_escapes_without_form_decoding() {
        let url = Url::parse("https://cdn.example.test/live.m3u8?token=a+b%2Fc").unwrap();
        assert_eq!(query_parameter(&url, "token").unwrap(), "a+b/c");
    }

    #[test]
    fn query_parameter_rejects_bare_and_empty_values() {
        for raw in [
            "https://cdn.example.test/live.m3u8?token",
            "https://cdn.example.test/live.m3u8?token=",
        ] {
            let url = Url::parse(raw).unwrap();
            assert!(query_parameter(&url, "token").is_err(), "{raw}");
        }
    }

    #[test]
    fn rejects_missing_and_ambiguous_query_parameters() {
        let body = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:QUERYPARAM="token"
#EXTINF:4,
segment-{$token}.ts
#EXT-X-ENDLIST
"#;
        assert!(expand_media_playlist(BASE, body).is_err());
        assert!(
            expand_media_playlist(
                "https://cdn.example.test/live/index.m3u8?token=a&token=b",
                body
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_undefined_variables_and_injection_values() {
        let undefined = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXTINF:4,
segment-{$missing}.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            expand_media_playlist(BASE, undefined),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));

        let injected = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:QUERYPARAM="token"
#EXTINF:4,
segment-{$token}.ts
#EXT-X-ENDLIST
"#;
        assert!(
            expand_media_playlist(
                "https://cdn.example.test/live/index.m3u8?token=%0A%23EXT-X-KEY",
                injected
            )
            .is_err()
        );
    }
}
