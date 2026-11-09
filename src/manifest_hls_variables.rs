//! Bounded RFC 8216bis playlist variable substitution.
//!
//! broadcast-hls provides the typed EXT-X-DEFINE representation. Grab applies
//! cross-line substitutions before handing expanded playlists to the existing
//! HLS planners; it retains ownership of URLs, resources, and timeline checks.

use std::collections::{HashMap, HashSet};
use std::fmt;

use broadcast_hls::{Define, MasterPlaylist, MediaPlaylist};
use url::Url;

use super::{MAX_MANIFEST_BYTES, ManifestError};

const MAX_VARIABLES: usize = 128;

/// Variables defined by a multivariant playlist, retained only for this in-memory selection.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct HlsVariableEnvironment {
    values: HashMap<String, String>,
}

impl fmt::Debug for HlsVariableEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HlsVariableEnvironment")
            .field("count", &self.values.len())
            .finish()
    }
}

impl HlsVariableEnvironment {
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
}

/// Expand master-playlist NAME/VALUE and QUERYPARAM declarations.
/// Returns the parent scope so a selected child media playlist can explicitly IMPORT names.
pub(crate) fn expand_master_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<(Option<Vec<u8>>, HlsVariableEnvironment), ManifestError> {
    let text = playlist_text(body)?;
    if !has_definitions(body) {
        if contains_variable_reference(text) {
            return Err(unsupported(
                "master-playlist variable reference has no EXT-X-DEFINE declaration",
            ));
        }
        return Ok((None, HlsVariableEnvironment::default()));
    }

    let parsed = MasterPlaylist::parse(text).map_err(|error| {
        ManifestError::Parse(format!(
            "could not parse HLS master variable declarations: {error}"
        ))
    })?;
    if parsed.defines.len() > MAX_VARIABLES {
        return Err(unsupported("playlist has too many variable declarations"));
    }
    let playlist_url = Url::parse(manifest_url).map_err(|_| ManifestError::InvalidManifestUrl)?;
    let variables = collect_variables(parsed.defines, &playlist_url, None, false)?;
    let expanded = expand_playlist_text(text, &variables)?;
    Ok((Some(expanded.into_bytes()), variables))
}

/// Expand local media-playlist declarations using no parent scope.
pub(super) fn expand_media_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<Option<Vec<u8>>, ManifestError> {
    expand_media_playlist_with_parent(
        manifest_url,
        body,
        &HlsVariableEnvironment::default(),
    )
}

/// Expand a child media playlist, resolving IMPORT only from the selected parent master playlist.
pub(crate) fn expand_media_playlist_with_parent(
    manifest_url: &str,
    body: &[u8],
    parent_variables: &HlsVariableEnvironment,
) -> Result<Option<Vec<u8>>, ManifestError> {
    let text = playlist_text(body)?;
    if !has_definitions(body) {
        return if contains_variable_reference(text) {
            Err(unsupported(
                "media-playlist variable reference has no local or imported declaration",
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
    let variables = collect_variables(
        parsed.defines,
        &playlist_url,
        Some(parent_variables),
        true,
    )?;
    let expanded = expand_playlist_text(text, &variables)?;
    Ok(Some(expanded.into_bytes()))
}

fn playlist_text(body: &[u8]) -> Result<&str, ManifestError> {
    std::str::from_utf8(body)
        .map_err(|error| ManifestError::Parse(format!("HLS playlist is not UTF-8: {error}")))
}

fn has_definitions(body: &[u8]) -> bool {
    body.windows(b"#EXT-X-DEFINE:".len())
        .any(|window| window == b"#EXT-X-DEFINE:")
}

fn collect_variables(
    definitions: Vec<Define>,
    playlist_url: &Url,
    parent_variables: Option<&HlsVariableEnvironment>,
    allow_import: bool,
) -> Result<HlsVariableEnvironment, ManifestError> {
    if definitions.len() > MAX_VARIABLES {
        return Err(unsupported("playlist has too many variable declarations"));
    }
    let mut variables = HlsVariableEnvironment::default();
    let mut declared_names = HashSet::with_capacity(definitions.len());

    for definition in definitions {
        let (name, value, extra_attrs) = match definition {
            Define::Name {
                name,
                value,
                extra_attrs,
            } => {
                let value = substitute(&value, &variables)?;
                (name, value, extra_attrs)
            }
            Define::QueryParam { name, extra_attrs } => {
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
                (name, value, extra_attrs)
            }
            Define::Import { name, extra_attrs } => {
                if !allow_import {
                    return Err(unsupported(
                        "EXT-X-DEFINE IMPORT is only valid in a child media playlist",
                    ));
                }
                let value = parent_variables
                    .and_then(|parent| parent.values.get(&name))
                    .cloned()
                    .ok_or_else(|| {
                        unsupported("EXT-X-DEFINE IMPORT does not exist in the parent playlist")
                    })?;
                (name, value, extra_attrs)
            }
            _ => return Err(unsupported("unsupported EXT-X-DEFINE declaration")),
        };

        if !extra_attrs.is_empty() {
            return Err(unsupported(
                "EXT-X-DEFINE has unsupported required attributes",
            ));
        }
        if !declared_names.insert(name.clone()) {
            return Err(unsupported("duplicate EXT-X-DEFINE variable"));
        }
        validate_value(&value)?;
        if value.len() > MAX_MANIFEST_BYTES {
            return Err(unsupported("HLS variable exceeds its supported size limit"));
        }
        variables.values.insert(name, value);
    }

    Ok(variables)
}

fn expand_playlist_text(
    text: &str,
    variables: &HlsVariableEnvironment,
) -> Result<String, ManifestError> {
    let mut expanded = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let line_content = line.trim_end_matches(['\r', '\n']);
        if line_content.starts_with("#EXT-X-DEFINE:") {
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
    variables: &HlsVariableEnvironment,
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

fn substitute(
    input: &str,
    variables: &HlsVariableEnvironment,
) -> Result<String, ManifestError> {
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
            .values
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

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://cdn.example.test/live/index.m3u8";

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
    fn expands_master_variables_and_imports_them_into_a_child_playlist() {
        let master = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-DEFINE:NAME="folder",VALUE="media"
#EXT-X-DEFINE:QUERYPARAM="token"
#EXT-X-STREAM-INF:BANDWIDTH=1000000
{$folder}/index-{$token}.m3u8
"#;
        let (expanded, parent) = expand_master_playlist(
            "https://cdn.example.test/master.m3u8?token=abc%2F123",
            master,
        )
        .expect("valid master variable declarations");
        let expanded = String::from_utf8(expanded.expect("master had definitions"))
            .expect("expanded master is UTF-8");
        assert!(expanded.contains("media/index-abc/123.m3u8"));
        assert_eq!(parent.get("folder"), Some("media"));
        assert_eq!(parent.get("token"), Some("abc/123"));

        let child = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="folder"
#EXT-X-DEFINE:IMPORT="token"
#EXTINF:4,
{$folder}/segment-{$token}.ts
#EXT-X-ENDLIST
"#;
        let expanded = expand_media_playlist_with_parent(
            "https://cdn.example.test/media/index.m3u8",
            child,
            &parent,
        )
        .expect("child imports variables")
        .expect("child has IMPORT declarations");
        let expanded = String::from_utf8(expanded).expect("expanded child is UTF-8");
        assert!(expanded.contains("media/segment-abc/123.ts"));
    }

    #[test]
    fn rejects_imports_that_are_not_declared_by_the_parent() {
        let parent = HlsVariableEnvironment::default();
        let child = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="missing"
#EXTINF:4,
segment-{$missing}.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            expand_media_playlist_with_parent(BASE, child, &parent),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn rejects_import_declarations_in_master_playlists() {
        let master = br#"#EXTM3U
#EXT-X-DEFINE:IMPORT="missing"
#EXT-X-STREAM-INF:BANDWIDTH=1000000
video.m3u8
"#;
        assert!(matches!(
            expand_master_playlist(BASE, master),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
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
    fn expands_imported_variables_in_the_media_planner() {
        let parent = HlsVariableEnvironment {
            values: [("folder".to_string(), "media".to_string())].into(),
        };
        let body = br#"#EXTM3U
#EXT-X-VERSION:8
#EXT-X-TARGETDURATION:4
#EXT-X-DEFINE:IMPORT="folder"
#EXTINF:4,
{$folder}/segment.ts
#EXT-X-ENDLIST
"#;
        let plan = crate::manifest::plan_hls_media_playlist_with_parent_variables(
            BASE,
            body,
            &parent,
        )
        .expect("imported child playlist should be planned");
        assert_eq!(
            plan.resources[0].url.as_str(),
            "https://cdn.example.test/live/media/segment.ts"
        );
        assert_eq!(plan.inherited_variables.get("folder"), Some("media"));
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
