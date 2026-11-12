use super::*;
use crate::extension_server::handoff_cookie_hash;
use crate::manifest_plan::{
    hls_audio_rendition_key, hls_audio_rendition_keys, hls_variant_key, hls_variant_keys,
};

#[test]
fn signed_master_query_is_inherited_only_by_same_origin_renditions() {
    let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",DEFAULT=YES,AUTOSELECT=YES,URI="audio.m3u8"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",URI="https://other.example.test/subs.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO="aud",SUBTITLES="subs"
video.m3u8
"#;
    let mut master = crate::manifest::parse_hls_master_playlist(
        "https://cdn.example.test/master.m3u8?t=token&s=signature&e=86400",
        body,
    )
    .expect("HLS master");
    inherit_hls_master_query_parameters(
        &Url::parse("https://cdn.example.test/master.m3u8?t=token&s=signature&e=86400")
            .expect("signed master URL"),
        &mut master,
    );
    assert!(master.variants[0].url.query().is_some_and(|query| {
        query.contains("t=token") && query.contains("s=signature") && query.contains("e=86400")
    }));
    assert!(
        master.audio_renditions[0]
            .url
            .as_ref()
            .is_some_and(|url| { url.query().is_some_and(|query| query.contains("t=token")) })
    );
    assert_eq!(master.subtitle_renditions[0].url.query(), None);
}

#[test]
fn native_manifest_gate_accepts_explicit_format_query_hints() {
    assert!(supports("https://cdn.example.test/stream?format=m3u8"));
    assert!(supports(
        "https://cdn.example.test/stream?type=application/dash%2Bxml"
    ));
    assert!(!supports("https://cdn.example.test/watch?v=video"));
    assert!(!supports("http://cdn.example.test/stream?format=m3u8"));
}

#[test]
fn native_manifest_gate_accepts_only_named_extensionless_endpoints() {
    assert!(supports("https://cdn.example.test/master"));
    assert!(supports("https://cdn.example.test/live/playlist?token=x"));
    assert!(supports("https://cdn.example.test/manifest"));
    assert!(!supports("https://cdn.example.test/watch"));
    assert!(!supports("https://cdn.example.test/index"));
}

#[test]
fn endpoint_only_preview_candidates_skip_browser_cookie_export() {
    let browser = "firefox";
    let endpoint = Url::parse("https://cdn.example.test/playlist").unwrap();
    let explicit_hint = Url::parse("https://cdn.example.test/stream?format=m3u8").unwrap();

    assert_eq!(
        preview_cookie_browser(&endpoint, browser, ManifestTransportPolicy::HttpsOnly),
        "",
        "endpoint-only URLs are sniffed without decrypting browser cookies"
    );
    assert_eq!(
        preview_cookie_browser(&explicit_hint, browser, ManifestTransportPolicy::HttpsOnly),
        browser,
        "explicit manifest hints retain authenticated preview support"
    );
}

#[tokio::test]
async fn youtube_playlist_page_skips_native_preview_before_cookie_export() {
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        lookup(
            "https://www.youtube.com/playlist?list=PL_test",
            "firefox",
            None,
        ),
    )
    .await
    .expect("YouTube playlist URLs must be rejected before browser-cookie export");

    assert!(
        result
            .expect("ordinary YouTube playlist pages do not enter native intake")
            .is_none(),
        "the endpoint-name heuristic must not route YouTube pages to native intake"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn extensionless_master_endpoint_is_sniffed_as_hls() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind extensionless manifest fixture");
    let address = listener.local_addr().expect("fixture address");
    let body = concat!(
        "#EXTM3U\n",
        "#EXT-X-VERSION:3\n",
        "#EXT-X-TARGETDURATION:4\n",
        "#EXT-X-MEDIA-SEQUENCE:0\n",
        "#EXTINF:4.0,\n",
        "segment0.ts\n",
        "#EXT-X-ENDLIST\n"
    );
    let response_body = body.to_string();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept manifest request");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = socket
                .read(&mut chunk)
                .await
                .expect("read manifest request");
            assert_ne!(read, 0, "request ended before HTTP headers");
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            assert!(request.len() < 16 * 1024, "request headers stay bounded");
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write manifest response");
    });

    let url = format!("http://{address}/master");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build local manifest client");
    let lookup = lookup_with_http_fixture_client(&client, &url)
        .await
        .expect("extensionless manifest lookup")
        .expect("HLS signature should select native parsing");
    server.await.expect("fixture server task");

    let NativeManifest::HlsMedia(plan) = lookup.manifest else {
        panic!("extensionless HLS body should produce a media playlist");
    };
    assert_eq!(plan.resources.len(), 1);
    assert_eq!(
        plan.resources[0].url.as_str(),
        format!("https://{address}/segment0.ts")
    );
}

#[test]
fn manifest_body_sniffing_requires_a_manifest_signature() {
    assert_eq!(
        sniff_manifest_kind(b"\xef\xbb\xbf  #EXTM3U\n#EXT-X-ENDLIST"),
        Some(crate::media_types::ManifestKind::Hls)
    );
    assert_eq!(
        sniff_manifest_kind(
            br#"<?xml version="1.0" encoding="UTF-8"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011"/>"#
        ),
        Some(crate::media_types::ManifestKind::Dash)
    );
    assert_eq!(
        sniff_manifest_kind(
            br#"<!-- generated MPD --><!-- another comment --><?xml version="1.0"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011"/>"#
        ),
        Some(crate::media_types::ManifestKind::Dash)
    );
    assert_eq!(
        sniff_manifest_kind(b"<!doctype html><html><body>video</body></html>"),
        None
    );
    assert_eq!(sniff_manifest_kind(b"not a manifest"), None);
}

#[test]
fn manifest_preview_http_statuses_fall_back_without_retry() {
    for status in [
        reqwest::StatusCode::REQUEST_TIMEOUT,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        reqwest::StatusCode::NOT_FOUND,
        reqwest::StatusCode::FORBIDDEN,
    ] {
        assert!(
            matches!(
                manifest_status_error(status, ManifestFetchMode::Preview),
                ManifestIntakeError::Unsupported(_)
            ),
            "preview HTTP {status} should yield immediately to yt-dlp"
        );
    }
}

#[test]
fn manifest_download_retries_only_transient_http_statuses() {
    for status in [
        reqwest::StatusCode::REQUEST_TIMEOUT,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
    ] {
        assert!(
            matches!(
                manifest_status_error(status, ManifestFetchMode::Download),
                ManifestIntakeError::Retryable(_)
            ),
            "download HTTP {status} should retain transient retries"
        );
    }
    for status in [
        reqwest::StatusCode::NOT_FOUND,
        reqwest::StatusCode::FORBIDDEN,
    ] {
        assert!(matches!(
            manifest_status_error(status, ManifestFetchMode::Download),
            ManifestIntakeError::Unsupported(_)
        ));
    }
}

#[test]
fn manifest_transport_retries_only_timeouts_and_connection_resets() {
    let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
    let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
    assert!(has_connection_reset_cause(&reset));
    assert!(!has_connection_reset_cause(&refused));
    assert!(!has_connection_reset_cause(&std::io::Error::other(
        "DNS resolution was refused by the guarded resolver"
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn manifest_preview_budget_cancels_a_slow_server_request() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind slow manifest fixture");
    let port = listener
        .local_addr()
        .expect("read slow manifest fixture address")
        .port();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept manifest request");
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(socket);
    });
    let url =
        Url::parse(&format!("http://127.0.0.1:{port}/master.m3u8")).expect("fixture manifest URL");
    let started = tokio::time::Instant::now();

    let result = fetch_bounded_preview_manifest_with_limits(
        &reqwest::Client::new(),
        url,
        ManifestTransportPolicy::HttpFixture,
        Duration::from_millis(20),
        Duration::from_secs(1),
        1,
    )
    .await;

    assert!(
        matches!(
            &result,
            Err(ManifestIntakeError::Unsupported(message))
                if message.contains("20 ms preview budget")
        ),
        "slow preview should yield to the extractor fallback: {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "preview should stop at its deadline instead of waiting for the request timeout"
    );
    server.await.expect("slow manifest fixture task");
}

#[tokio::test(flavor = "current_thread")]
async fn manifest_transport_failure_does_not_disclose_signed_url() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve a local port");
    let port = listener.local_addr().expect("read reserved port").port();
    drop(listener);
    let url = format!("http://127.0.0.1:{port}/master.m3u8?sig=secret-token");
    let error = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(1))
        .send()
        .await
        .expect_err("the reserved port is no longer listening");

    let classified = manifest_request_error("Could not retrieve the stream manifest", error);
    let message = classified.to_string();
    assert!(matches!(classified, ManifestIntakeError::Unsupported(_)));
    assert!(message.contains("Could not retrieve the stream manifest"));
    assert!(!message.contains("secret-token"));
}

#[test]
fn preferred_source_selects_the_configured_hls_representation() {
    let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720,STABLE-VARIANT-ID="fixture-variant-1"
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="fixture-variant-2"
high.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3500000,RESOLUTION=2560x1440,STABLE-VARIANT-ID="fixture-variant-3"
highest.m3u8
"#;
    let master =
        crate::manifest::parse_hls_master_playlist("https://cdn.example.test/master.m3u8", body)
            .expect("valid HLS master playlist");
    let lookup = ManifestLookup {
        source_url: "https://cdn.example.test/master.m3u8".to_string(),
        manifest: NativeManifest::HlsMaster(Box::new(master)),
    };

    let preferred = lookup
        .preferred_source("1080p", None)
        .expect("1080p HLS representation");
    let best = lookup
        .preferred_source("best", None)
        .expect("best HLS representation");

    let choices = lookup.choices();
    let preferred_choice = choices
        .iter()
        .find(|choice| choice.key == preferred.representation_key)
        .expect("preferred choice exists");
    let best_choice = choices
        .iter()
        .find(|choice| choice.key == best.representation_key)
        .expect("best choice exists");
    assert_eq!(preferred_choice.height, Some(1080));
    assert_eq!(best_choice.height, Some(1440));
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.height)
            .collect::<Vec<_>>(),
        vec![Some(1440), Some(1080), Some(720)]
    );
}

#[test]
fn choices_sort_by_height_then_bandwidth_and_keep_ties_stable() {
    let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="fixture-variant-4"
first.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720,STABLE-VARIANT-ID="fixture-variant-5"
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="fixture-variant-6"
higher-bandwidth.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="fixture-variant-7"
second.m3u8
"#;
    let master =
        crate::manifest::parse_hls_master_playlist("https://cdn.example.test/master.m3u8", body)
            .expect("valid HLS master playlist");
    let original_keys = hls_variant_keys(&master.variants);
    let lookup = ManifestLookup {
        source_url: "https://cdn.example.test/master.m3u8".to_string(),
        manifest: NativeManifest::HlsMaster(Box::new(master)),
    };

    let choices = lookup.choices();
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.height)
            .collect::<Vec<_>>(),
        vec![Some(1080), Some(1080), Some(1080), Some(720)],
        "choices should be ordered by descending height"
    );
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.key.as_str())
            .collect::<Vec<_>>(),
        vec![
            original_keys[2].as_deref().expect("stable variant key"),
            original_keys[0].as_deref().expect("stable variant key"),
            original_keys[3].as_deref().expect("stable variant key"),
            original_keys[1].as_deref().expect("stable variant key"),
        ],
        "bandwidth should break height ties, and equal ties should keep manifest order"
    );
}

#[test]
fn preferred_choice_index_matches_configured_height_and_best_uses_highest() {
    let choices = [
        ManifestChoice {
            key: "720".into(),
            label: "720p".to_string(),
            height: Some(720),
            bandwidth: Some(1_000_000),
        },
        ManifestChoice {
            key: "1080".into(),
            label: "1080p".to_string(),
            height: Some(1080),
            bandwidth: Some(2_500_000),
        },
        ManifestChoice {
            key: "1440".into(),
            label: "1440p".to_string(),
            height: Some(1440),
            bandwidth: Some(4_000_000),
        },
    ];

    assert_eq!(preferred_choice_index(&choices, "1080p"), 1);
    assert_eq!(preferred_choice_index(&choices, "best"), 2);
    assert_eq!(preferred_choice_index(&choices, "720p"), 0);
}

#[test]
fn hls_audio_choices_are_scoped_to_variant_and_preselect_default() {
    let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-en",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,STABLE-RENDITION-ID="fixture-rendition-1",URI="audio/en.m3u8?sig=old"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-en",NAME="French",LANGUAGE="fr",DEFAULT=NO,AUTOSELECT=YES,STABLE-RENDITION-ID="fixture-rendition-2",URI="audio/fr.m3u8?sig=old"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-es",NAME="Spanish",LANGUAGE="es",DEFAULT=YES,AUTOSELECT=YES,STABLE-RENDITION-ID="fixture-rendition-3",URI="audio/es.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,AUDIO="audio-en",STABLE-VARIANT-ID="fixture-variant-8"
video/en.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720,AUDIO="audio-es",STABLE-VARIANT-ID="fixture-variant-9"
video/es.m3u8
"#;
    let master =
        crate::manifest::parse_hls_master_playlist("https://cdn.example.test/master.m3u8", body)
            .expect("valid HLS master playlist");
    let video_keys = hls_variant_keys(&master.variants);
    let lookup = ManifestLookup {
        source_url: "https://cdn.example.test/master.m3u8".to_string(),
        manifest: NativeManifest::HlsMaster(Box::new(master)),
    };

    let english_video_key = crate::media_types::ManifestRepresentationKey::from(
        video_keys[0].as_deref().expect("stable English video key"),
    );
    let spanish_video_key = crate::media_types::ManifestRepresentationKey::from(
        video_keys[1].as_deref().expect("stable Spanish video key"),
    );
    let english_choices = lookup.audio_choices(&english_video_key);
    assert_eq!(english_choices.len(), 2);
    assert!(english_choices[0].is_default);
    assert!(
        english_choices
            .iter()
            .any(|choice| choice.label.contains("French"))
    );
    assert_eq!(lookup.audio_choices(&spanish_video_key).len(), 1);

    let default_source = lookup
        .preferred_source("1080p", None)
        .expect("preferred HLS video and default audio");
    assert_eq!(default_source.representation_key, english_video_key);
    assert_eq!(
        default_source.audio_key.as_deref(),
        Some(english_choices[0].key.as_str())
    );

    let selected = lookup
        .selected_source(&english_video_key, Some(&english_choices[1].key), None)
        .expect("selected alternate audio rendition");
    assert_eq!(
        selected.audio_key.as_deref(),
        Some(english_choices[1].key.as_str())
    );
    assert!(
        lookup
            .selected_source(&spanish_video_key, Some(&english_choices[1].key), None)
            .is_none(),
        "an audio rendition must not be selectable from another variant's group"
    );
}

#[test]
fn hls_audio_key_uses_stable_rendition_id_across_url_rotation() {
    let rendition = |url: &str| crate::manifest::HlsAudioRendition {
        url: Some(Url::parse(url).expect("valid audio rendition URL")),
        group_id: "audio-main".to_string(),
        name: "English".to_string(),
        language: Some("en".to_string()),
        assoc_language: None,
        is_default: true,
        is_autoselect: true,
        characteristics: None,
        channels: None,
        stable_rendition_id: Some("rendition-en".to_string()),
    };
    let original = rendition("https://cdn.example.test/audio/en.m3u8?sig=old");
    let refreshed = rendition("https://cdn.example.test/audio/en-new.m3u8?sig=new");
    let mut missing_id = rendition("https://cdn.example.test/audio/no-id.m3u8");
    missing_id.stable_rendition_id = None;

    assert_eq!(
        hls_audio_rendition_key(&original),
        hls_audio_rendition_key(&refreshed)
    );
    assert!(
        hls_audio_rendition_key(&missing_id).is_some(),
        "renditions without Apple stable IDs derive a content key"
    );
}

#[test]
fn hls_stable_rendition_key_survives_url_rotation() {
    let rendition = |url: &str| crate::manifest::HlsAudioRendition {
        url: Some(Url::parse(url).expect("valid audio rendition URL")),
        group_id: "audio-main".to_string(),
        name: "English".to_string(),
        language: Some("en".to_string()),
        assoc_language: None,
        is_default: true,
        is_autoselect: true,
        characteristics: None,
        channels: None,
        stable_rendition_id: Some("rendition-en".to_string()),
    };
    let original = rendition("https://cdn.example.test/audio/en-old.m3u8?sig=old");
    let refreshed = rendition("https://cdn.example.test/audio/en-new.m3u8?sig=new");

    assert_eq!(
        hls_audio_rendition_key(&original),
        hls_audio_rendition_key(&refreshed),
        "stable rendition IDs survive URL and signature rotation"
    );
}

#[test]
fn dash_audio_choices_include_every_track_and_pin_the_main_role() {
    let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="5" media="video-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720" codecs="avc1.64001f"/>
    </AdaptationSet>
    <AdaptationSet id="audio-en" contentType="audio" mimeType="audio/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate duration="5" media="audio-en-$Number$.m4s"/>
      <Representation id="a-en" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
    <AdaptationSet id="audio-fr" contentType="audio" mimeType="audio/mp4" lang="fr">
      <SegmentTemplate duration="5" media="audio-fr-$Number$.m4s"/>
      <Representation id="a-fr" bandwidth="192000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let dash =
        crate::manifest::dash::parse_dash_manifest("https://cdn.example.test/manifest.mpd", body)
            .expect("valid static DASH presentation");
    let video_key = crate::media_types::ManifestRepresentationKey::from(
        dash.video_representations[0].key.clone(),
    );
    let main_audio_key = crate::media_types::ManifestAudioKey::from(
        dash.default_audio()
            .expect("DASH main audio representation")
            .key
            .clone(),
    );
    let lookup = ManifestLookup {
        source_url: "https://cdn.example.test/manifest.mpd".to_string(),
        manifest: NativeManifest::Dash(dash),
    };

    let choices = lookup.audio_choices(&video_key);
    assert_eq!(choices.len(), 2);
    assert_eq!(choices[0].key, main_audio_key);
    assert!(choices[0].is_default);
    assert!(choices.iter().any(|choice| choice.label.contains("fr")));

    let selected = lookup
        .selected_source(&video_key, Some(&choices[1].key), None)
        .expect("select the alternate DASH audio representation");
    assert_eq!(selected.audio_key.as_deref(), Some(choices[1].key.as_str()));
    let preferred = lookup
        .preferred_source("720p", None)
        .expect("default DASH audio is selected automatically");
    assert_eq!(
        preferred.audio_key.as_deref(),
        Some(main_audio_key.as_str())
    );
}

#[test]
fn manifest_source_audio_key_is_optional_and_validated() {
    let legacy: crate::media_types::ManifestSource = serde_json::from_value(serde_json::json!({
        "url": "https://cdn.example.test/master.m3u8",
        "kind": "Hls",
        "representation_key": "hls-media"
    }))
    .expect("legacy manifest source without an audio key remains compatible");
    assert_eq!(legacy.audio_key, None);
    assert_eq!(legacy.subtitle_key, None);
    assert_eq!(legacy.subtitle_language, None);

    // Existing queue files store these identities as plain JSON strings.
    let legacy_with_tracks: crate::media_types::ManifestSource =
        serde_json::from_value(serde_json::json!({
            "url": "https://cdn.example.test/master.m3u8",
            "kind": "Hls",
            "representation_key": "hls-variant-0123456789abcdef0123456789abcdef",
            "audio_key": "hls-audio-0123456789abcdef0123456789abcdef",
            "subtitle_key": "hls-subtitle-0123456789abcdef0123456789abcdef",
            "subtitle_language": "en"
        }))
        .expect("legacy string keys remain compatible with typed identities");
    assert_eq!(
        legacy_with_tracks.representation_key.as_str(),
        "hls-variant-0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        legacy_with_tracks.audio_key.as_deref(),
        Some("hls-audio-0123456789abcdef0123456789abcdef")
    );
    assert_eq!(
        legacy_with_tracks.subtitle_key.as_deref(),
        Some("hls-subtitle-0123456789abcdef0123456789abcdef")
    );
    let encoded =
        serde_json::to_value(&legacy_with_tracks).expect("serialize typed manifest identities");
    assert_eq!(
        encoded["representation_key"],
        "hls-variant-0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        encoded["audio_key"],
        "hls-audio-0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        encoded["subtitle_key"],
        "hls-subtitle-0123456789abcdef0123456789abcdef"
    );

    let mut preview_source = legacy_with_tracks.clone();
    preview_source.output_extension = Some("ts".to_string());
    let preview_encoded =
        serde_json::to_value(&preview_source).expect("serialize preview metadata");
    assert!(
        preview_encoded.get("output_extension").is_none(),
        "preview-only format hints must not become stale persisted metadata"
    );
    let restored_preview: crate::media_types::ManifestSource =
        serde_json::from_value(preview_encoded).expect("restore persisted source");
    assert_eq!(restored_preview.output_extension, None);

    let mut source = crate::media_types::ManifestSource {
        url: "https://cdn.example.test/master.m3u8".to_string(),
        kind: crate::media_types::ManifestKind::Hls,
        representation_key: "hls-variant-0123456789abcdef0123456789abcdef".into(),
        output_extension: None,
        audio_key: Some("hls-audio-0123456789abcdef0123456789abcdef".into()),
        subtitle_key: None,
        subtitle_language: None,
    };
    assert!(valid_source(&source));
    source.output_extension = Some("../unexpected".to_string());
    assert!(!valid_source(&source));
    source.output_extension = None;
    assert!(valid_source(&source));
    source.url = "https://cdn.example.test/master".to_string();
    assert!(
        valid_source(&source),
        "extensionless endpoint kind is carried by the persisted source"
    );
    source.url = "https://cdn.example.test/master.m3u8".to_string();
    source.audio_key = Some("not-a-stable-audio-key".into());
    assert!(!valid_source(&source));
    source.audio_key = None;
    source.subtitle_key = Some("hls-audio-0123456789abcdef0123456789abcdef".into());
    assert!(!valid_source(&source));
    source.subtitle_key = Some("hls-subtitle-0123456789abcdef0123456789abcdef".into());
    assert!(valid_source(&source));
    source.subtitle_key = None;
    source.representation_key = format!("hls-variant-{}", "0".repeat(64)).into();
    assert!(
        valid_source(&source),
        "legacy queue keys remain restorable so the runner can fall back to yt-dlp"
    );
    source.representation_key = "hls-media".into();
    source.audio_key = None;
    assert!(valid_source(&source));
    source.subtitle_key = Some("hls-subtitle-0123456789abcdef0123456789abcdef".into());
    assert!(!valid_source(&source));
}

#[test]
fn accepts_only_https_hls_and_dash_manifest_paths() {
    assert!(supports("https://cdn.example.test/master.m3u8?token=abc"));
    assert!(supports("https://cdn.example.test/path/manifest.MPD"));
    assert!(!supports("http://cdn.example.test/master.m3u8"));
    assert!(!supports("https://cdn.example.test/video.mp4"));
    assert!(!supports("https://cdn.example.test/manifest.m3u"));
    assert!(!supports("https://cdn.example.test/manifest.pls"));
    assert!(!supports("https://cdn.example.test/manifest.m3u8/"));
    assert!(!supports(
        "https://user:secret@cdn.example.test/manifest.mpd"
    ));
    assert!(!supports("https://cdn.example.test/path/master.m3u8.exe"));
}

#[test]
fn hls_variant_keys_prefer_stable_ids_over_content_identities() {
    let variant = |url: &str, stable_id: &str| HlsVariant {
        url: Url::parse(url).expect("valid variant URL"),
        bandwidth: 2_000_000,
        average_bandwidth: Some(1_800_000),
        width: Some(1920),
        height: Some(1080),
        codecs: Some("avc1.640028".to_string()),
        video_range: None,
        frame_rate: Some(30.0),
        audio_group: None,
        subtitles_group: None,
        video_rendition_group: None,
        video_variant_path: None,
        video_variant_url: None,
        video_rendition_name: None,
        stable_video_rendition_id: None,
        stable_variant_id: Some(stable_id.to_string()),
    };
    let original = variant(
        "https://cdn.example.test/video/old.m3u8?sig=old",
        "variant-1080",
    );
    let refreshed = variant(
        "https://cdn.example.test/video/new.m3u8?sig=new",
        "variant-1080",
    );
    let different = variant(
        "https://cdn.example.test/video/720.m3u8?sig=new",
        "variant-720",
    );
    let mut missing_id = original.clone();
    missing_id.stable_variant_id = None;
    let mut rotated_host = missing_id.clone();
    rotated_host.url =
        Url::parse("https://mirror.example.test/video/old.m3u8?sig=new").expect("valid URL");

    let derived = hls_variant_key(&missing_id)
        .expect("variants without Apple stable IDs keep a content-derived key");
    assert_eq!(hls_variant_key(&rotated_host), Some(derived));
    assert_eq!(hls_variant_key(&original), hls_variant_key(&refreshed));
    assert_ne!(hls_variant_key(&original), hls_variant_key(&different));
    assert_eq!(hls_variant_keys(&[original, refreshed, different]).len(), 3);
}

#[test]
fn hls_variant_content_keys_survive_host_and_signature_rotation() {
    // Twitter/X amplify-style master: no STABLE-VARIANT-ID at all.
    let variant = |url: &str, height: u64, width: u64, bandwidth: u64| HlsVariant {
        url: Url::parse(url).expect("valid variant URL"),
        bandwidth,
        average_bandwidth: Some(bandwidth * 2 / 3),
        width: Some(width),
        height: Some(height),
        codecs: Some("mp4a.40.2,avc1.4D401F".to_string()),
        video_range: None,
        frame_rate: Some(30.0),
        audio_group: Some(format!("audio-{bandwidth}")),
        subtitles_group: None,
        video_rendition_group: None,
        video_variant_path: None,
        video_variant_url: None,
        video_rendition_name: None,
        stable_video_rendition_id: None,
        stable_variant_id: None,
    };
    let original = variant(
        "https://video.twimg.com/amplify_video/6/pl/avc1/854x480/abc.m3u8",
        480,
        854,
        1_630_685,
    );
    let refreshed = variant(
        "https://video10.twimg.com/amplify_video/6/pl/avc1/854x480/abc.m3u8?sig=rotated",
        480,
        854,
        1_630_685,
    );
    let other = variant(
        "https://video.twimg.com/amplify_video/6/pl/avc1/640x360/xyz.m3u8",
        360,
        640,
        817_916,
    );

    assert_eq!(
        hls_variant_key(&original),
        hls_variant_key(&refreshed),
        "host and signed-query rotation must not change the identity"
    );
    assert_ne!(
        hls_variant_key(&original),
        hls_variant_key(&other),
        "different tracks must not share a content identity"
    );
}

#[test]
fn hls_video_rendition_keys_combine_stable_variant_and_rendition_ids() {
    let make_master = |base: &str, variant_uri: &str, main_uri: &str, camera_uri: &str| {
        let body = format!(
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"angles\",NAME=\"Main\",URI=\"{main_uri}\",STABLE-RENDITION-ID=\"main\"\n#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"angles\",NAME=\"Camera B\",URI=\"{camera_uri}\",STABLE-RENDITION-ID=\"camera-b\"\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1920x1080,VIDEO=\"angles\",STABLE-VARIANT-ID=\"variant-1080\"\n{variant_uri}\n"
        );
        crate::manifest::parse_hls_master_playlist(base, body.as_bytes())
            .expect("valid video rendition master")
    };
    let original = make_master(
        "https://cdn.example.test/master.m3u8",
        "video/combined.m3u8",
        "video/main.m3u8",
        "video/camera-b.m3u8",
    );
    let refreshed = make_master(
        "https://mirror.example.test/master.m3u8",
        "rotated/combined.m3u8",
        "rotated/main.m3u8",
        "rotated/camera-b.m3u8",
    );
    let original_keys = hls_variant_keys(&original.variants);
    let refreshed_keys = hls_variant_keys(&refreshed.variants);
    assert_eq!(original_keys, refreshed_keys);
    assert_ne!(original_keys[0], original_keys[1]);
}

#[test]
fn hls_video_rendition_content_keys_include_parent_variant_and_audio_group() {
    let variant = |parent_path: &str, audio_group: &str| HlsVariant {
        url: Url::parse("https://cdn.example.test/angles/main.m3u8").expect("valid angle URL"),
        bandwidth: 2_000_000,
        average_bandwidth: Some(1_800_000),
        width: Some(1920),
        height: Some(1080),
        codecs: Some("avc1.640028".to_string()),
        video_range: None,
        frame_rate: Some(30.0),
        audio_group: Some(audio_group.to_string()),
        subtitles_group: None,
        video_rendition_group: Some("angles".to_string()),
        video_variant_path: Some(parent_path.to_string()),
        video_variant_url: None,
        video_rendition_name: Some("Main".to_string()),
        stable_video_rendition_id: None,
        stable_variant_id: None,
    };
    let first = variant("/video/first.m3u8", "audio-en");
    let other_variant = variant("/video/second.m3u8", "audio-en");
    let other_audio = variant("/video/first.m3u8", "audio-fr");
    assert_ne!(hls_variant_key(&first), hls_variant_key(&other_variant));
    assert_ne!(hls_variant_key(&first), hls_variant_key(&other_audio));
}

#[test]
fn hls_stable_variant_key_survives_resource_rotation() {
    let variant = |url: &str| HlsVariant {
        url: Url::parse(url).expect("valid variant URL"),
        bandwidth: 2_000_000,
        average_bandwidth: Some(1_800_000),
        width: Some(1920),
        height: Some(1080),
        codecs: Some("avc1.640028".to_string()),
        video_range: None,
        frame_rate: Some(30.0),
        audio_group: Some("audio-main".to_string()),
        subtitles_group: None,
        video_rendition_group: None,
        video_variant_path: None,
        video_variant_url: None,
        video_rendition_name: None,
        stable_video_rendition_id: None,
        stable_variant_id: Some("variant-1080".to_string()),
    };
    let original = variant("https://cdn.example.test/video/old.m3u8?sig=old");
    let refreshed = variant("https://cdn.example.test/video/new.m3u8?sig=new");

    assert_eq!(
        hls_variant_key(&original),
        hls_variant_key(&refreshed),
        "stable variant IDs survive resource and signature rotation"
    );
}

#[test]
fn selected_source_rejects_url_derived_hls_keys() {
    let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",LANGUAGE="en",STABLE-RENDITION-ID="rendition-en",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,AUDIO="audio",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
    let master =
        crate::manifest::parse_hls_master_playlist("https://cdn.example.test/master.m3u8", body)
            .expect("valid HLS master playlist");
    let current_video_key = hls_variant_keys(&master.variants)[0]
        .as_deref()
        .expect("stable video key")
        .to_string();
    let current_audio_key = hls_audio_rendition_keys(&master.audio_renditions)[0]
        .as_deref()
        .expect("stable audio key")
        .to_string();
    let lookup = ManifestLookup {
        source_url: "https://cdn.example.test/master.m3u8".to_string(),
        manifest: NativeManifest::HlsMaster(Box::new(master)),
    };

    assert!(
        lookup
            .selected_source(
                &crate::media_types::ManifestRepresentationKey::from(
                    "hls-variant-00000000000000000000000000000000",
                ),
                Some(&crate::media_types::ManifestAudioKey::from(
                    current_audio_key.as_str(),
                )),
                None,
            )
            .is_none()
    );
    assert!(
        lookup
            .selected_source(
                &crate::media_types::ManifestRepresentationKey::from(current_video_key.as_str()),
                Some(&crate::media_types::ManifestAudioKey::from(
                    "hls-audio-00000000000000000000000000000000",
                )),
                None,
            )
            .is_none()
    );
}

#[test]
fn hls_master_rejects_duplicate_track_identities() {
    let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",STABLE-RENDITION-ID="rendition-en",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO="audio",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
    let mut master =
        crate::manifest::parse_hls_master_playlist("https://cdn.example.test/master.m3u8", body)
            .expect("valid HLS master playlist");
    assert!(validate_hls_stable_track_ids(&master).is_ok());

    let duplicate = master.variants[0].clone();
    master.variants = vec![duplicate.clone(), duplicate].into_boxed_slice();
    assert!(matches!(
        validate_hls_stable_track_ids(&master),
        Err(ManifestIntakeError::Unsupported(_))
    ));
    master.variants = vec![master.variants[0].clone()].into_boxed_slice();

    // Attribute-less tracks must collide through their content identities
    // too; this is the dominant path for ordinary non-Apple masters.
    let mut attribute_less = master.variants[0].clone();
    attribute_less.stable_variant_id = None;
    master.variants = vec![attribute_less.clone(), attribute_less].into_boxed_slice();
    assert!(validate_hls_stable_track_ids(&master).is_err());
    master.variants = vec![master.variants[0].clone()].into_boxed_slice();

    let duplicate_rendition = master.audio_renditions[0].clone();
    master.audio_renditions =
        vec![duplicate_rendition.clone(), duplicate_rendition].into_boxed_slice();
    assert!(validate_hls_stable_track_ids(&master).is_err());
    master.audio_renditions = vec![master.audio_renditions[0].clone()].into_boxed_slice();

    let mut attribute_less_rendition = master.audio_renditions[0].clone();
    attribute_less_rendition.stable_rendition_id = None;
    master.audio_renditions =
        vec![attribute_less_rendition.clone(), attribute_less_rendition].into_boxed_slice();
    assert!(validate_hls_stable_track_ids(&master).is_err());
}

#[test]
fn hls_master_supports_tracks_without_stable_ids() {
    // Twitter/X amplify-style master: attribute-less audio and variants.
    let body = br#"#EXTM3U
#EXT-X-VERSION:6
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-MEDIA:NAME="Audio",TYPE=AUDIO,GROUP-ID="audio-32000",AUTOSELECT=YES,URI="/pl/mp4a/32000/a.m3u8"
#EXT-X-MEDIA:NAME="Audio",TYPE=AUDIO,GROUP-ID="audio-128000",AUTOSELECT=YES,URI="/pl/mp4a/128000/b.m3u8"

#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=146321,BANDWIDTH=241630,RESOLUTION=480x270,CODECS="mp4a.40.2,avc1.4D4015",AUDIO="audio-32000"
/pl/avc1/480x270/v1.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=792011,BANDWIDTH=1630685,RESOLUTION=854x480,CODECS="mp4a.40.2,avc1.64001F",AUDIO="audio-128000"
/pl/avc1/854x480/v3.m3u8
"#;
    let master = crate::manifest::parse_hls_master_playlist(
        "https://video.twimg.com/amplify_video/6/pl/master.m3u8",
        body,
    )
    .expect("valid HLS master playlist");

    validate_hls_stable_track_ids(&master).expect("attribute-less masters stay on the native path");
    let keys = hls_variant_keys(&master.variants);
    assert!(
        keys.iter().all(Option::is_some),
        "every variant gets a durable content-derived key"
    );
    assert_ne!(keys[0], keys[1], "distinct tracks get distinct keys");
    let lookup = ManifestLookup {
        source_url: "https://video.twimg.com/amplify_video/6/pl/master.m3u8".to_string(),
        manifest: NativeManifest::HlsMaster(Box::new(master)),
    };
    assert_eq!(
        lookup.choices().len(),
        2,
        "attribute-less masters offer selectable variants"
    );
}

#[test]
fn hls_master_supports_subtitle_renditions_without_stable_ids() {
    let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",URI="subs/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,SUBTITLES="subs",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
    let master =
        crate::manifest::parse_hls_master_playlist("https://cdn.example.test/master.m3u8", body)
            .expect("HLS parser retains subtitle metadata");
    assert!(
        validate_hls_stable_track_ids(&master).is_ok(),
        "subtitle renditions without Apple stable IDs fall back to content identities"
    );
}

#[test]
fn supports_manifest_urls_with_query_strings() {
    assert!(supports(
        "https://cdn.example.test/master.m3u8?expires=1&signature=x"
    ));
    assert!(supports("https://cdn.example.test/manifest.mpd#fragment"));
}

#[tokio::test(flavor = "current_thread")]
async fn manifest_lookup_follows_signed_redirect_with_proxy_and_cookie_scope() {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind manifest proxy fixture");
    let proxy_port = listener
        .local_addr()
        .expect("read manifest proxy address")
        .port();
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::clone(&seen_requests);
    let manifest_body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1280x720,STABLE-VARIANT-ID=\"preview-variant\"\nvideo/720.m3u8?sig=child\n";
    let response_body = manifest_body.to_string();

    let mut proxy_server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept proxy request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.expect("read proxy request");
                assert_ne!(read, 0, "proxy request ended before its headers");
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                assert!(
                    request.len() <= 16 * 1024,
                    "proxy request headers exceeded test limit"
                );
            }
            requests
                .lock()
                .expect("lock captured proxy requests")
                .push(String::from_utf8_lossy(&request).into_owned());

            let response = if request_index == 0 {
                "HTTP/1.1 302 Found\r\nLocation: /signed/master.m3u8?sig=rotated\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
            };
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write proxy response");
        }
    });

    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    let proxy = crate::net_types::ResolvedProxy {
        proxies: vec![reqwest::Proxy::http(&proxy_url).expect("valid test proxy")],
        cli_url: proxy_url,
        no_proxy_env: String::new(),
    };
    let cookie_url = Url::parse("http://manifest.example.test/").expect("cookie origin URL");
    let jar = Arc::new(reqwest::cookie::Jar::default());
    jar.add_cookie_str("sid=manifest-fixture", &cookie_url);
    let client = crate::download_net::http_client_with_jar(Some(&proxy), jar)
        .expect("build manifest client with proxy and cookie provider");

    let lookup_result = tokio::time::timeout(
        Duration::from_secs(5),
        lookup_with_http_fixture_client(
            &client,
            "http://manifest.example.test/master.m3u8?sig=initial",
        ),
    )
    .await;

    let server_result = tokio::time::timeout(Duration::from_secs(5), &mut proxy_server).await;
    if server_result.is_err() {
        proxy_server.abort();
    }
    server_result
        .expect("proxy fixture server timed out")
        .expect("proxy fixture server task failed");

    let lookup = lookup_result
        .expect("manifest lookup timed out")
        .expect("manifest lookup request")
        .expect("fixture manifest should be supported");

    let NativeManifest::HlsMaster(master) = lookup.manifest else {
        panic!("fixture should parse as an HLS master playlist");
    };
    let variant = master
        .variants
        .first()
        .expect("fixture master contains a variant");
    assert_eq!(
        variant.url.as_str(),
        "https://manifest.example.test/signed/video/720.m3u8?sig=child",
        "relative child resources must resolve from the redirected, signed manifest URL"
    );

    let requests = seen_requests.lock().expect("lock captured proxy requests");
    assert_eq!(
        requests.len(),
        2,
        "the proxy should receive both redirect hops"
    );
    assert!(
        requests[0].starts_with("GET http://manifest.example.test/master.m3u8?sig=initial "),
        "first request should retain the original signed URL: {}",
        requests[0]
            .lines()
            .next()
            .unwrap_or("<missing request line>")
    );
    assert!(
        requests[1].starts_with("GET http://manifest.example.test/signed/master.m3u8?sig=rotated "),
        "redirect request should use the refreshed signed URL: {}",
        requests[1]
            .lines()
            .next()
            .unwrap_or("<missing request line>")
    );
    for request in requests.iter() {
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("cookie: sid=manifest-fixture")),
            "the cookie provider must apply the in-scope cookie on every redirect hop: {request}"
        );
    }
}

#[test]
fn preview_cache_accepts_static_hls_and_returns_a_clone() {
    let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:10
#EXTINF:10,
segment.ts
#EXT-X-ENDLIST
"#;
    let plan = crate::manifest::plan_hls_media_playlist(
        "https://cache-preview.example.test/media.m3u8",
        body,
    )
    .expect("valid static HLS media playlist");
    let lookup = ManifestLookup {
        source_url: "https://cache-preview.example.test/media.m3u8".to_string(),
        manifest: crate::manifest_types::NativeManifest::HlsMedia(Box::new(plan)),
    };
    assert!(is_cacheable_static_manifest(&lookup));

    let key = ManifestPreviewCacheKey {
        url: lookup.source_url.clone(),
        cookies_browser: "none".to_string(),
        handoff_cookie_hash: None,
        proxy: None,
    };
    manifest_preview_cache_insert(key.clone(), &lookup);
    let cached = manifest_preview_cache_get(&key).expect("cache hit");
    assert_eq!(cached.source_url, lookup.source_url);
    assert_eq!(cached.choices(), lookup.choices());
}

#[test]
fn preview_cache_accepts_master_selection_metadata_without_signed_children() {
    let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720
video/720.m3u8
"#;
    let master = crate::manifest::parse_hls_master_playlist(
        "https://cache-preview.example.test/master.m3u8",
        body,
    )
    .expect("valid HLS master");
    let lookup = ManifestLookup {
        source_url: "https://cache-preview.example.test/master.m3u8".to_string(),
        manifest: crate::manifest_types::NativeManifest::HlsMaster(Box::new(master)),
    };
    assert!(is_cacheable_static_manifest(&lookup));
}

#[test]
fn preview_cache_rejects_live_hls_media_playlists() {
    let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:10
#EXTINF:10,
segment.ts
"#;
    let plan = crate::manifest::plan_hls_media_playlist(
        "https://cache-preview.example.test/live.m3u8",
        body,
    )
    .expect("valid live HLS media playlist");
    let lookup = ManifestLookup {
        source_url: "https://cache-preview.example.test/live.m3u8".to_string(),
        manifest: crate::manifest_types::NativeManifest::HlsMedia(Box::new(plan)),
    };
    assert!(!is_cacheable_static_manifest(&lookup));
}

#[test]
fn preview_cache_expires_entries_after_the_ttl() {
    let key = ManifestPreviewCacheKey {
        url: "https://cache-expiry.example.test/media.m3u8".to_string(),
        cookies_browser: "none".to_string(),
        handoff_cookie_hash: None,
        proxy: None,
    };
    MANIFEST_PREVIEW_CACHE.lock().expect("cache lock").insert(
        key.clone(),
        (
            Instant::now() - MANIFEST_PREVIEW_CACHE_TTL - Duration::from_secs(1),
            ManifestLookup {
                source_url: key.url.clone(),
                manifest: crate::manifest_types::NativeManifest::HlsMedia(Box::new(
                    crate::manifest::plan_hls_media_playlist(
                        &key.url,
                        br#"#EXTM3U
#EXT-X-TARGETDURATION:10
#EXTINF:10,
segment.ts
#EXT-X-ENDLIST
"#,
                    )
                    .expect("valid static HLS media playlist"),
                )),
            },
        ),
    );
    assert!(manifest_preview_cache_get(&key).is_none());
}

#[test]
fn preview_cache_rejects_any_query_bearing_hls_source_and_child_resources() {
    assert!(
        manifest_preview_cache_key(
            "https://cache-preview.example.test/master.m3u8?X-Amz-Signature=secret",
            "none",
            None,
        )
        .is_none(),
        "known signed source URLs must not be cached"
    );
    assert!(
        manifest_preview_cache_key(
            "https://cache-preview.example.test/master.m3u8?f=418899&srv=server&i=0.4&sp=500&p1=one&p2=two&asn=327934",
            "none",
            None,
        )
        .is_some(),
        "unknown query fields are not classified as signatures at URL-key creation"
    );

    let source_signed_master = crate::manifest::parse_hls_master_playlist(
        "https://cache-preview.example.test/master.m3u8?f=418899&srv=server&i=0.4&sp=500&p1=one&p2=two&asn=327934",
        br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720
video/720.m3u8
"#,
    )
    .expect("valid HLS master with query-bearing source");
    let source_master_lookup = ManifestLookup {
        source_url: "https://cache-preview.example.test/master.m3u8?f=418899&srv=server&i=0.4&sp=500&p1=one&p2=two&asn=327934".to_string(),
        manifest: crate::manifest_types::NativeManifest::HlsMaster(Box::new(source_signed_master)),
    };
    assert!(
        !is_cacheable_static_manifest(&source_master_lookup),
        "a query-bearing HLS source must not be reused even when parameter names are unknown"
    );

    let source_signed_media = crate::manifest::plan_hls_media_playlist(
        "https://cache-preview.example.test/media.m3u8?f=418899&srv=server&i=0.4&sp=500&p1=one&p2=two&asn=327934",
        br#"#EXTM3U
#EXT-X-TARGETDURATION:10
#EXTINF:10,
segment.ts
#EXT-X-ENDLIST
"#,
    )
    .expect("valid static HLS media playlist with query-bearing source");
    let source_media_lookup = ManifestLookup {
        source_url: "https://cache-preview.example.test/media.m3u8?f=418899&srv=server&i=0.4&sp=500&p1=one&p2=two&asn=327934".to_string(),
        manifest: crate::manifest_types::NativeManifest::HlsMedia(Box::new(source_signed_media)),
    };
    assert!(
        !is_cacheable_static_manifest(&source_media_lookup),
        "a query-bearing HLS media source must not be reused"
    );

    let signed_master = crate::manifest::parse_hls_master_playlist(
        "https://cache-preview.example.test/master.m3u8",
        br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720
video/720.m3u8?f=418899&srv=server&i=0.4&sp=500&p1=one&p2=two&asn=327934
"#,
    )
    .expect("valid HLS master");
    let master_lookup = ManifestLookup {
        source_url: "https://cache-preview.example.test/master.m3u8".to_string(),
        manifest: crate::manifest_types::NativeManifest::HlsMaster(Box::new(signed_master)),
    };
    assert!(
        !is_cacheable_static_manifest(&master_lookup),
        "a query-bearing variant URL must prevent caching the master selection"
    );

    let signed_media = crate::manifest::plan_hls_media_playlist(
        "https://cache-preview.example.test/media.m3u8",
        br#"#EXTM3U
#EXT-X-TARGETDURATION:10
#EXTINF:10,
segment.ts?f=418899&srv=server&i=0.4&sp=500&p1=one&p2=two&asn=327934
#EXT-X-ENDLIST
"#,
    )
    .expect("valid static HLS media playlist");
    let media_lookup = ManifestLookup {
        source_url: "https://cache-preview.example.test/media.m3u8".to_string(),
        manifest: crate::manifest_types::NativeManifest::HlsMedia(Box::new(signed_media)),
    };
    assert!(
        !is_cacheable_static_manifest(&media_lookup),
        "a query-bearing segment URL must prevent caching the media playlist"
    );
}

#[test]
fn preview_cache_rejects_signed_urls_but_keys_browser_profiles() {
    assert!(
        manifest_preview_cache_key(
            "https://cache-preview.example.test/master.m3u8?X-Amz-Signature=secret",
            "none",
            None,
        )
        .is_none(),
        "signed manifest URLs must not be retained"
    );
    let firefox = manifest_preview_cache_key(
        "https://cache-preview.example.test/master.m3u8",
        "firefox",
        None,
    );
    assert!(
        firefox.is_some(),
        "a browser profile is keyed by the browser setting with a short TTL"
    );
    assert_ne!(
        firefox,
        manifest_preview_cache_key(
            "https://cache-preview.example.test/master.m3u8",
            "chrome",
            None,
        ),
        "each browser profile must get its own preview entry"
    );
}

#[test]
fn preview_cache_hashes_handoff_cookie_contents_not_paths() {
    let directory = tempfile::tempdir().expect("temporary cookie directory");
    let first = directory.path().join("first.jar");
    let second = directory.path().join("second.jar");
    let contents = b"# Netscape HTTP Cookie File\n.example.test\tTRUE\t/\tFALSE\t0\tsid\tv\n";
    std::fs::write(&first, contents).expect("first jar");
    std::fs::write(&second, contents).expect("second jar");

    assert_eq!(
        handoff_cookie_hash(&first),
        handoff_cookie_hash(&second),
        "a new staged path with the same cookies must reuse the preview"
    );

    std::fs::write(&second, b"# Netscape HTTP Cookie File\n").expect("changed jar");
    assert_ne!(handoff_cookie_hash(&first), handoff_cookie_hash(&second));
    assert!(handoff_cookie_hash(&directory.path().join("missing.jar")).is_none());
}

#[test]
fn preview_cache_key_only_matches_the_source_that_served_it() {
    let url = "https://cache-preview.example.test/master.m3u8";
    let anonymous = manifest_preview_cache_key(url, "none", None).expect("anonymous key");
    let browser = manifest_preview_cache_key(url, "firefox", None).expect("browser key");
    let handoff = ManifestPreviewCacheKey {
        handoff_cookie_hash: Some([7_u8; 32]),
        ..browser.clone()
    };

    assert!(anonymous.matches_auth(ManifestClientAuth::Anonymous));
    assert!(!anonymous.matches_auth(ManifestClientAuth::Handoff));
    assert!(!anonymous.matches_auth(ManifestClientAuth::Browser));

    assert!(browser.matches_auth(ManifestClientAuth::Browser));
    assert!(
        !browser.matches_auth(ManifestClientAuth::Anonymous),
        "a failed cookie export must not store an anonymous preview under a profile key"
    );

    assert!(handoff.matches_auth(ManifestClientAuth::Handoff));
    assert!(!handoff.matches_auth(ManifestClientAuth::Browser));
}
