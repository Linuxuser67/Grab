//! Authoritative selection and download planning for native HLS and DASH.

use gettextrs::gettext;
use sha2::{Digest, Sha256};
use url::Url;

use crate::manifest::{
    HlsAudioRendition, HlsMasterPlan, HlsPlan, HlsVariant,
    dash::{DashManifestPlan, DashRepresentationPlan},
};
use crate::manifest_types::{ManifestAudioChoice, ManifestChoice, ManifestLookup, NativeManifest};
use crate::media_types::{ManifestKind, ManifestSource};

/// A validated protocol plan ready for the runner to materialize and execute.
#[derive(Debug)]
pub(crate) enum ManifestDownloadPlan {
    HlsMedia(HlsPlan),
    HlsMaster {
        video_url: Url,
        audio_url: Option<Url>,
    },
    Dash {
        video: DashRepresentationPlan,
        audio: Option<Box<DashRepresentationPlan>>,
    },
}

#[derive(Debug)]
pub(crate) struct ManifestPlanError(String);

impl std::fmt::Display for ManifestPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ManifestPlanError {}

impl ManifestPlanError {
    fn unsupported(message: impl Into<String>) -> Self {
        Self(message.into())
    }
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
                choice.height.map_or(u64::MAX, |height| height.abs_diff(desired))
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

    /// Index of the default audio choice, or the first available choice.
    #[must_use]
    pub(crate) fn preferred_audio_choice_index(choices: &[ManifestAudioChoice]) -> usize {
        choices
            .iter()
            .position(|choice| choice.is_default)
            .unwrap_or(0)
    }

    /// Return the representation closest to the configured video-quality preference.
    pub(crate) fn preferred_source(&self, quality: &str) -> Option<ManifestSource> {
        let choices = self.choices();
        let index = preferred_choice_index(&choices, quality);
        let choice = choices.get(index)?;
        let audio_choices = self.audio_choices(&choice.key);
        let audio_index = Self::preferred_audio_choice_index(&audio_choices);
        let audio_key = audio_choices.get(audio_index).map(|audio| audio.key.as_str());
        self.selected_source(&choice.key, audio_key)
    }

    /// Validate and capture the exact video and optional audio selections.
    pub(crate) fn selected_source(
        &self,
        representation_key: &str,
        audio_key: Option<&str>,
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
        Some(ManifestSource {
            url: self.source_url.clone(),
            kind,
            representation_key: representation_key.to_string(),
            audio_key: audio_key.map(str::to_string),
        })
    }
}

/// Apply the persisted selection to the freshly parsed manifest exactly once.
///
/// The returned plan contains the selected representation and all DASH
/// resources, or the HLS child-playlist URLs that intake must fetch before
/// the runner can execute the media playlists.
pub(crate) fn download_plan(
    lookup: &ManifestLookup,
    source: &ManifestSource,
) -> Result<ManifestDownloadPlan, ManifestPlanError> {
    if lookup.kind() != source.kind {
        return Err(ManifestPlanError::unsupported(
            "The stream manifest protocol changed since this item was queued",
        ));
    }

    match (&lookup.manifest, source.kind) {
        (NativeManifest::HlsMedia(plan), ManifestKind::Hls)
            if source.representation_key == "hls-media" && source.audio_key.is_none() =>
        {
            Ok(ManifestDownloadPlan::HlsMedia(plan.as_ref().clone()))
        }
        (NativeManifest::HlsMaster(master), ManifestKind::Hls) => {
            let variant = master
                .variants
                .iter()
                .zip(hls_variant_keys(&master.variants))
                .find_map(|(variant, key)| {
                    (key.as_deref() == Some(source.representation_key.as_str()))
                        .then_some(variant)
                })
                .ok_or_else(|| {
                    ManifestPlanError::unsupported(
                        "The selected HLS variant is no longer available",
                    )
                })?;
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
                            ManifestPlanError::unsupported(
                                "The selected HLS audio rendition is no longer available",
                            )
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
            })
        }
        (NativeManifest::Dash(dash), ManifestKind::Dash) => {
            let video = crate::manifest::dash::plan_dash_representation(
                dash,
                &source.representation_key,
            )
            .map_err(|error| ManifestPlanError::unsupported(error.to_string()))?;
            let selected_audio = match source.audio_key.as_deref() {
                Some(audio_key) => Some(
                    dash.audio_representations
                        .iter()
                        .find(|track| track.key == audio_key || track.live_key == audio_key)
                        .ok_or_else(|| {
                            ManifestPlanError::unsupported(
                                "The selected DASH audio representation is no longer available",
                            )
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
                    crate::manifest::dash::plan_dash_representation(dash, plan_key)
                        .map(Box::new)
                        .map_err(|error| ManifestPlanError::unsupported(error.to_string()))
                })
                .transpose()?;
            Ok(ManifestDownloadPlan::Dash { video, audio })
        }
        _ => Err(ManifestPlanError::unsupported(
            "The selected representation is not valid for this manifest",
        )),
    }
}

#[must_use]
fn stable_hls_track_key(prefix: &str, track_kind: &[u8], stable_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"grab-hls-stable-track\\0");
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

/// Build keys only for renditions with Apple stable rendition IDs.
#[must_use]
pub(crate) fn hls_audio_rendition_keys(
    renditions: &[HlsAudioRendition],
) -> Vec<Option<String>> {
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
