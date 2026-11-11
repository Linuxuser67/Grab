//! Authoritative selection and download planning for native HLS and DASH.

use gettextrs::gettext;
use sha2::{Digest, Sha256};
use url::Url;

use crate::manifest::{
    HlsAudioRendition, HlsContentSteering, HlsPlan, HlsSubtitleRendition, HlsVariant,
    dash::{DashPresentation, DashRepresentationPlan},
};
use crate::manifest_types::{
    ManifestAudioChoice, ManifestChoice, ManifestLookup, ManifestSubtitleChoice, NativeManifest,
};
use crate::media_types::{
    ManifestAudioKey, ManifestKind, ManifestRepresentationKey, ManifestSource, ManifestSubtitleKey,
};

/// A validated protocol plan ready for the runner to materialize and execute.
#[derive(Debug)]
pub(crate) struct HlsMasterDownloadPlan {
    pub(crate) video_url: Url,
    pub(crate) audio_url: Option<Url>,
    pub(crate) subtitle_url: Option<Url>,
    pub(crate) subtitle_language: Option<String>,
    pub(crate) inherited_variables: Box<[(String, String)]>,
    pub(crate) content_steering: Option<HlsContentSteering>,
}

#[derive(Debug)]
pub(crate) enum ManifestDownloadPlan {
    HlsMedia(Box<HlsPlan>),
    HlsMaster(Box<HlsMasterDownloadPlan>),
    Dash {
        video: Box<DashRepresentationPlan>,
        audio: Option<Box<DashRepresentationPlan>>,
        subtitle: Option<Box<DashRepresentationPlan>>,
    },
}

/// Infer a supported output container from a DASH representation MIME type.
pub(crate) fn dash_output_container_extension(mime_type: Option<&str>) -> Option<&'static str> {
    let mime_type = mime_type?.split(';').next()?.trim();
    if mime_type.eq_ignore_ascii_case("video/mp2t")
        || mime_type.eq_ignore_ascii_case("application/mp2t")
    {
        Some("ts")
    } else if mime_type.eq_ignore_ascii_case("video/mp4")
        || mime_type.eq_ignore_ascii_case("application/mp4")
    {
        Some("mp4")
    } else {
        None
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
                    let quality = join_label_parts(
                        resolution_label(variant.width, variant.height),
                        bitrate_label(Some(variant.average_bandwidth.unwrap_or(variant.bandwidth))),
                        codec_label(variant.codecs.as_deref()),
                    );
                    let label = variant
                        .video_rendition_name
                        .as_deref()
                        .map(|name| format!("{} · {quality}", short_label(name)))
                        .unwrap_or(quality);
                    Some(ManifestChoice {
                        key: key.into(),
                        label,
                        height: variant.height,
                        bandwidth: Some(variant.average_bandwidth.unwrap_or(variant.bandwidth)),
                    })
                })
                .collect(),
            NativeManifest::HlsMedia(_) => vec![ManifestChoice {
                key: "hls-media".into(),
                label: gettext("Source playlist"),
                height: None,
                bandwidth: None,
            }],
            NativeManifest::Dash(dash) => dash
                .video_representations
                .iter()
                .map(|representation| ManifestChoice {
                    key: representation.key.clone().into(),
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
    pub(crate) fn audio_choices(
        &self,
        video_key: &ManifestRepresentationKey,
    ) -> Vec<ManifestAudioChoice> {
        match &self.manifest {
            NativeManifest::HlsMaster(master) => {
                let group_id = master
                    .variants
                    .iter()
                    .zip(hls_variant_keys(&master.variants))
                    .find_map(|(variant, key)| {
                        (key.as_deref() == Some(video_key.as_str()))
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
                            key: key.into(),
                            label: hls_audio_rendition_label(rendition),
                            is_default: rendition.is_default,
                            is_autoselect: rendition.is_autoselect,
                        })
                    })
                    .collect()
            }
            NativeManifest::HlsMedia(_) => Vec::new(),
            NativeManifest::Dash(dash) => {
                if !dash
                    .video_representations
                    .iter()
                    .any(|representation| representation.matches_key(video_key.as_str()))
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
                            key: representation.key.clone().into(),
                            label,
                            is_default: representation.is_default_audio,
                            is_autoselect: false,
                        }
                    })
                    .collect::<Vec<_>>();
                choices.sort_by_key(|choice| !choice.is_default);
                choices
            }
        }
    }

    /// List only subtitle renditions associated with the selected video representation.
    pub(crate) fn subtitle_choices(
        &self,
        video_key: &ManifestRepresentationKey,
    ) -> Vec<ManifestSubtitleChoice> {
        match &self.manifest {
            NativeManifest::HlsMaster(master) => {
                let group_id = master
                    .variants
                    .iter()
                    .zip(hls_variant_keys(&master.variants))
                    .find_map(|(variant, key)| {
                        (key.as_deref() == Some(video_key.as_str()))
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
                            key: key.into(),
                            language: rendition.language.clone(),
                            is_default: rendition.is_default,
                            is_autoselect: rendition.is_autoselect,
                            is_forced: rendition.is_forced,
                        })
                    })
                    .collect()
            }
            NativeManifest::HlsMedia(_) => Vec::new(),
            NativeManifest::Dash(dash) => {
                if !dash
                    .video_representations
                    .iter()
                    .any(|track| track.matches_key(video_key.as_str()))
                {
                    return Vec::new();
                }
                let mut choices = dash
                    .subtitle_representations
                    .iter()
                    .map(|track| ManifestSubtitleChoice {
                        key: track.key.clone().into(),
                        language: track.language.clone(),
                        is_default: track.is_default_subtitle,
                        is_autoselect: false,
                        is_forced: false,
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
        let preferred_index =
            Self::preferred_subtitle_choice_index_for_language(choices, preferred);
        preferred_index
            .or_else(|| Self::preferred_subtitle_choice_index_for_language(choices, "en"))
    }

    fn preferred_subtitle_choice_index_for_language(
        choices: &[ManifestSubtitleChoice],
        language: &str,
    ) -> Option<usize> {
        choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                choice
                    .language
                    .as_deref()
                    .is_some_and(|available| subtitle_language_matches(language, available))
            })
            .min_by_key(|(index, choice)| {
                (
                    choice.is_forced,
                    !choice.is_default,
                    !choice.is_autoselect,
                    *index,
                )
            })
            .map(|(index, _)| index)
    }

    /// Resolve the selected subtitle key using the same language preference as yt-dlp.
    #[must_use]
    pub(crate) fn preferred_subtitle_key(
        &self,
        video_key: &ManifestRepresentationKey,
        preferred_language: Option<&str>,
    ) -> Option<ManifestSubtitleKey> {
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
            .or_else(|| choices.iter().position(|choice| choice.is_autoselect))
            .unwrap_or(0)
    }

    /// Return the representation closest to the configured video-quality preference.
    pub(crate) fn preferred_source(
        &self,
        quality: &str,
        subtitle_language: Option<&str>,
    ) -> Option<ManifestSource> {
        let choices = self.choices();
        let index = preferred_choice_index(&choices, quality);
        let choice = choices.get(index)?;
        let audio_choices = self.audio_choices(&choice.key);
        let audio_index = Self::preferred_audio_choice_index(&audio_choices);
        let audio_key = audio_choices.get(audio_index).map(|audio| &audio.key);
        let subtitle_key = self.preferred_subtitle_key(&choice.key, subtitle_language);
        self.selected_source(&choice.key, audio_key, subtitle_key.as_ref())
    }

    /// Validate and capture the exact video and optional audio selections.
    pub(crate) fn selected_source(
        &self,
        representation_key: &ManifestRepresentationKey,
        audio_key: Option<&ManifestAudioKey>,
        subtitle_key: Option<&ManifestSubtitleKey>,
    ) -> Option<ManifestSource> {
        let kind =
            match &self.manifest {
                NativeManifest::HlsMaster(master) => {
                    hls_variant_keys(&master.variants)
                        .iter()
                        .any(|key| key.as_deref() == Some(representation_key.as_str()))
                        .then_some(())?;
                    ManifestKind::Hls
                }
                NativeManifest::HlsMedia(_) if representation_key.as_str() == "hls-media" => {
                    ManifestKind::Hls
                }
                NativeManifest::HlsMedia(_) => return None,
                NativeManifest::Dash(dash) => {
                    if !dash.video_representations.iter().any(|representation| {
                        representation.matches_key(representation_key.as_str())
                    }) {
                        return None;
                    }
                    ManifestKind::Dash
                }
            };
        if let Some(selected) = audio_key {
            let valid = match &self.manifest {
                NativeManifest::Dash(dash) => dash
                    .audio_representations
                    .iter()
                    .any(|track| track.matches_key(selected.as_str())),
                NativeManifest::HlsMaster(_) => self
                    .audio_choices(representation_key)
                    .iter()
                    .any(|choice| choice.key == *selected),
                NativeManifest::HlsMedia(_) => false,
            };
            if !valid {
                return None;
            }
        }
        if let Some(selected) = subtitle_key {
            let valid = match &self.manifest {
                NativeManifest::Dash(dash) => dash
                    .subtitle_representations
                    .iter()
                    .any(|track| track.matches_key(selected.as_str())),
                NativeManifest::HlsMaster(_) => self
                    .subtitle_choices(representation_key)
                    .iter()
                    .any(|choice| choice.key == *selected),
                NativeManifest::HlsMedia(_) => false,
            };
            if !valid {
                return None;
            }
        }
        let subtitle_language = subtitle_key
            .and_then(|selected| {
                self.subtitle_choices(representation_key)
                    .into_iter()
                    .find(|choice| choice.key.as_str() == selected.as_str())
            })
            .and_then(|choice| choice.language)
            .as_deref()
            .and_then(normalize_subtitle_language);
        // The preview already contains the complete media playlist for direct
        // HLS and the selected representation MIME type for DASH. HLS master
        // children are not fetched here, so keep their output conservative
        // rather than performing a second network lookup during Add.
        let output_extension = match &self.manifest {
            NativeManifest::HlsMaster(_) => None,
            NativeManifest::HlsMedia(plan) => {
                plan.output_container_extension().map(str::to_owned)
            }
            NativeManifest::Dash(dash) => dash
                .video_representations
                .iter()
                .find(|representation| representation.matches_key(representation_key.as_str()))
                .and_then(|representation| {
                    dash_output_container_extension(
                        representation.representation.mime_type.as_deref(),
                    )
                })
                .map(str::to_owned),
        };
        Some(ManifestSource {
            url: self.source_url.clone(),
            kind,
            representation_key: representation_key.clone(),
            output_extension,
            audio_key: audio_key.cloned(),
            subtitle_key: subtitle_key.cloned(),
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
            if source.representation_key.as_str() == "hls-media" =>
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
                                .filter(|rendition| rendition.group_id == group)
                                .find(|rendition| rendition.is_autoselect)
                        })
                        .or_else(|| {
                            master
                                .audio_renditions
                                .iter()
                                .find(|rendition| rendition.group_id == group)
                        })
                }),
            };
            Ok(ManifestDownloadPlan::HlsMaster(Box::new(
                HlsMasterDownloadPlan {
                    video_url: variant.url.clone(),
                    audio_url: selected_audio.and_then(|rendition| rendition.url.clone()),
                    subtitle_url: match source.subtitle_key.as_deref() {
                        Some(subtitle_key) => Some(
                            master
                                .subtitle_renditions
                                .iter()
                                .zip(hls_subtitle_rendition_keys(&master.subtitle_renditions))
                                .find_map(|(rendition, key)| {
                                    (key.as_deref() == Some(subtitle_key)
                                        && variant.subtitles_group.as_deref()
                                            == Some(rendition.group_id.as_str()))
                                    .then_some(rendition.url.clone())
                                })
                                .ok_or_else(|| {
                                    "The selected HLS subtitle rendition is no longer available"
                                        .to_string()
                                })?,
                        ),
                        None => None,
                    },
                    subtitle_language: source.subtitle_key.as_deref().and_then(|subtitle_key| {
                        master
                            .subtitle_renditions
                            .iter()
                            .zip(hls_subtitle_rendition_keys(&master.subtitle_renditions))
                            .find_map(|(rendition, key)| {
                                (key.as_deref() == Some(subtitle_key)
                                    && variant.subtitles_group.as_deref()
                                        == Some(rendition.group_id.as_str()))
                                .then_some(rendition.language.clone())
                                .flatten()
                            })
                    }),
                    inherited_variables: master.inherited_variables.clone(),
                    content_steering: master.content_steering.clone(),
                },
            )))
        }
        (NativeManifest::Dash(dash), ManifestKind::Dash) => {
            let video = crate::manifest::dash::plan_dash_representation(
                &dash,
                source.representation_key.as_str(),
            )
            .map_err(|error| error.to_string())?;
            let selected_audio = match source.audio_key.as_deref() {
                Some(audio_key) => Some(
                    dash.audio_representations
                        .iter()
                        .find(|track| track.matches_key(audio_key))
                        .ok_or_else(|| {
                            "The selected DASH audio representation is no longer available"
                                .to_string()
                        })?,
                ),
                None => dash.default_audio(),
            };
            let audio = selected_audio
                .map(|track| {
                    let plan_key = if source.representation_key.as_str().starts_with("dash-live-")
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
                    return Err(
                        "Native subtitles are not supported for live DASH presentations"
                            .to_string(),
                    );
                }
                Some(key) => {
                    let track = dash
                        .subtitle_representations
                        .iter()
                        .find(|track| track.matches_key(key))
                        .ok_or_else(|| {
                            "The selected DASH subtitle representation is no longer available"
                                .to_string()
                        })?;
                    Some(Box::new(
                        crate::manifest::dash::plan_dash_representation(&dash, &track.key)
                            .map_err(|error| error.to_string())?,
                    ))
                }
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

/// Content-derived identity for HLS tracks whose origin provides no Apple
/// stable IDs. The hash deliberately ignores the child host, port, and query
/// string so the identity survives CDN and signed-URL rotation. It does not
/// survive path-embedded session tokens: those rows degrade to the documented
/// "no longer available" fallback instead of picking a wrong track. Truly
/// identical tracks collide into one key and fail intake validation instead
/// of being silently disambiguated. Costs one in-memory SHA-256 per track:
/// no extra network traffic.
#[must_use]
fn derived_hls_track_key(prefix: &str, track_kind: &[u8], fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"grab-hls-content-track\0");
    hasher.update(track_kind);
    hasher.update([0]);
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    let fingerprint: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}{fingerprint}")
}

/// Variant content identity: child path plus the quality ladder. Bandwidth is
/// deliberately excluded — advertised `BANDWIDTH`/`AVERAGE-BANDWIDTH` values
/// can differ between fetches, and ladder steps almost always differ in path,
/// resolution, codecs, or frame rate. Rendition content identities drop
/// language for the same reason (`en-US` vs `en` rotates between fetches; the
/// group, name, and default flag separate language variants).
///
/// Like every content key, always `Some`: `None` arms on consumers are dead
/// but kept for the same `Option` plumbing as Apple IDs.
#[must_use]
fn hls_variant_content_key(variant: &HlsVariant) -> String {
    if let Some(group_id) = variant.video_rendition_group.as_deref() {
        let variant_identity = variant
            .stable_variant_id
            .as_deref()
            .or(variant.video_variant_path.as_deref())
            .unwrap_or_else(|| variant.url.path());
        let rendition_identity = variant
            .stable_video_rendition_id
            .as_deref()
            .or(variant.video_rendition_name.as_deref())
            .unwrap_or_default();
        let rendition_path = if variant.stable_video_rendition_id.is_some() {
            &b""[..]
        } else {
            variant.url.path().as_bytes()
        };
        return derived_hls_track_key(
            "hls-variant-",
            b"variant-video-rendition",
            &[
                variant_identity.as_bytes(),
                group_id.as_bytes(),
                rendition_identity.as_bytes(),
                rendition_path,
                &variant.width.unwrap_or_default().to_le_bytes(),
                &variant.height.unwrap_or_default().to_le_bytes(),
                &variant
                    .frame_rate
                    .unwrap_or_default()
                    .to_bits()
                    .to_le_bytes(),
                variant.codecs.as_deref().unwrap_or_default().as_bytes(),
                variant
                    .video_range
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes(),
                variant
                    .audio_group
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes(),
                variant
                    .subtitles_group
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes(),
            ],
        );
    }
    derived_hls_track_key(
        "hls-variant-",
        b"variant",
        &[
            variant.url.path().as_bytes(),
            &variant.width.unwrap_or_default().to_le_bytes(),
            &variant.height.unwrap_or_default().to_le_bytes(),
            &variant
                .frame_rate
                .unwrap_or_default()
                .to_bits()
                .to_le_bytes(),
            variant.codecs.as_deref().unwrap_or_default().as_bytes(),
            variant
                .video_range
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
            variant
                .audio_group
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
            variant
                .subtitles_group
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        ],
    )
}

/// Audio rendition content identity: group, label, default flag, and child
/// path. Deliberately omits language: origins may emit `en-US` or `en`
/// between fetches, so language variants are separated by group, name, and
/// default flag. Always `Some` (see `hls_variant_content_key`).
#[must_use]
fn hls_audio_rendition_content_key(rendition: &HlsAudioRendition) -> String {
    derived_hls_track_key(
        "hls-audio-",
        b"rendition",
        &[
            rendition.group_id.as_bytes(),
            rendition.name.as_bytes(),
            &[
                u8::from(rendition.is_default),
                u8::from(rendition.is_autoselect),
            ],
            rendition
                .assoc_language
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
            rendition
                .characteristics
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
            rendition.channels.as_deref().unwrap_or_default().as_bytes(),
            rendition
                .url
                .as_ref()
                .map_or(&b"<embedded>"[..], |url| url.path().as_bytes()),
        ],
    )
}

/// Subtitle rendition content identity: group, label, default flag, and
/// child path; language omitted as in `hls_audio_rendition_content_key`.
/// Always `Some` (see `hls_variant_content_key`).
#[must_use]
fn hls_subtitle_rendition_content_key(rendition: &HlsSubtitleRendition) -> String {
    derived_hls_track_key(
        "hls-subtitle-",
        b"subtitle",
        &[
            rendition.group_id.as_bytes(),
            rendition.name.as_bytes(),
            &[
                u8::from(rendition.is_default),
                u8::from(rendition.is_autoselect),
                u8::from(rendition.is_forced),
            ],
            rendition
                .assoc_language
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
            rendition
                .characteristics
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
            rendition.url.path().as_bytes(),
        ],
    )
}

#[must_use]
pub(crate) fn hls_audio_rendition_key(rendition: &HlsAudioRendition) -> Option<String> {
    Some(
        rendition
            .stable_rendition_id
            .as_deref()
            .map(|stable_id| stable_hls_track_key("hls-audio-", b"rendition", stable_id))
            .unwrap_or_else(|| hls_audio_rendition_content_key(rendition)),
    )
}

/// Build keys for subtitle renditions: Apple stable rendition IDs, else a
/// derived content identity.
#[must_use]
pub(crate) fn hls_subtitle_rendition_key(rendition: &HlsSubtitleRendition) -> Option<String> {
    Some(
        rendition
            .stable_rendition_id
            .as_deref()
            .map(|stable_id| stable_hls_track_key("hls-subtitle-", b"subtitle", stable_id))
            .unwrap_or_else(|| hls_subtitle_rendition_content_key(rendition)),
    )
}

#[must_use]
pub(crate) fn hls_subtitle_rendition_keys(
    renditions: &[HlsSubtitleRendition],
) -> Vec<Option<String>> {
    renditions.iter().map(hls_subtitle_rendition_key).collect()
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
    let preferred = preferred
        .split(['-', '_'])
        .next()
        .unwrap_or(preferred)
        .to_ascii_lowercase();
    let available = available
        .split(['-', '_'])
        .next()
        .unwrap_or(available)
        .to_ascii_lowercase();
    if preferred == available {
        return true;
    }
    const ALIASES: &[(&str, &str)] = &[
        ("en", "eng"),
        ("ar", "ara"),
        ("de", "deu"),
        ("de", "ger"),
        ("es", "spa"),
        ("fr", "fra"),
        ("fr", "fre"),
        ("hi", "hin"),
        ("id", "ind"),
        ("it", "ita"),
        ("ja", "jpn"),
        ("ko", "kor"),
        ("nl", "nld"),
        ("nl", "dut"),
        ("pl", "pol"),
        ("pt", "por"),
        ("ru", "rus"),
        ("tr", "tur"),
        ("vi", "vie"),
        ("zh", "zho"),
        ("zh", "chi"),
    ];
    ALIASES.iter().any(|(two, three)| {
        (preferred == *two && available == *three) || (preferred == *three && available == *two)
    })
}

/// Build keys for renditions: Apple stable rendition IDs, else derived content identities.
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
    if let Some(assoc_language) = rendition
        .assoc_language
        .as_deref()
        .filter(|language| !language.is_empty())
    {
        parts.push(format!(
            "{}: {}",
            gettext("Associated language"),
            short_label(assoc_language)
        ));
    }
    if let Some(channels) = rendition
        .channels
        .as_deref()
        .filter(|channels| !channels.is_empty())
    {
        parts.push(format!(
            "{}: {}",
            gettext("Channels"),
            short_label(channels)
        ));
    }
    if rendition.characteristics.as_deref().is_some_and(|value| {
        value
            .split(',')
            .any(|item| item.trim() == "public.accessibility.describes-video")
    }) {
        parts.push(gettext("Audio description"));
    }
    if rendition.url.is_none() {
        parts.push(gettext("Embedded audio"));
    }
    if rendition.is_default {
        parts.push(gettext("Default"));
    } else if rendition.is_autoselect {
        parts.push(gettext("Auto-selectable"));
    }
    if parts.is_empty() {
        gettext("Audio track")
    } else {
        parts.join(" · ")
    }
}

#[must_use]
pub(crate) fn hls_variant_key(variant: &HlsVariant) -> Option<String> {
    if variant.video_rendition_group.is_some() {
        if let (Some(variant_id), Some(rendition_id)) = (
            variant.stable_variant_id.as_deref(),
            variant.stable_video_rendition_id.as_deref(),
        ) {
            return Some(derived_hls_track_key(
                "hls-variant-",
                b"variant-video-rendition",
                &[variant_id.as_bytes(), rendition_id.as_bytes()],
            ));
        }
        return Some(hls_variant_content_key(variant));
    }
    Some(
        variant
            .stable_variant_id
            .as_deref()
            .map(|stable_id| stable_hls_track_key("hls-variant-", b"variant", stable_id))
            .unwrap_or_else(|| hls_variant_content_key(variant)),
    )
}

/// Build keys for variants: Apple stable variant IDs, else derived content identities.
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
    fn subtitle_preference_uses_english_fallback_but_not_manifest_default() {
        let choices = vec![
            ManifestSubtitleChoice {
                key: "fr".into(),
                language: Some("fr".to_string()),
                is_default: true,
                is_autoselect: true,
                is_forced: false,
            },
            ManifestSubtitleChoice {
                key: "en".into(),
                language: Some("en".to_string()),
                is_default: false,
                is_autoselect: false,
                is_forced: false,
            },
        ];

        assert_eq!(
            ManifestLookup::preferred_subtitle_choice_index(&choices, Some("de")),
            Some(1),
            "native selection follows yt-dlp: requested language, then English"
        );
        assert_eq!(
            ManifestLookup::preferred_subtitle_choice_index(&choices, Some("off")),
            None
        );
        assert_eq!(
            ManifestLookup::preferred_subtitle_choice_index(&choices, None),
            None
        );
    }

    #[test]
    fn subtitle_selection_prefers_regular_tracks_then_default_and_autoselect() {
        let choices = vec![
            ManifestSubtitleChoice {
                key: "forced".into(),
                language: Some("en".to_string()),
                is_default: true,
                is_autoselect: true,
                is_forced: true,
            },
            ManifestSubtitleChoice {
                key: "regular-auto".into(),
                language: Some("en".to_string()),
                is_default: false,
                is_autoselect: true,
                is_forced: false,
            },
            ManifestSubtitleChoice {
                key: "regular-default".into(),
                language: Some("en".to_string()),
                is_default: true,
                is_autoselect: true,
                is_forced: false,
            },
        ];
        assert_eq!(
            ManifestLookup::preferred_subtitle_choice_index(&choices, Some("en")),
            Some(2),
            "the regular rendition is preferred over a forced-only rendition"
        );
    }

    #[test]
    fn audio_selection_prefers_default_then_autoselect_then_first() {
        let choices = vec![
            ManifestAudioChoice {
                key: "first".into(),
                label: "First".to_string(),
                is_default: false,
                is_autoselect: false,
            },
            ManifestAudioChoice {
                key: "auto".into(),
                label: "Auto".to_string(),
                is_default: false,
                is_autoselect: true,
            },
        ];
        assert_eq!(ManifestLookup::preferred_audio_choice_index(&choices), 1);
        let mut default_choices = choices.clone();
        default_choices[0].is_default = true;
        assert_eq!(
            ManifestLookup::preferred_audio_choice_index(&default_choices),
            0
        );
    }

    #[test]
    fn hls_video_rendition_selection_resolves_the_selected_angle() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="angles",NAME="Main",URI="video/main.m3u8",STABLE-RENDITION-ID="main"
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="angles",NAME="Camera B",URI="video/camera-b.m3u8",STABLE-RENDITION-ID="camera-b"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1920x1080,VIDEO="angles",AUDIO="audio",STABLE-VARIANT-ID="variant-1080"
video/combined.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("master playlist with alternate video angles");
        let selected_key = master
            .variants
            .iter()
            .zip(hls_variant_keys(&master.variants))
            .find_map(|(variant, key)| {
                (variant.video_rendition_name.as_deref() == Some("Camera B"))
                    .then(|| key.map(ManifestRepresentationKey::from))
                    .flatten()
            })
            .expect("Camera B has a stable selection key");
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(Box::new(master)),
        };
        let choices = lookup.choices();
        assert_eq!(choices.len(), 2);
        assert!(
            choices
                .iter()
                .any(|choice| choice.label.starts_with("Camera B")),
            "the quality picker labels the selected camera angle"
        );
        let audio = lookup.audio_choices(&selected_key);
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].label, "en · English · Default");

        let source = lookup
            .selected_source(&selected_key, Some(&audio[0].key), None)
            .expect("the selected VIDEO rendition and audio track are valid");
        let plan = download_plan(lookup, &source).expect("selected angle download plan");
        let ManifestDownloadPlan::HlsMaster(plan) = plan else {
            panic!("expected an HLS master download plan");
        };
        assert_eq!(
            plan.video_url.as_str(),
            "https://cdn.example.test/video/camera-b.m3u8"
        );
        assert_eq!(
            plan.audio_url.expect("associated audio playlist").as_str(),
            "https://cdn.example.test/audio/en.m3u8"
        );
    }

    #[test]
    fn hls_subtitle_selection_uses_stable_rendition_ids_and_language_preference() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,URI="subs/en.m3u8",STABLE-RENDITION-ID="subtitle-en"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,SUBTITLES="subs",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        let video_key = ManifestRepresentationKey::from(
            hls_variant_keys(&master.variants)[0]
                .as_deref()
                .expect("stable video key"),
        );
        let subtitle_key = ManifestSubtitleKey::from(
            hls_subtitle_rendition_keys(&master.subtitle_renditions)[0]
                .as_deref()
                .expect("stable subtitle key"),
        );
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(Box::new(master)),
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
        let ManifestDownloadPlan::HlsMaster(plan) = plan else {
            panic!("expected an HLS master plan");
        };
        assert_eq!(
            plan.subtitle_url.expect("selected subtitle URL").as_str(),
            "https://cdn.example.test/subs/en.m3u8"
        );
        assert_eq!(plan.subtitle_language.as_deref(), Some("en"));
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
        let video_key = ManifestRepresentationKey::from(dash.video_representations[0].key.clone());
        let subtitle_key = ManifestSubtitleKey::from(dash.subtitle_representations[0].key.clone());
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
        assert_eq!(source.output_extension.as_deref(), Some("mp4"));
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

        let invalid_lookup = ManifestLookup {
            source_url: "https://cdn.example.test/manifest.mpd".to_string(),
            manifest: NativeManifest::Dash(
                crate::manifest::dash::parse_dash_manifest(
                    "https://cdn.example.test/manifest.mpd",
                    body,
                )
                .expect("valid static DASH presentation"),
            ),
        };
        let mut invalid_source = source;
        invalid_source.subtitle_key = Some(video_key.as_str().into());
        assert!(
            download_plan(invalid_lookup, &invalid_source).is_err(),
            "a video representation key must not be accepted as a subtitle key"
        );
    }

    #[test]
    fn dash_output_container_uses_the_selected_representation_mime_type() {
        assert_eq!(dash_output_container_extension(Some("video/mp2t")), Some("ts"));
        assert_eq!(dash_output_container_extension(Some("video/mp4")), Some("mp4"));
        assert_eq!(
            dash_output_container_extension(Some("application/mp4; codecs=avc1")),
            Some("mp4")
        );
        assert_eq!(dash_output_container_extension(Some("video/webm")), None);
        assert_eq!(dash_output_container_extension(None), None);
    }

    #[test]
    fn direct_hls_media_source_carries_its_preview_container() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:6
#EXTINF:6,
segment.ts
#EXT-X-ENDLIST
"#;
        let source_url = "https://cdn.example.test/stream.m3u8";
        let plan = crate::manifest::plan_hls_media_playlist(source_url, body)
            .expect("valid direct HLS media playlist");
        let lookup = ManifestLookup {
            source_url: source_url.to_string(),
            manifest: NativeManifest::HlsMedia(Box::new(plan)),
        };
        let source = lookup
            .selected_source(&ManifestRepresentationKey::from("hls-media"), None, None)
            .expect("direct HLS source");
        assert_eq!(source.output_extension.as_deref(), Some("ts"));
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
        let video_key = ManifestRepresentationKey::from(
            hls_variant_keys(&master.variants)[0]
                .as_deref()
                .expect("stable video key"),
        );
        let audio_key = ManifestAudioKey::from(
            hls_audio_rendition_keys(&master.audio_renditions)[0]
                .as_deref()
                .expect("stable audio key"),
        );
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(Box::new(master)),
        };
        let source = lookup
            .selected_source(&video_key, Some(&audio_key), None)
            .expect("valid stable track selection");
        assert_eq!(
            source.output_extension,
            None,
            "HLS master children are not fetched during preview; use the safe Matroska default"
        );

        let plan = download_plan(lookup, &source).expect("valid selected download plan");
        let ManifestDownloadPlan::HlsMaster(plan) = plan else {
            panic!("expected an HLS master plan");
        };
        assert_eq!(
            plan.video_url.as_str(),
            "https://cdn.example.test/video/720.m3u8"
        );
        assert_eq!(
            plan.audio_url
                .expect("selected audio rendition URL")
                .as_str(),
            "https://cdn.example.test/audio/en.m3u8"
        );
        assert!(plan.subtitle_url.is_none());
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
            manifest: NativeManifest::HlsMaster(Box::new(master)),
        };
        let source = ManifestSource {
            url: lookup.source_url.clone(),
            kind: ManifestKind::Hls,
            representation_key: "hls-variant-00000000000000000000000000000000".into(),
            output_extension: None,
            audio_key: None,
            subtitle_key: None,
            subtitle_language: None,
        };

        assert!(download_plan(lookup, &source).is_err());
    }
}
