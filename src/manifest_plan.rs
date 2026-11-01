//! Authoritative selection and download planning for native HLS and DASH.

use gettextrs::gettext;
use sha2::{Digest, Sha256};
use url::Url;

use crate::manifest::{HlsAudioRendition, HlsPlan, HlsSubtitleRendition, HlsVariant, dash::{DashPresentation, DashRepresentationPlan}};
use crate::manifest_types::{ManifestAudioChoice, ManifestChoice, ManifestLookup, ManifestSubtitleChoice, NativeManifest};
use crate::media_types::{ManifestKind, ManifestSource};

/// A validated protocol plan ready for the runner to materialize and execute.
#[derive(Debug)]
pub(crate) enum ManifestDownloadPlan {
    HlsMedia(Box<HlsPlan>),
    HlsMaster {
        video_url: Url,
        audio_url: Option<Url>,
        subtitle_url: Option<Url>,
        subtitle_language: Option<String>,
    },
    Dash {
        video: Box<DashRepresentationPlan>,
        audio: Option<Box<DashRepresentationPlan>>,
        subtitle: Option<Box<DashRepresentationPlan>>,
    },
}

/// Select the closest representation to the configured video-quality preference.
#[must_use]
pub(crate) fn preferred_choice_index(choices: &[ManifestChoice], quality: &str) -> usize {
    let desired_height = crate::video_quality::quality_height(quality).map(u64::from);
    choices
        .iter()
        .enumerate()
        .min_by_key(|(index, choice)| {
            let height = choice.height.unwrap_or_default();
            let distance = desired_height.map_or(0, |desired| {
                choice
                    .height
                    .map_or(u64::MAX, |height| height.abs_diff(desired))
            });
            (
                distance,
                std::cmp::Reverse(height),
                std::cmp::Reverse(choice.bandwidth.unwrap_or_default()),
                *index,
            )
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

impl ManifestLookup {
    /// Build visible choices from highest to lowest quality, retaining manifest order on ties.
    pub(crate) fn choices(&self) -> Vec<ManifestChoice> {
        let mut choices = match &self.manifest {
            NativeManifest::HlsMaster(master) => master
                .variants
                .iter()
                .zip(hls_variant_keys(&master.variants))
                .filter_map(|(variant, key)| {
                    let key = key?;
                    Some(ManifestChoice {
                        key,
                        label: join_label_parts(
                            resolution_label(variant.width, variant.height),
                            bitrate_label(Some(
                                variant.average_bandwidth.unwrap_or(variant.bandwidth),
                            )),
                            codec_label(variant.codecs.as_deref()),
                        ),
                        height: variant.height,
                        bandwidth: Some(variant.average_bandwidth.unwrap_or(variant.bandwidth)),
                    })
                })
                .collect(),
            NativeManifest::HlsMedia(_) => vec![ManifestChoice {
                key: "hls-media".to_string(),
                label: gettext("Source playlist"),
                height: None,
                bandwidth: None,
            }],
            NativeManifest::Dash(dash) => dash
                .video_representations
                .iter()
                .map(|representation| ManifestChoice {
                    key: representation.key.clone(),
                    label: join_label_parts(
                        resolution_label(representation.width, representation.height),
                        bitrate_label(representation.bandwidth),
                        codec_label(representation.codecs.as_deref()),
                    ),
                    height: representation.height,
                    bandwidth: representation.bandwidth,
                })
                .collect(),
        };
        choices.sort_by(|left, right| {
            right
                .height
                .cmp(&left.height)
                .then_with(|| right.bandwidth.cmp(&left.bandwidth))
        });
        choices
    }

    /// List only audio renditions associated with the selected video representation.
    pub(crate) fn audio_choices(&self, video_key: &str) -> Vec<ManifestAudioChoice> {
        match &self.manifest {
            NativeManifest::HlsMaster(master) => {
                let group_id = master
                    .variants
                    .iter()
                    .zip(hls_variant_keys(&master.variants))
                    .find_map(|(variant, key)| {
                        (key.as_deref() == Some(video_key))
                            .then_some(variant.audio_group.as_deref())
                            .flatten()
                    });
                let Some(group_id) = group_id else {
                    return Vec::new();
                };
                master
                    .audio_renditions
                    .iter()
                    .zip(hls_audio_rendition_keys(&master.audio_renditions))
                    .filter_map(|(rendition, key)| {
                        let key = key?;
                        (rendition.group_id == group_id).then(|| ManifestAudioChoice {
                            key,
                            label: hls_audio_rendition_label(rendition),
                            is_default: rendition.is_default,
                        })
                    })
                    .collect()
            }
            NativeManifest::HlsMedia(_) => Vec::new(),
            NativeManifest::Dash(dash) => {
                if !dash
                    .video_representations
                    .iter()
                    .any(|representation| representation.key == video_key)
                {
                    return Vec::new();
                }
                let mut choices = dash
                    .audio_representations
                    .iter()
                    .map(|representation| {
                        let title = representation
                            .language
                            .as_deref()
                            .filter(|language| !language.is_empty())
                            .or(representation.representation_id.as_deref())
                            .map(short_label)
                            .unwrap_or_else(|| gettext("Audio track"));
                        let mut label = join_label_parts(
                            title,
                            bitrate_label(representation.bandwidth),
                            codec_label(representation.codecs.as_deref()),
                        );
                        if representation.is_default_audio {
                            label.push_str(" · ");
                            label.push_str(&gettext("Default"));
                        }
                        ManifestAudioChoice {
                            key: representation.key.clone(),
                            label,
                            is_default: representation.is_default_audio,
                        }
                    })
                    .collect::<Vec<_>>();
                choices.sort_by_key(|choice| !choice.is_default);
                choices
            }
        }
    }

    /// List only subtitle renditions associated with the selected video representation.
    pub(crate) fn subtitle_choices(&self, video_key: &str) -> Vec<ManifestSubtitleChoice> {
        match &self.manifest {
            NativeManifest::HlsMaster(master) => {
                let group_id = master
                    .variants
                    .iter()
                    .zip(hls_variant_keys(&master.variants))
                    .find_map(|(variant, key)| {
                        (key.as_deref() == Some(video_key))
                            .then_some(variant.subtitles_group.as_deref())
                            .flatten()
                    });
                let Some(group_id) = group_id else {
                    return Vec::new();
                };
                master
                    .subtitle_renditions
                    .iter()
                    .zip(hls_subtitle_rendition_keys(&master.subtitle_renditions))
                    .filter_map(|(rendition, key)| {
                        let key = key?;
                        (rendition.group_id == group_id).then(|| ManifestSubtitleChoice {
                            key,
                            label: hls_subtitle_rendition_label(rendition),
                            language: rendition.language.clone(),
                            is_default: rendition.is_default,
                        })
                    })
                    .collect()
            }
            NativeManifest::HlsMedia(_) => Vec::new(),
            NativeManifest::Dash(dash) => {
                if !dash.video_representations.iter().any(|track| track.key == video_key) {
                    return Vec::new();
                }
                let mut choices = dash
                    .subtitle_representations
                    .iter()
                    .map(|track| {
                        let title = track.language.as_deref()
                            .filter(|language| !language.is_empty())
                            .or(track.representation_id.as_deref())
                            .map(short_label)
                            .unwrap_or_else(|| gettext("Subtitle track"));
                        let mut label = join_label_parts(
                            title,
                            bitrate_label(track.bandwidth),
                            codec_label(track.codecs.as_deref()),
                        );
                        if track.is_default_subtitle {
                            label.push_str(" · ");
                            label.push_str(&gettext("Default"));
                        }
                        ManifestSubtitleChoice {
                            key: track.key.clone(),
                            label,
                            language: track.language.clone(),
                            is_default: track.is_default_subtitle,
                        }
                    })
                    .collect::<Vec<_>>();
                choices.sort_by_key(|choice| !choice.is_default);
                choices
            }
        }
    }

    /// Match yt-dlp's preference policy: configured language, then English,
    /// otherwise no subtitles. "off" and an unset preference never select a track.
    #[must_use]
    pub(crate) fn preferred_subtitle_choice_index(
        choices: &[ManifestSubtitleChoice],
        preferred_language: Option<&str>,
    ) -> Option<usize> {
        let preferred = preferred_language?;
        if preferred.eq_ignore_ascii_case("off") {
            return None;
        }
        choices
            .iter()
            .position(|choice| {
                choice.language.as_deref().is_some_and(|language| {
                    subtitle_language_matches(preferred, language)
                })
            })
            .or_else(|| {
                choices.iter().position(|choice| {
                    choice.language.as_deref().is_some_and(|language| {
                        subtitle_language_matches("en", language)
                    })
                })
            })
    }

    /// Resolve the selected subtitle key using the same language preference as yt-dlp.
    #[must_use]
    pub(crate) fn preferred_subtitle_key(
        &self,
        video_key: &str,
        preferred_language: Option<&str>,
    ) -> Option<String> {
        let choices = self.subtitle_choices(video_key);
        let index = Self::preferred_subtitle_choice_index(&choices, preferred_language)?;
        choices.get(index).map(|choice| choice.key.clone())
    }

    /// Index of the default audio choice, or the first available choice.
    #[must_use]
    pub(crate) fn preferred_audio_choice_index(choices: &[ManifestAudioChoice]) -> usize {
        choices
            .iter()
            .position(|choice| choice.is_default)
            .unwrap_or(0)
    }

    /// Return the representation closest to the configured video-quality preference.
    pub(crate) fn preferred_source(&self, quality: &str, subtitle_language: Option<&str>) -> Option<ManifestSource> {
        let choices = self.choices();
        let index = preferred_choice_index(&choices, quality);
        let choice = choices.get(index)?;
        let audio_choices = self.audio_choices(&choice.key);
        let audio_index = Self::preferred_audio_choice_index(&audio_choices);
        let audio_key = audio_choices
            .get(audio_index)
            .map(|audio| audio.key.as_str());
        let subtitle_key = self.preferred_subtitle_key(&choice.key, subtitle_language);
        self.selected_source(&choice.key, audio_key, subtitle_key.as_deref())
    }

    /// Validate and capture the exact video and optional audio selections.
    pub(crate) fn selected_source(
        &self,
        representation_key: &str,
        audio_key: Option<&str>,
        subtitle_key: Option<&str>,
    ) -> Option<ManifestSource> {
        let kind = match &self.manifest {
            NativeManifest::HlsMaster(master) => {
                hls_variant_keys(&master.variants)
                    .iter()
                    .any(|key| key.as_deref() == Some(representation_key))
                    .then_some(())?;
                ManifestKind::Hls
            }
            NativeManifest::HlsMedia(_) if representation_key == "hls-media" => ManifestKind::Hls,
            NativeManifest::HlsMedia(_) => return None,
            NativeManifest::Dash(dash) => {
                if !dash
                    .video_representations
                    .iter()
                    .any(|representation| representation.key == representation_key)
                {
                    return None;
                }
                ManifestKind::Dash
            }
        };
        if audio_key.is_some_and(|selected| {
            !self
                .audio_choices(representation_key)
                .iter()
                .any(|choice| choice.key == selected)
        }) {
            return None;
        }
        if subtitle_key.is_some_and(|selected| {
            !self.subtitle_choices(representation_key).iter().any(|choice| choice.key == selected)
        }) {
            return None;
        }
        let subtitle_language = subtitle_key
            .and_then(|selected| {
                self.subtitle_choices(representation_key)
                    .into_iter()
                    .find(|choice| choice.key == selected)
            })
            .and_then(|choice| choice.language)
            .as_deref()
            .and_then(normalize_subtitle_language);
        Some(ManifestSource {
            url: self.source_url.clone(),
            kind,
            representation_key: representation_key.to_string(),
            audio_key: audio_key.map(str::to_string),
            subtitle_key: subtitle_key.map(str::to_string),
            subtitle_language,
        })
    }
}

/// Apply the persisted selection to the freshly parsed manifest exactly once.
///
/// The returned plan contains the selected representation and all DASH
/// resources, or the HLS child-playlist URLs that the runner fetches before
/// executing the media playlists.
pub(crate) fn download_plan(
    lookup: ManifestLookup,
    source: &ManifestSource,
) -> Result<ManifestDownloadPlan, String> {
    if lookup.kind() != source.kind {
        return Err("The stream manifest is no longer supported".to_string());
    }

    match (lookup.manifest, source.kind) {
        (NativeManifest::HlsMedia(plan), ManifestKind::Hls)
            if source.representation_key == "hls-media" =>
        {
            Ok(ManifestDownloadPlan::HlsMedia(plan))
        }
        (NativeManifest::HlsMaster(master), ManifestKind::Hls) => {
            let variant = master
                .variants
                .iter()
                .zip(hls_variant_keys(&master.variants))
                .find_map(|(variant, key)| {
                    (key.as_deref() == Some(source.representation_key.as_str())).then_some(variant)
                })
                .ok_or_else(|| "The selected HLS variant is no longer available".to_string())?;
            let selected_audio = match source.audio_key.as_deref() {
                Some(audio_key) => Some(
                    master
                        .audio_renditions
                        .iter()
                        .zip(hls_audio_rendition_keys(&master.audio_renditions))
                        .find_map(|(rendition, key)| {
                            (key.as_deref() == Some(audio_key)).then_some(rendition)
                        })
                        .filter(|rendition| {
                            variant
                                .audio_group
                                .as_deref()
                                .is_some_and(|group| rendition.group_id == group)
                        })
                        .ok_or_else(|| {
                            "The selected HLS audio rendition is no longer available".to_string()
                        })?,
                ),
                None => variant.audio_group.as_deref().and_then(|group| {
                    master
                        .audio_renditions
                        .iter()
                        .filter(|rendition| rendition.group_id == group)
                        .find(|rendition| rendition.is_default)
                        .or_else(|| {
                            master
                                .audio_renditions
                                .iter()
                                .find(|rendition| rendition.group_id == group)
                        })
                }),
            };
            Ok(ManifestDownloadPlan::HlsMaster {
                video_url: variant.url.clone(),
                audio_url: selected_audio.and_then(|rendition| rendition.url.clone()),
                subtitle_url: match source.subtitle_key.as_deref() {
                    Some(subtitle_key) => Some(master.subtitle_renditions.iter()
                        .zip(hls_subtitle_rendition_keys(&master.subtitle_renditions))
                        .find_map(|(rendition, key)| (key.as_deref() == Some(subtitle_key)
                            && variant.subtitles_group.as_deref() == Some(rendition.group_id.as_str()))
                            .then_some(rendition.url.clone()))
                        .ok_or_else(|| "The selected HLS subtitle rendition is no longer available".to_string())?),
                    None => None,
                },
                subtitle_language: source.subtitle_key.as_deref().and_then(|subtitle_key| {
                    master.subtitle_renditions.iter()
                        .zip(hls_subtitle_rendition_keys(&master.subtitle_renditions))
                        .find_map(|(rendition, key)| {
                            (key.as_deref() == Some(subtitle_key)
                                && variant.subtitles_group.as_deref() == Some(rendition.group_id.as_str()))
                                .then(|| rendition.language.clone())
                        })
                }),
            })
        }
        (NativeManifest::Dash(dash), ManifestKind::Dash) => {
            let video =
                crate::manifest::dash::plan_dash_representation(&dash, &source.representation_key)
                    .map_err(|error| error.to_string())?;
            let selected_audio = match source.audio_key.as_deref() {
                Some(audio_key) => Some(
                    dash.audio_representations
                        .iter()
                        .find(|track| track.key == audio_key || track.live_key == audio_key)
                        .ok_or_else(|| {
                            "The selected DASH audio representation is no longer available"
                                .to_string()
                        })?,
                ),
                None => dash.default_audio(),
            };
            let audio = selected_audio
                .map(|track| {
                    let plan_key = if source.representation_key.starts_with("dash-live-")
                        || source.audio_key.as_deref() == Some(track.live_key.as_str())
                    {
                        track.live_key.as_str()
                    } else {
                        track.key.as_str()
                    };
                    crate::manifest::dash::plan_dash_representation(&dash, plan_key)
                        .map(Box::new)
                        .map_err(|error| error.to_string())
                })
                .transpose()?;
            let subtitle = match source.subtitle_key.as_deref() {
                Some(_) if matches!(&dash.presentation, DashPresentation::Dynamic(_)) => {
                    return Err("Native subtitles are not supported for live DASH presentations".to_string());
                }
                Some(key) => Some(Box::new(
                    crate::manifest::dash::plan_dash_representation(&dash, key)
                        .map_err(|error| error.to_string())?,
                )),
                None => None,
            };
            Ok(ManifestDownloadPlan::Dash {
                video: Box::new(video),
                audio,
                subtitle,
            })
        }
        _ => Err("The selected representation is not valid for this manifest".to_string()),
    }
}

#[must_use]
fn stable_hls_track_key(prefix: &str, track_kind: &[u8], stable_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"grab-hls-stable-track\0");
    hasher.update(track_kind);
    hasher.update([0]);
    hasher.update(stable_id.as_bytes());
    let digest = hasher.finalize();
    let fingerprint: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}{fingerprint}")
}

#[must_use]
pub(crate) fn hls_audio_rendition_key(rendition: &HlsAudioRendition) -> Option<String> {
    rendition
        .stable_rendition_id
        .as_deref()
        .map(|stable_id| stable_hls_track_key("hls-audio-", b"rendition", stable_id))
}

/// Build keys only for subtitle renditions with Apple stable rendition IDs.
pub(crate) fn hls_subtitle_rendition_keys(
    renditions: &[HlsSubtitleRendition],
) -> Vec<Option<String>> {
    renditions
        .iter()
        .map(|rendition| {
            rendition.stable_rendition_id.as_deref()
                .map(|stable_id| stable_hls_track_key("hls-subtitle-", b"subtitle", stable_id))
        })
        .collect()
}

fn hls_subtitle_rendition_label(rendition: &HlsSubtitleRendition) -> String {
    let language = rendition.language.as_deref()
        .filter(|language| !language.is_empty())
        .map(short_label);
    let name = (!rendition.name.is_empty()).then(|| rendition.name.clone());
    let mut label = join_label_parts(
        language.unwrap_or_else(|| gettext("Subtitle track")),
        name,
        None,
    );
    if rendition.is_default {
        label.push_str(" · ");
        label.push_str(&gettext("Default"));
    }
    label
}

fn normalize_subtitle_language(language: &str) -> Option<String> {
    let primary = language
        .split(['-', '_'])
        .next()
        .unwrap_or(language)
        .to_ascii_lowercase();
    let normalized = match primary.as_str() {
        "eng" => "en",
        "ara" => "ar",
        "deu" | "ger" => "de",
        "spa" => "es",
        "fra" | "fre" => "fr",
        "hin" => "hi",
        "ind" => "id",
        "ita" => "it",
        "jpn" => "ja",
        "kor" => "ko",
        "nld" | "dut" => "nl",
        "pol" => "pl",
        "por" => "pt",
        "rus" => "ru",
        "tur" => "tr",
        "vie" => "vi",
        "zho" | "chi" => "zh",
        code => code,
    };
    crate::video_prefs::subtitle_lang_active(normalized)
}

fn subtitle_language_matches(preferred: &str, available: &str) -> bool {
    let preferred = preferred.split(['-', '_']).next().unwrap_or(preferred).to_ascii_lowercase();
    let available = available.split(['-', '_']).next().unwrap_or(available).to_ascii_lowercase();
    if preferred == available {
        return true;
    }
    const ALIASES: &[(&str, &str)] = &[
        ("en", "eng"), ("ar", "ara"), ("de", "deu"), ("de", "ger"),
        ("es", "spa"), ("fr", "fra"), ("fr", "fre"), ("hi", "hin"),
        ("id", "ind"), ("it", "ita"), ("ja", "jpn"), ("ko", "kor"),
        ("nl", "nld"), ("nl", "dut"), ("pl", "pol"), ("pt", "por"),
        ("ru", "rus"), ("tr", "tur"), ("vi", "vie"), ("zh", "zho"),
        ("zh", "chi"),
    ];
    ALIASES.iter().any(|(two, three)| {
        (preferred == *two && available == *three)
            || (preferred == *three && available == *two)
    })
}

/// Build keys only for renditions with Apple stable rendition IDs.
#[must_use]
pub(crate) fn hls_audio_rendition_keys(renditions: &[HlsAudioRendition]) -> Vec<Option<String>> {
    renditions.iter().map(hls_audio_rendition_key).collect()
}

fn hls_audio_rendition_label(rendition: &HlsAudioRendition) -> String {
    let language = rendition
        .language
        .as_deref()
        .filter(|language| !language.is_empty());
    let name = (!rendition.name.is_empty()).then_some(rendition.name.as_str());
    let mut parts = Vec::with_capacity(3);
    if let Some(language) = language {
        parts.push(short_label(language));
    }
    if let Some(name) = name.filter(|name| Some(*name) != language) {
        parts.push(short_label(name));
    }
    if rendition.url.is_none() {
        parts.push(gettext("Embedded audio"));
    }
    if rendition.is_default {
        parts.push(gettext("Default"));
    }
    if parts.is_empty() {
        gettext("Audio track")
    } else {
        parts.join(" · ")
    }
}

#[must_use]
pub(crate) fn hls_variant_key(variant: &HlsVariant) -> Option<String> {
    variant
        .stable_variant_id
        .as_deref()
        .map(|stable_id| stable_hls_track_key("hls-variant-", b"variant", stable_id))
}

/// Build keys only for variants with Apple stable variant IDs.
#[must_use]
pub(crate) fn hls_variant_keys(variants: &[HlsVariant]) -> Vec<Option<String>> {
    variants.iter().map(hls_variant_key).collect()
}

fn resolution_label(width: Option<u64>, height: Option<u64>) -> String {
    match (width, height) {
        (_, Some(height)) if height > 0 => format!("{height}p"),
        (Some(width), _) if width > 0 => format!("{width}px wide"),
        _ => gettext("Unknown resolution"),
    }
}

fn bitrate_label(bandwidth: Option<u64>) -> Option<String> {
    bandwidth
        .filter(|bandwidth| *bandwidth > 0)
        .map(|bandwidth| format!("{:.1} Mb/s", bandwidth as f64 / 1_000_000.0))
}

fn codec_label(codecs: Option<&str>) -> Option<String> {
    codecs.filter(|codecs| !codecs.is_empty()).map(short_label)
}

fn short_label(value: &str) -> String {
    const MAX_LABEL_CHARS: usize = 36;
    let mut label = value.chars().take(MAX_LABEL_CHARS).collect::<String>();
    if value.chars().count() > MAX_LABEL_CHARS {
        label.push('…');
    }
    label
}

fn join_label_parts(resolution: String, bitrate: Option<String>, codecs: Option<String>) -> String {
    let mut parts = vec![resolution];
    parts.extend(bitrate);
    parts.extend(codecs);
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hls_subtitle_selection_uses_stable_rendition_ids_and_language_preference() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="subs/en.m3u8",STABLE-RENDITION-ID="subtitle-en"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,SUBTITLES="subs",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        let video_key = hls_variant_keys(&master.variants)[0]
            .clone()
            .expect("stable video key");
        let subtitle_key = hls_subtitle_rendition_keys(&master.subtitle_renditions)[0]
            .clone()
            .expect("stable subtitle key");
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(master),
        };

        let choices = lookup.subtitle_choices(&video_key);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].language.as_deref(), Some("en"));
        assert_eq!(
            ManifestLookup::preferred_subtitle_choice_index(&choices, Some("en")),
            Some(0)
        );
        assert_eq!(
            ManifestLookup::preferred_subtitle_choice_index(&choices, Some("off")),
            None
        );

        let source = lookup
            .selected_source(&video_key, None, Some(&subtitle_key))
            .expect("selected subtitle should be validated");
        let plan = download_plan(lookup, &source).expect("selected HLS subtitle plan");
        let ManifestDownloadPlan::HlsMaster {
            subtitle_url,
            subtitle_language,
            ..
        } = plan
        else {
            panic!("expected an HLS master plan");
        };
        assert_eq!(
            subtitle_url.expect("selected subtitle URL").as_str(),
            "https://cdn.example.test/subs/en.m3u8"
        );
        assert_eq!(subtitle_language.as_deref(), Some("en"));
    }

    #[test]
    fn dash_subtitle_selection_resolves_the_selected_text_representation() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="5" media="video-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" codecs="avc1.64001f"/>
    </AdaptationSet>
    <AdaptationSet contentType="text" mimeType="application/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate timescale="1" duration="5" initialization="sub-init.mp4" media="sub-$Number$.m4s"/>
      <Representation id="sub-en" bandwidth="1000" codecs="wvtt"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let dash = crate::manifest::dash::parse_dash_manifest(
            "https://cdn.example.test/manifest.mpd",
            body,
        )
        .expect("valid static DASH presentation");
        let video_key = dash.video_representations[0].key.clone();
        let subtitle_key = dash.subtitle_representations[0].key.clone();
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/manifest.mpd".to_string(),
            manifest: NativeManifest::Dash(dash),
        };

        let choices = lookup.subtitle_choices(&video_key);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].language.as_deref(), Some("en"));
        assert_eq!(
            ManifestLookup::preferred_subtitle_choice_index(&choices, Some("en")),
            Some(0)
        );
        let source = lookup
            .selected_source(&video_key, None, Some(&subtitle_key))
            .expect("selected DASH subtitle should be validated");
        let plan = download_plan(lookup, &source).expect("selected DASH subtitle plan");
        let ManifestDownloadPlan::Dash {
            subtitle: Some(subtitle),
            ..
        } = plan
        else {
            panic!("expected a DASH subtitle plan");
        };
        assert_eq!(
            subtitle.representation.representation_id.as_deref(),
            Some("sub-en")
        );
    }

    #[test]
    fn hls_download_plan_uses_the_selected_stable_video_and_audio_ids() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",LANGUAGE="en",STABLE-RENDITION-ID="rendition-en",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,AUDIO="audio",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        let video_key = hls_variant_keys(&master.variants)[0]
            .clone()
            .expect("stable video key");
        let audio_key = hls_audio_rendition_keys(&master.audio_renditions)[0]
            .clone()
            .expect("stable audio key");
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(master),
        };
        let source = lookup
            .selected_source(&video_key, Some(&audio_key), None)
            .expect("valid stable track selection");

        let plan = download_plan(lookup, &source).expect("valid selected download plan");
        let ManifestDownloadPlan::HlsMaster {
            video_url,
            audio_url,
            subtitle_url,
            ..
        } = plan
        else {
            panic!("expected an HLS master plan");
        };
        assert_eq!(
            video_url.as_str(),
            "https://cdn.example.test/video/720.m3u8"
        );
        assert_eq!(
            audio_url.expect("selected audio rendition URL").as_str(),
            "https://cdn.example.test/audio/en.m3u8"
        );
        assert!(subtitle_url.is_none());
    }

    #[test]
    fn download_plan_rejects_a_stale_hls_track_selection() {
        let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(master),
        };
        let source = ManifestSource {
            url: lookup.source_url.clone(),
            kind: ManifestKind::Hls,
            representation_key: "hls-variant-00000000000000000000000000000000".to_string(),
            audio_key: None,
            subtitle_key: None,
        };

        assert!(download_plan(lookup, &source).is_err());
    }
}
