//! Bounded RFC 8216bis HLS playlist variable substitution.
//!
//! broadcast-hls provides the typed EXT-X-DEFINE representation. Grab applies
//! the cross-line substitutions before handing the expanded playlist to the
//! existing HLS planners; it does not replace their URL, resource, or timeline
//! validation.

use std::collections::HashMap;

use broadcast_hls::{Define, MediaPlaylist};
use url::Url;

use super::{MAX_MANIFEST_BYTES, ManifestError};

const MAX_VARIABLES: usize = 128;

pub(super) fn expand_media_playlist(
    manifest_url: &str,
    body: &[u8],
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
    let mut variables = HashMap::with_capacity(parsed.defines.len());
    for definition in parsed.defines {
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
                let value = substitute(&value, &variables)?;
                (name, value)
            }
            Define::QueryParam { name, extra_attrs } => {
                if !extra_attrs.is_empty() {
                    return Err(unsupported(
                        "EXT-X-DEFINE has unsupported required attributes",
                    ));
                }
                let mut values = playlist_url
                    .query_pairs()
                    .filter(|(key, _)| key.as_ref() == name.as_str())
                    .map(|(_, value)| value.into_owned());
                let value = values
                    .next()
                    .ok_or_else(|| unsupported("EXT-X-DEFINE query parameter is missing"))?;
                if values.next().is_some() {
                    return Err(unsupported("EXT-X-DEFINE query parameter is ambiguous"));
                }
                (name, value)
            }
            Define::Import { .. } => {
                return Err(unsupported(
                    "media-playlist IMPORT requires parent multivariant variables",
                ));
            }
            _ => return Err(unsupported("unsupported EXT-X-DEFINE declaration")),
        };
        validate_value(&value)?;
        if value.len() > MAX_MANIFEST_BYTES {
            return Err(unsupported("HLS variable exceeds its supported size limit"));
        }
        if variables.insert(name, value).is_some() {
            return Err(unsupported("duplicate EXT-X-DEFINE variable"));
        }
    }

    let mut expanded = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let line_content = line.trim_end_matches(['\r', '\n']);
        if line_content.starts_with("#EXT-X-DEFINE:") {
            expanded.push_str(&line[line_content.len()..]);
            continue;
        }
        expanded.push_str(&substitute_playlist_line(line_content, &variables)?);
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
    Ok(Some(expanded.into_bytes()))
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

pub(super) fn expand_master_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<Option<Vec<u8>>, ManifestError> {
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
            Ok(None)
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
    let mut variables = HashMap::with_capacity(parsed.defines.len());
    let mut total_variable_bytes = 0_usize;
    for definition in parsed.defines {
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
                let value = substitute(&value, &variables)?;
                (name, value)
            }
            Define::QueryParam { name, extra_attrs } => {
                if !extra_attrs.is_empty() {
                    return Err(unsupported(
                        "EXT-X-DEFINE has unsupported required attributes",
                    ));
                }
                let mut values = playlist_url
                    .query_pairs()
                    .filter(|(key, _)| key.as_ref() == name.as_str())
                    .map(|(_, value)| value.into_owned());
                let value = values
                    .next()
                    .ok_or_else(|| unsupported("EXT-X-DEFINE query parameter is missing"))?;
                if values.next().is_some() {
                    return Err(unsupported("EXT-X-DEFINE query parameter is ambiguous"));
                }
                (name, value)
            }
            Define::Import { .. } => {
                return Err(unsupported(
                    "master-playlist IMPORT declarations have no parent playlist",
                ));
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

    let mut expanded = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let line_content = line.trim_end_matches(['\r', '\n']);
        if line_content.trim_start().starts_with("#EXT-X-DEFINE:") {
            expanded.push_str(&line[line_content.len()..]);
            continue;
        }
        expanded.push_str(&substitute_playlist_line(line_content, &variables)?);
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
    if contains_master_variable_reference(&expanded) {
        return Err(unsupported(
            "master-playlist variable reference was not expanded",
        ));
    }
    Ok(Some(expanded.into_bytes()))
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
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",DEFAULT=YES,URI="{$root}/audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO="audio"
{$root}/video/main.m3u8
"#;
        let expanded = expand_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
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


/// Expand RFC 8216bis variables in an HLS multivariant playlist.
///
/// Only NAME/VALUE and QUERYPARAM definitions are accepted here. IMPORT is
/// meaningful only in a child media playlist and needs parent context to be
/// propagated through the selected-track handoff; until that is implemented,
/// it remains an explicit fallback.
