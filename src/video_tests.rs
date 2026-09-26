use super::*;
use crate::attempt_gate::AttemptGate;
use crate::file_names::{is_url_derived_name, strip_dedupe_suffix};
use crate::media_types::{
    PlaylistInfo, PlaylistItem, PlaylistKind, VIDEO_QUALITY_VALUES, VideoSource, quality_index,
    quality_value,
};
use crate::video_argv::{
    YTDLP_PROGRESS_TEMPLATE, apply_proxy_env, container_truth_name, fallback_to_live_edge,
    hls_download_argv, hls_format_spec, live_capture_argv, live_remux_argv, merge_output_ext,
    part_fallback_spec, proxy_cli_args, unified_download_argv, unified_format_spec,
};
use crate::video_plan::{
    StreamSel, find_hls_format, find_usable_format, plan_streams, select_audio_original_first,
    select_hls_format,
};
use crate::video_prefs::{
    CODEC_PRIORITY_NEWEST, SUBTITLE_LANGUAGE_VALUES, codec_priority_index, codec_priority_value,
    cookies_browser_index, cookies_browser_labels, cookies_browser_value, remux_video_active,
    remux_video_labels, subtitle_lang_active, subtitle_language_index, subtitle_language_labels,
    subtitle_language_value,
};
use crate::video_probe::{
    DIRECT_FILE_EXTS, MAX_PLAYLIST_ITEMS, drive_direct_url, drive_file_id, expand_child_target,
    insta_shortcode_to_pk, is_direct_file_url, is_expired, is_http_url, now_unix,
    parse_playlist_json, parse_single_video, pick_playlist_entry, playlist_resolve_error,
    retarget_story_items, sanitize_video_json, story_segment_url, story_tray_url,
};
use crate::video_progress::{
    grid_needs_rebuild, is_format_selection_line, is_ytdlp_merge_line, leg_changed,
    parse_ytdlp_after_move, parse_ytdlp_template, piece_marks, trace_format_lines,
};
use crate::video_quality::selector_for_quality;
use crate::video_quality::{default_quality_index, default_video_filename, quality_for_height};
use crate::video_runner::{
    pick_subtitle_lang, remux_live_capture, resolve_subtitle_lang, run_hls_ytdlp, run_live_ytdlp,
    run_unified_ytdlp,
};
use crate::video_spawn::{
    ProcessGroupGuard, await_group_quiescence, fetch_raw_dump_json, fetch_video_page, reap_child,
    ytdlp_command,
};
use crate::video_staging::{
    ResumePlan, ResumeQuery, VideoManifest, clean_dest_parts, clean_staging, collect_sidecar,
    dest_part_path, dir_file_names, discover_unified_output, ensure_staging_dir, is_grab_part,
    is_sparse_shell, is_ytdlp_fragment, manifest_path, read_manifest, release_remux_lease,
    reserve_remux_temp, resume_plan, sidecar_path_for, staging_dir, staging_root, stem_reserved_in,
    sweep_partial_remuxes, sweep_staging_preserving_recordings, unified_candidate,
    unified_temp_limit, ytdlp_output_template,
};
use crate::video_tools::VideoError;
use crate::video_tools::{
    COOKIES_BROWSERS, MIN_YTDLP_VERSION, browser_override_command_for, browser_override_dirs,
    browser_profile_dir_in, chromium_subdirs, cookies_browser_spec, distro_packages,
    ensure_tool_versions, extract_ffmpeg_toolchain, find_in_dirs, is_youtube_url,
    parse_yt_dlp_version, quickjs_download_url, quickjs_expected_sha256, toolchain_dir_in,
    user_lib_dir, ytdlp_identity_args, ytdlp_update_available,
};
use crate::video_types::FetchedVideo;
use crate::video_types::codec_preference;
use crate::video_types::video_domain;
use crate::video_types::{
    VideoFormatOption, classify, codec_rank, has_fetchable_media, video_format_options,
};
use pretty_assertions::assert_eq;
use yt_dlp::model::Video;

// ── classify ───────────────────────────────────────────────────────────

#[test]
fn classify_youtube_watch() {
    let url = "https://www.youtube.com/watch?v=gXtp6C-3JKo";
    assert!(matches!(classify(url), VideoSource::Page { page_url, .. } if page_url == url));
}

#[test]
fn classify_youtu_be_short() {
    assert!(is_video_page("https://youtu.be/dQw4w9WgXcQ"));
}

#[test]
fn classify_music_youtube_subdomain() {
    assert!(is_video_page("https://music.youtube.com/watch?v=abc"));
}

#[test]
fn classify_vimeo() {
    assert!(is_video_page("https://vimeo.com/123456"));
}

#[test]
fn classify_tiktok() {
    assert!(is_video_page("https://www.tiktok.com/@user/video/123"));
}

#[test]
fn classify_dailymotion() {
    assert!(is_video_page("https://www.dailymotion.com/video/x5tmt"));
}

#[test]
fn classify_twitch() {
    assert!(is_video_page("https://www.twitch.tv/clip/abc123"));
}

#[test]
fn classify_bilibili() {
    assert!(is_video_page("https://bilibili.com/video/BV1xx411c7mD"));
}

#[test]
fn classify_top_up_domains() {
    for url in [
        "https://www.instagram.com/reel/abc123/",
        "https://www.facebook.com/watch/?v=123",
        "https://fb.watch/abc123/",
        "https://www.threads.com/@user/post/abc",
        "https://bsky.app/profile/user/post/abc",
        "https://www.pinterest.com/pin/123/",
        "https://pin.it/abc123",
        "https://user.tumblr.com/post/123",
        "https://vk.com/video-123_456",
        "https://ok.ru/video/123",
        "https://www.coub.com/view/abc",
        "https://www.bitchute.com/video/abc/",
        "https://odysee.com/@channel:abc/video:def",
        "https://rutube.ru/video/abc/",
        "https://www.nicovideo.jp/watch/sm123",
        "https://www.ted.com/talks/speaker_title",
        "https://archive.org/details/some-item",
        "https://drive.google.com/file/d/abc/view",
        "https://www.dropbox.com/s/abc/file.mp4",
        "https://www.mediafire.com/file/abc/file.mp4",
        "https://www.loom.com/share/abc",
        "https://example.wistia.com/medias/abc",
        "https://fast.wistia.net/embed/abc",
        "https://soundcloud.com/artist/track",
        "https://bandcamp.com/track/name",
        "https://artist.bandcamp.com/track/name",
    ] {
        assert!(is_video_page(url), "should route to video: {url}");
    }
}

#[test]
fn classify_drm_walled_stays_direct() {
    // DRM services stay unlisted: the pipeline refuses to fetch them.
    for url in [
        "https://www.netflix.com/watch/123",
        "https://www.disneyplus.com/video/abc",
        "https://www.primevideo.com/detail/abc",
        "https://open.spotify.com/track/abc",
    ] {
        assert_eq!(
            classify(url),
            VideoSource::Direct,
            "must stay direct: {url}"
        );
    }
}

// ── drive direct fallback ────────────────────────────────────────────

#[test]
fn drive_file_id_matrix() {
    // Share, edit, uc/open and usercontent forms all yield the id.
    for url in [
        "https://drive.google.com/file/d/1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn/view?usp=drive_link",
        "https://drive.google.com/file/d/1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn/edit",
        "https://drive.google.com/uc?id=1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn&export=download",
        "https://drive.google.com/open?id=1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn",
        "https://drive.usercontent.google.com/download?id=1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn&export=download&confirm=t",
    ] {
        assert_eq!(
            drive_file_id(url).as_deref(),
            Some("1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn"),
            "should extract id: {url}"
        );
    }
    // Non-Drive hosts, short ids and id-less Takeout links yield nothing.
    for url in [
        "https://example.com/file/d/1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn/view",
        "https://drive.google.com/file/d/short/view",
        "https://takeout-download-drive-eu.usercontent.google.com/download/drive-download-20260922T071209Z-1-001.zip?j=abc&user=1",
        "not a url",
    ] {
        assert_eq!(drive_file_id(url), None, "must yield nothing: {url}");
    }
}

#[test]
fn drive_direct_url_builds_export_endpoint() {
    assert_eq!(
        drive_direct_url(
            "https://drive.google.com/file/d/1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn/view?usp=drive_link"
        )
        .as_deref(),
        Some(
            "https://drive.usercontent.google.com/download?id=1B2kVNoAT800TK2DyjRX0_mL7LWyL_XCn&export=download&confirm=t"
        )
    );
    assert_eq!(drive_direct_url("https://example.com/file"), None);
}

// ── format guards ────────────────────────────────────────────────────

fn test_format(overrides: serde_json::Value) -> yt_dlp::model::format::Format {
    let mut base = serde_json::json!({
        "format": "137 - 1920x1080",
        "format_id": "137",
        "protocol": "https",
        "ext": "mp4",
        "url": "https://cdn.example/v.mp4",
        "vcodec": "avc1.640028",
        "acodec": "none",
        "http_headers": {},
        "filesize": 100,
    });
    for (k, v) in overrides.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).expect("test format must parse")
}

#[test]
fn from_format_accepts_plain_https() {
    let sel = StreamSel::from_format(&test_format(serde_json::json!({}))).unwrap();
    assert_eq!(sel.format_id, "137");
    assert_eq!(sel.url, "https://cdn.example/v.mp4");
    assert_eq!(sel.size, Some(100));
}

#[test]
fn from_format_rejects_hls_manifest() {
    let f = test_format(serde_json::json!({"protocol": "m3u8_native"}));
    assert!(StreamSel::from_format(&f).is_err());
}

#[test]
fn from_format_rejects_unknown_protocol() {
    let f = test_format(serde_json::json!({"protocol": "weirdcast"}));
    assert!(StreamSel::from_format(&f).is_err());
}

#[test]
fn from_format_rejects_drm() {
    let f = test_format(serde_json::json!({"has_drm": true}));
    assert!(StreamSel::from_format(&f).is_err());
}

#[test]
fn from_format_rejects_missing_url() {
    let f = test_format(serde_json::json!({"url": null}));
    assert!(StreamSel::from_format(&f).is_err());
}

fn test_audio(overrides: serde_json::Value) -> yt_dlp::model::format::Format {
    let mut base = serde_json::json!({
        "vcodec": "none",
        "acodec": "opus",
    });
    for (k, v) in overrides.as_object().unwrap() {
        base[k] = v.clone();
    }
    test_format(base)
}

#[test]
fn audio_prefers_original_over_higher_bitrate_dub() {
    // Auto-dub shape: higher-bitrate dub loses to the original's preference.
    let dub = test_audio(serde_json::json!({
        "format_id": "251-20", "abr": 137.475,
        "language": "it", "language_preference": -1,
    }));
    let original = test_audio(serde_json::json!({
        "format_id": "251-21", "abr": 127.412,
        "language": "en", "language_preference": 10,
    }));
    let formats = vec![dub, original];
    assert_eq!(
        select_audio_original_first(&formats).map(|f| f.format_id.as_str()),
        Some("251-21")
    );
}

#[test]
fn audio_untagged_falls_back_to_bitrate() {
    // Extractors without language scores tie at 0: today's ranking.
    let low = test_audio(serde_json::json!({"format_id": "140", "abr": 129.0}));
    let high = test_audio(serde_json::json!({"format_id": "251", "abr": 135.0}));
    let formats = vec![low, high];
    assert_eq!(
        select_audio_original_first(&formats).map(|f| f.format_id.as_str()),
        Some("251")
    );
}

#[test]
fn audio_untagged_beats_explicit_dub() {
    // Neutral (no score) outranks an explicitly dubbed track.
    let dub = test_audio(serde_json::json!({
        "format_id": "d", "abr": 200.0, "language_preference": -1,
    }));
    let plain = test_audio(serde_json::json!({"format_id": "p", "abr": 48.0}));
    let formats = vec![dub, plain];
    assert_eq!(
        select_audio_original_first(&formats).map(|f| f.format_id.as_str()),
        Some("p")
    );
}

#[test]
fn audio_skips_hls_and_drm_tracks() {
    // Unfetchable tracks never win; the HLS preset depends on degrading to absent here.
    let hls = test_audio(serde_json::json!({
        "format_id": "h", "protocol": "m3u8_native",
        "language_preference": 10,
    }));
    let drm = test_audio(serde_json::json!({
        "format_id": "x", "has_drm": true, "language_preference": 10,
    }));
    let direct = test_audio(serde_json::json!({
        "format_id": "d", "language_preference": -1,
    }));
    let formats = vec![hls, drm, direct];
    assert_eq!(
        select_audio_original_first(&formats).map(|f| f.format_id.as_str()),
        Some("d")
    );
    // All unfetchable degrades to None for the HLS preset path.
    let formats = vec![
        test_audio(serde_json::json!({"format_id": "h", "protocol": "m3u8_native"})),
        test_audio(serde_json::json!({"format_id": "x", "has_drm": true})),
    ];
    assert_eq!(select_audio_original_first(&formats), None);
}

#[test]
fn audio_skips_video_sized_unknown_codec_formats() {
    // Extractor shape seen in the wild: direct MP4s report no video codec
    // (the crate classifies acodec-set/vcodec-unknown as audio) while
    // carrying a height. A height means a video stream, so they must never
    // win the audio leg — otherwise the bogus track suppresses the HLS
    // preset and the download falls back to the lowest-quality direct file.
    let bogus = test_audio(serde_json::json!({
        "format_id": "240p",
        "acodec": "mp4a.40.2",
        "vcodec": null,
        "height": 240,
        "ext": "mp4",
    }));
    let real = test_audio(serde_json::json!({"format_id": "140"}));
    // Bogus last: max_by keeps the last maximal element on ties, so without
    // the height filter this selects "240p".
    let formats = vec![real, bogus.clone()];
    assert_eq!(
        select_audio_original_first(&formats).map(|f| f.format_id.as_str()),
        Some("140")
    );
    // Alone, the bogus format degrades to None for the HLS preset.
    assert_eq!(select_audio_original_first(&[bogus]), None);
}

#[test]
fn classify_reddit_video() {
    assert!(is_video_page(
        "https://www.reddit.com/r/pics/comments/abc/test/"
    ));
}

#[test]
fn classify_direct_file() {
    assert_eq!(
        classify("https://example.com/archive.tar.gz"),
        VideoSource::Direct
    );
}

#[test]
fn classify_ftp_is_direct() {
    assert_eq!(classify("ftp://server/file.bin"), VideoSource::Direct);
}

#[test]
fn classify_empty_is_direct() {
    assert_eq!(classify(""), VideoSource::Direct);
}

#[test]
fn url_derived_names_detect_dialog_less_rows() {
    let page = "https://www.youtube.com/watch?v=abc123";
    // Intake-derives straight and with dedupe suffixes.
    assert!(is_url_derived_name("watch", page));
    assert!(is_url_derived_name("watch (1)", page));
    assert!(is_url_derived_name("watch (12)", page));
    // Dialog-seeded and typed names never match.
    assert!(!is_url_derived_name("Clip [abc123].mp4", page));
    assert!(!is_url_derived_name("myvideo", page));
}

#[test]
fn strip_dedupe_suffix_leaves_titles_alone() {
    assert_eq!(strip_dedupe_suffix("watch (12)"), "watch");
    assert_eq!(strip_dedupe_suffix("Clip (3).mp4"), "Clip.mp4");
    assert_eq!(
        strip_dedupe_suffix("My (old) title.mp4"),
        "My (old) title.mp4"
    );
    assert_eq!(strip_dedupe_suffix("a (1) (2).mp4"), "a (1).mp4");
    // Non-ASCII titles are never split mid-codepoint.
    assert_eq!(strip_dedupe_suffix("Café (2).mp4"), "Café.mp4");
    assert_eq!(strip_dedupe_suffix("watch"), "watch");
    assert_eq!(strip_dedupe_suffix("(1)"), "(1)");
    assert_eq!(strip_dedupe_suffix("Clip."), "Clip.");
    assert_eq!(strip_dedupe_suffix(" (1)"), " (1)");
    assert_eq!(strip_dedupe_suffix("watch (1).mp4"), "watch.mp4");
}

#[test]
fn classify_bare_host_no_scheme() {
    // Missing scheme → Url::parse fails → Direct (caller must normalize first).
    assert_eq!(classify("youtube.com/watch?v=x"), VideoSource::Direct);
}

#[test]
fn classify_garbage_string() {
    assert_eq!(classify("not a url at all"), VideoSource::Direct);
}

// ── video_domain suffix matching ───────────────────────────────────────

#[test]
fn domain_exact_match() {
    assert!(video_domain("youtube.com"));
}

#[test]
fn domain_subdomain_match() {
    assert!(video_domain("www.youtube.com"));
}

#[test]
fn domain_deep_subdomain() {
    assert!(video_domain("sub.sub.youtube.com"));
}

#[test]
fn domain_trailing_dot_stripped() {
    assert!(video_domain("youtube.com."));
}

#[test]
fn domain_case_insensitive() {
    assert!(video_domain("YouTube.COM"));
}

#[test]
fn domain_unknown_not_match() {
    assert!(!video_domain("google.com"));
}

// ── expiry ─────────────────────────────────────────────────────────────

#[test]
fn expired_none_is_expired() {
    assert!(is_expired(None));
}

#[test]
fn expired_past() {
    assert!(is_expired(Some(0)));
}

#[test]
fn expired_future() {
    let future = now_unix() + 3600;
    assert!(!is_expired(Some(future)));
}

// ── staging ────────────────────────────────────────────────────────────

#[test]
fn staging_dir_is_under_root() {
    let d = staging_dir(42);
    assert!(d.starts_with(staging_root()));
    assert!(d.ends_with("42"));
}

#[test]
fn clean_staging_refuses_outside_root() {
    // /tmp is outside the staging root, so clean_staging must delete nothing.
    let fake = std::env::temp_dir().join("grab-video-PROMISE-I-WILL-NOT-DELETE");
    std::fs::create_dir_all(&fake).unwrap();
    clean_staging(&fake);
    assert!(fake.exists());
    let _ = std::fs::remove_dir_all(&fake);
}

#[test]
fn clean_staging_removes_our_dir() {
    let dir = staging_dir(999_999);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(dir.exists());
    clean_staging(&dir);
    assert!(!dir.exists());
}

#[test]
fn ensure_staging_dir_roundtrip_and_clean() {
    let dir = staging_dir(u64::MAX - 8);
    let canon = ensure_staging_dir(&dir).expect("fresh dir verifies");
    assert!(
        canon.starts_with(std::fs::canonicalize(staging_root()).unwrap()),
        "{canon:?}"
    );
    clean_staging(&dir);
    assert!(!dir.exists());
    assert!(!canon.exists());
}

#[cfg(unix)]
#[test]
fn ensure_staging_dir_rejects_symlink_escape() {
    // Pre-planted symlink at the item path: creation follows it, but the canonical check must refuse the escape.
    std::fs::create_dir_all(staging_root()).unwrap();
    let outside = std::env::temp_dir().join("grab-video-escape-target");
    let _ = std::fs::remove_dir_all(&outside);
    let _ = std::fs::remove_file(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    let link = staging_dir(u64::MAX - 7);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let err = ensure_staging_dir(&link).expect_err("symlink escape must fail");
    assert!(err.to_string().contains("escaped"), "{err}");
    assert!(outside.read_dir().unwrap().next().is_none());
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&outside);
}

// ── VideoSource serde round-trip ───────────────────────────────────────

#[test]
fn serde_direct() {
    let s = serde_json::to_string(&VideoSource::Direct).unwrap();
    assert_eq!(s, "\"Direct\"");
    let back: VideoSource = serde_json::from_str(&s).unwrap();
    assert_eq!(back, VideoSource::Direct);
}

#[test]
fn serde_page_round_trip() {
    let src = VideoSource::Page {
        page_url: "https://vimeo.com/99".into(),
        media_url: None,
        expires_at: Some(1_700_000_000),
        quality: "720p".into(),
        audio_only: false,
        is_live: false,
        video_format_id: Some("137".into()),
        playlist_item_id: None,
    };
    let json = serde_json::to_string(&src).unwrap();
    let back: VideoSource = serde_json::from_str(&json).unwrap();
    assert_eq!(back, src);
}

#[test]
fn serde_page_old_json_gets_defaults() {
    // Pre-quality/format queue files must still parse (fall back to 1080p, audio off, no pin).
    let json = r#"{"Page":{"page_url":"https://vimeo.com/99","media_url":null,"expires_at":null}}"#;
    let back: VideoSource = serde_json::from_str(json).unwrap();
    assert_eq!(
        back,
        VideoSource::Page {
            page_url: "https://vimeo.com/99".into(),
            media_url: None,
            expires_at: None,
            quality: "1080p".into(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        }
    );
    // Files that still carry audio_only:true restore as audio rows.
    let back: VideoSource = serde_json::from_str(
        r#"{"Page":{"page_url":"https://vimeo.com/99","media_url":null,"expires_at":null,"audio_only":true}}"#,
    )
    .unwrap();
    assert!(matches!(
        back,
        VideoSource::Page {
            audio_only: true,
            ..
        }
    ));
}

// ── quality mapping ──────────────────────────────────────────────────

#[test]
fn quality_round_trip() {
    for (i, v) in VIDEO_QUALITY_VALUES.iter().enumerate() {
        assert_eq!(quality_index(v), i);
        assert_eq!(quality_value(i), *v);
    }
}

#[test]
fn quality_unknown_falls_back_to_1080p() {
    assert_eq!(quality_index("8k"), 3);
    assert_eq!(quality_index(""), 3);
    assert_eq!(VIDEO_QUALITY_VALUES[quality_index("audio")], "1080p");
}

#[test]
fn quality_oob_index_falls_back() {
    assert_eq!(quality_value(99), "1080p");
}

#[test]
fn classify_page_carries_defaults() {
    let VideoSource::Page { quality, .. } = classify("https://www.youtube.com/watch?v=x") else {
        panic!("expected Page");
    };
    assert_eq!(quality, "1080p");
}

// ── find_in_dirs (pure, no env races) ─────────────────────────────────

#[test]
fn find_in_dirs_picks_first_match() {
    let tmp = std::env::temp_dir().join("grab-test-find-in-dirs");
    let sub = tmp.join("a");
    std::fs::create_dir_all(&sub).unwrap();
    let bin = sub.join("tool");
    std::fs::write(&bin, b"").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let sub_b = tmp.join("b");
    std::fs::create_dir_all(&sub_b).unwrap();
    let bin_b = sub_b.join("tool");
    std::fs::write(&bin_b, b"").unwrap();
    std::fs::set_permissions(&bin_b, std::fs::Permissions::from_mode(0o755)).unwrap();

    let found = find_in_dirs("tool", &[sub_b, sub]);
    assert_eq!(found.as_deref(), Some(bin_b.as_path()));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn find_in_dirs_none_when_non_executable() {
    let tmp = std::env::temp_dir().join("grab-test-find-noexec");
    std::fs::create_dir_all(&tmp).unwrap();
    let bin = tmp.join("noexec");
    std::fs::write(&bin, b"").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(find_in_dirs("noexec", std::slice::from_ref(&tmp)).is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}

// ── user_lib_dir (env-driven, no assertions on real system) ────────────

#[test]
fn user_lib_dir_ends_with_grab_libs() {
    let d = user_lib_dir();
    assert!(
        d.ends_with("grab/libs"),
        "expected …/grab/libs, got {}",
        d.display()
    );
}

// ── quality selector ─────────────────────────────────────────────────

#[test]
fn selector_for_quality_maps_all() {
    use yt_dlp::model::selector::VideoQuality;
    assert_eq!(selector_for_quality("best"), VideoQuality::Best);
    assert_eq!(
        selector_for_quality("2160p"),
        VideoQuality::CustomHeight(2160)
    );
    assert_eq!(
        selector_for_quality("1440p"),
        VideoQuality::CustomHeight(1440)
    );
    assert_eq!(
        selector_for_quality("1080p"),
        VideoQuality::CustomHeight(1080)
    );
    assert_eq!(
        selector_for_quality("720p"),
        VideoQuality::CustomHeight(720)
    );
    assert_eq!(
        selector_for_quality("480p"),
        VideoQuality::CustomHeight(480)
    );
    assert_eq!(selector_for_quality("8k"), VideoQuality::CustomHeight(1080));
    assert_eq!(selector_for_quality(""), VideoQuality::CustomHeight(1080));
}

// ── manifest + resume plan ───────────────────────────────────────────

fn test_manifest() -> VideoManifest {
    VideoManifest {
        page_url: "https://vimeo.com/99".into(),
        quality: "720p".into(),
        video_format_id: Some("137".into()),
        video_ext: "mp4".into(),
        audio_format_id: "251".into(),
        audio_ext: "webm".into(),
        final_bytes: None,
    }
}

fn test_manifest_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("grab-manifest-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_staging(dir: &std::path::Path) -> std::path::PathBuf {
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    staging
}

fn test_query<'a>(
    manifest: Option<&'a VideoManifest>,
    dest: &'a std::path::Path,
    staging: &'a std::path::Path,
) -> ResumeQuery<'a> {
    ResumeQuery {
        manifest,
        dest,
        staging,
        page_url: "https://vimeo.com/99",
        quality: "720p",
        video: Some(("137", "mp4")),
        audio: ("251", "webm"),
        total: Some(150),
    }
}

#[test]
fn resume_plan_fresh_without_manifest() {
    let dir = test_manifest_dir("fresh");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    let q = test_query(None, &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_resumes_partial_temp() {
    // Partial unified temp resumes in place via yt-dlp's own `.part` continuation.
    let dir = test_manifest_dir("partial-temp");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    std::fs::write(staging.join("grab-media.mp4"), vec![0u8; 60]).unwrap();
    let m = test_manifest();
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_resumes_without_temp() {
    // Matching manifest but no temp: same spawn downloads fresh, nothing to wipe.
    let dir = test_manifest_dir("no-temp");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    let m = test_manifest();
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unified_temp_limit_math() {
    // 10% + 8 MiB headroom over total; zero only pins the floor (unknown totals are `None`).
    assert_eq!(unified_temp_limit(0), 8 * 1024 * 1024);
    assert_eq!(unified_temp_limit(150), 150 + 15 + 8 * 1024 * 1024);
    // Saturates instead of overflowing on absurd totals.
    assert_eq!(unified_temp_limit(u64::MAX), u64::MAX);
}

#[test]
fn resume_plan_resumes_temp_within_merge_margin() {
    // Slightly over total is estimate wobble: resume, don't wipe. Dense zeros so `is_sparse_shell` doesn't trigger.
    let dir = test_manifest_dir("merge-margin-temp");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    std::fs::write(staging.join("grab-media.mp4"), vec![0u8; 200]).unwrap();
    let m = test_manifest();
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_resumes_temp_at_limit_boundary() {
    // Exactly at the bound (`>`, not `>=`) still resumes (150-byte fixture total).
    const AT_LIMIT: usize = 150 + 15 + 8 * 1024 * 1024;
    assert_eq!(AT_LIMIT as u64, unified_temp_limit(150));
    let dir = test_manifest_dir("limit-boundary-temp");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    std::fs::write(staging.join("grab-media.mp4"), vec![0u8; AT_LIMIT]).unwrap();
    let m = test_manifest();
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_overlong_temp() {
    // Far past total + margin is garbage: wipe and restart. Hardcoded (8388774) so constant changes force an update here.
    const OVERLONG: usize = 150 + 15 + 8 * 1024 * 1024 + 1;
    assert_eq!(OVERLONG, 8_388_774);
    let dir = test_manifest_dir("overlong-temp");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    std::fs::write(staging.join("grab-media.mp4"), vec![0u8; OVERLONG]).unwrap();
    let m = test_manifest();
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_sparse_temp() {
    // Full apparent size, nothing on disk: resuming would adopt zeros.
    let dir = test_manifest_dir("sparse-temp");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    let temp = staging.join("grab-media.mp4");
    std::fs::File::create(&temp).unwrap().set_len(150).unwrap();
    assert!(is_sparse_shell(&temp));
    let m = test_manifest();
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_ignores_dest_dir_parts() {
    // Legacy dest-dir parts are invisible to the unified engine.
    let dir = test_manifest_dir("legacy-parts");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    std::fs::write(dest_part_path(&dest, "video", "mp4"), vec![0u8; 100]).unwrap();
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 50]).unwrap();
    let m = test_manifest();
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_without_manifest_despite_temp() {
    // Bytes without a matching sidecar are unverifiable: wipe and start clean.
    let dir = test_manifest_dir("unverified");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    std::fs::write(staging.join("grab-media.mp4"), vec![0u8; 60]).unwrap();
    let q = test_query(None, &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_selection_change() {
    let dir = test_manifest_dir("reselect");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    let m = test_manifest();
    let q = ResumeQuery {
        quality: "1080p",
        ..test_query(Some(&m), &dest, &staging)
    };
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let q = ResumeQuery {
        audio: ("250", "webm"),
        ..test_query(Some(&m), &dest, &staging)
    };
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_finished_when_dest_complete() {
    let dir = test_manifest_dir("finished");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    std::fs::write(&dest, vec![0u8; 1000]).unwrap();
    let mut m = test_manifest();
    m.final_bytes = Some(1000);
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Finished);
    // Same manifest but foreign bytes at dest: fall through to the temp check instead.
    std::fs::write(&dest, vec![0u8; 999]).unwrap();
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_single_file_identity() {
    // Adopted videoless single file resumes; a stale video expectation restarts.
    let dir = test_manifest_dir("adopted-single");
    let dest = dir.join("Clip.m4a");
    let staging = test_staging(&dir);
    let mut m = test_manifest();
    m.video_format_id = None;
    m.video_ext = String::new();
    let q = ResumeQuery {
        video: None,
        ..test_query(Some(&m), &dest, &staging)
    };
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let q = test_query(Some(&m), &dest, &staging);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn manifest_serde_round_trip() {
    let dir = test_manifest_dir("serde");
    let m = test_manifest();
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&m).unwrap(),
    )
    .unwrap();
    assert_eq!(read_manifest(&dir).as_ref(), Some(&m));
    // Corrupt sidecars read as absent, never fatal.
    std::fs::write(dir.join("manifest.json"), b"{nope").unwrap();
    assert_eq!(read_manifest(&dir), None);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── resume re-resolve label ──────────────────────────────────────────

#[test]
fn resume_attempt_labels_reresolve_as_resuming() {
    // Parked row keeps its staging manifest, so re-resolve reads "Resuming download…". Probe fails on dumb fakes; harmless.
    let dir = std::env::temp_dir().join(format!("grab-resume-label-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let libs = dir.join("xdg").join("grab").join("libs");
    std::fs::create_dir_all(&libs).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    for (name, body) in [
        ("yt-dlp", "#!/bin/sh\necho 2026.09.25\n"),
        ("ffmpeg", "#!/bin/sh\necho ffmpeg version test\n"),
    ] {
        let bin = libs.join(name);
        std::fs::write(&bin, body).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _env = ScopedEnv::apply("/nonexistent-grab-test", &dir.join("xdg"));

    let item_id = 900_000 + std::process::id() as u64;
    let staging = staging_dir(item_id);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(
        manifest_path(&staging),
        serde_json::to_string(&test_manifest()).unwrap(),
    )
    .unwrap();

    let job = VideoJob {
        item_id,
        page_url: "https://vimeo.com/123456".into(),
        playlist_item_id: None,
        quality: "1080p".into(),
        audio_only: false,
        dest: dir.join("v.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let _ = crate::runtime::tokio_rt().block_on(run_video_download(
        job,
        &AttemptGate::new(),
        abort_rx,
        tx,
    ));
    let mut first_phase = None;
    while let Ok(msg) = rx.try_recv() {
        if let crate::engine_msg::EngineMsg::Phase(p) = msg
            && first_phase.is_none()
        {
            first_phase = Some(p);
        }
    }
    assert_eq!(first_phase.as_deref(), Some("Resuming download…"));
    drop(_env);
    clean_staging(&staging);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── pipeline failure without tools (no network) ──────────────────────

#[test]
fn pipeline_reports_missing_tools() {
    use crate::video::test_support::NoVideoTools;

    let _guard = NoVideoTools::apply();
    let item_id = 800_000 + std::process::id() as u64;
    let job = VideoJob {
        item_id,
        page_url: "https://vimeo.com/123456".into(),
        playlist_item_id: None,
        quality: "1080p".into(),
        audio_only: false,
        dest: std::env::temp_dir().join("grab-pipeline-probe.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_video_download(
        job,
        &AttemptGate::new(),
        abort_rx,
        tx,
    ));
    assert!(matches!(res, Err(VideoError::MissingLibraries(_))));
    // Resolving never started; the abandoned empty staging dir is the caller's to drop.
    assert!(rx.try_recv().is_err());
    drop(rx);
    clean_staging(&staging_dir(item_id));
    assert!(!staging_dir(item_id).exists());
}

// ── preview freshness (dialog kick/submit gate) ──────────────────────

fn test_video_info(page_url: &str) -> ProbeResult {
    ProbeResult::Single(VideoInfo {
        title: "T".into(),
        duration: None,
        duration_string: None,
        page_url: page_url.into(),
        expires_at: None,
        formats: vec![],
        is_live: false,
        fetchable: false,
    })
}

#[test]
fn preview_fresh_matches_round_trip() {
    let info = Some(test_video_info("https://www.youtube.com/watch?v=x"));
    assert!(preview_fresh(
        &info,
        "https://www.youtube.com/watch?v=x",
        "https://www.youtube.com/watch?v=x",
    ));
}

#[test]
fn preview_fresh_accepts_canonical_drift() {
    // Canonicalized youtu.be → youtube.com still matches, so Add proceeds instead of re-resolving.
    let info = Some(test_video_info("https://www.youtube.com/watch?v=x"));
    assert!(preview_fresh(
        &info,
        "https://youtu.be/x",
        "https://youtu.be/x"
    ));
}

#[test]
fn preview_fresh_rejects_stale_and_empty() {
    let info = Some(test_video_info("https://vimeo.com/1"));
    assert!(!preview_fresh(
        &info,
        "https://vimeo.com/1",
        "https://vimeo.com/2"
    ));
    assert!(!preview_fresh(
        &None,
        "https://vimeo.com/1",
        "https://vimeo.com/1"
    ));
    // Empty text never matches, even with a coincidental empty key.
    assert!(!preview_fresh(&info, "", ""));
}

// ── playlist probe ───────────────────────────────────────────────────

fn playlist_json(entries: serde_json::Value, url: &str) -> serde_json::Value {
    serde_json::json!({
        "_type": "playlist",
        "id": "PL1",
        "title": "My List",
        "webpage_url": url,
        "extractor": "youtube",
        "extractor_key": "YoutubePlaylist",
        "entries": entries,
    })
}

#[test]
fn parse_playlist_reads_flat_entries() {
    let value = playlist_json(
        serde_json::json!([
            {"_type": "url", "id": "a1", "title": "First", "webpage_url": "https://www.youtube.com/watch?v=a1", "playlist_index": 1, "duration": 61.9},
            null,
            {"id": "b2", "title": "", "url": "https://example.com/v/b2"},
            {"id": "c3", "title": "No usable url", "url": "not-a-url"},
        ]),
        "https://www.youtube.com/playlist?list=PL1",
    );
    let pl =
        parse_playlist_json(&value, "https://www.youtube.com/playlist?list=PL1").expect("playlist");
    assert_eq!(pl.kind, PlaylistKind::Playlist);
    assert_eq!(pl.title, "My List");
    assert_eq!(pl.total, 4);
    // Null, empty and unusable-URL entries are dropped.
    assert_eq!(pl.items.len(), 2);
    assert_eq!(pl.items[0].title, "First");
    assert_eq!(pl.items[0].page_url, "https://www.youtube.com/watch?v=a1");
    // Float durations truncate like the video path.
    assert_eq!(pl.items[0].duration, Some(61));
    assert_eq!(pl.items[0].index, 1);
    // Empty title falls back to id; missing index falls back to position.
    assert_eq!(pl.items[1].title, "b2");
    assert_eq!(pl.items[1].page_url, "https://example.com/v/b2");
    assert_eq!(pl.items[1].index, 3);
    assert_eq!(pl.items[1].duration, None);
}

#[test]
fn parse_playlist_classifies_stories_and_highlights() {
    let entries = serde_json::json!([]);
    let pl = parse_playlist_json(
        &playlist_json(
            entries.clone(),
            "https://www.instagram.com/stories/someuser/",
        ),
        "https://www.instagram.com/stories/someuser/",
    )
    .expect("playlist");
    assert_eq!(pl.kind, PlaylistKind::Stories);
    // Highlights URLs contain "/stories/" too: the highlight check wins.
    let pl = parse_playlist_json(
        &playlist_json(entries, "https://www.instagram.com/stories/highlights/123/"),
        "https://www.instagram.com/stories/highlights/123/",
    )
    .expect("playlist");
    assert_eq!(pl.kind, PlaylistKind::Highlights);
}

#[test]
fn parse_playlist_rejects_single_video_json() {
    let video = serde_json::json!({"id": "v", "title": "T", "_type": "video"});
    assert!(parse_playlist_json(&video, "https://www.youtube.com/watch?v=v").is_none());
    let bare = serde_json::json!({"id": "v"});
    assert!(parse_playlist_json(&bare, "https://example.com/v").is_none());
}

// ── story tray retarget ──────────────────────────────────────────────

#[test]
fn story_tray_url_extracts_owner_tray() {
    assert_eq!(
        story_tray_url("https://www.instagram.com/stories/someuser/12345678901234567/"),
        Some("https://www.instagram.com/stories/someuser/".to_string())
    );
    assert_eq!(
        story_tray_url("https://www.instagram.com/stories/someuser/12345678901234567"),
        Some("https://www.instagram.com/stories/someuser/".to_string())
    );
    // Tray URLs, highlights, non-story pages, other hosts and junk have nothing to retarget.
    assert_eq!(
        story_tray_url("https://www.instagram.com/stories/someuser/"),
        None
    );
    assert_eq!(
        story_tray_url("https://www.instagram.com/stories/highlights/123/"),
        None
    );
    assert_eq!(story_tray_url("https://www.instagram.com/p/ABCdef/"), None);
    assert_eq!(
        story_tray_url("https://example.com/stories/someuser/123/"),
        None
    );
    assert_eq!(story_tray_url("not a url"), None);
}

#[test]
fn insta_shortcode_to_pk_matches_ytdlp_vector() {
    // Known yt-dlp vector: `aye83DjauH` ↔ 482584233761418119.
    assert_eq!(
        insta_shortcode_to_pk("aye83DjauH"),
        Some(482584233761418119)
    );
    // Private-post suffix (shortcode + 28 chars) decodes to the same pk.
    let long = format!("aye83DjauH{}", "x".repeat(28));
    assert_eq!(insta_shortcode_to_pk(&long), Some(482584233761418119));
    // Outside the alphabet, empty, and overflow-adjacent junk fail.
    assert_eq!(insta_shortcode_to_pk("abc def!"), None);
    assert_eq!(insta_shortcode_to_pk(""), None);
}

#[test]
fn story_segment_url_points_at_the_segment() {
    assert_eq!(
        story_segment_url(
            "https://www.instagram.com/stories/fruits_zipper/",
            "aye83DjauH"
        ),
        Some("https://www.instagram.com/stories/fruits_zipper/482584233761418119/".to_string())
    );
    // Works off a single-story link too (username still parses).
    assert_eq!(
        story_segment_url(
            "https://www.instagram.com/stories/fruits_zipper/3570766765028588805/",
            "aye83DjauH"
        ),
        Some("https://www.instagram.com/stories/fruits_zipper/482584233761418119/".to_string())
    );
    // Highlights, non-story links and undecodable ids fall back to tray + entry-id selection.
    assert_eq!(
        story_segment_url(
            "https://www.instagram.com/stories/highlights/18090946048123978/",
            "aye83DjauH"
        ),
        None
    );
    assert_eq!(
        story_segment_url("https://www.instagram.com/p/ABCdef/", "aye83DjauH"),
        None
    );
    assert_eq!(
        story_segment_url(
            "https://www.instagram.com/stories/fruits_zipper/",
            "not a shortcode!"
        ),
        None
    );
}

#[test]
fn insta_shortcode_rejects_overflow_and_unicode() {
    // Overflow, non-ASCII, overlong and all-zero codes return None, never wrap/panic/split mid-codepoint.
    assert_eq!(insta_shortcode_to_pk(&"_".repeat(14)), None);
    assert_eq!(insta_shortcode_to_pk("é"), None);
    assert_eq!(insta_shortcode_to_pk(&"€".repeat(10)), None);
    assert_eq!(insta_shortcode_to_pk(&"A".repeat(64)), None);
    assert_eq!(insta_shortcode_to_pk("A"), None);
}

#[test]
fn insta_shortcode_pins_dash_underscore_order() {
    // Known vector lacks `-`/`_`; pin positions explicitly (table ends `...89-_`).
    assert_eq!(insta_shortcode_to_pk("-"), Some(62));
    assert_eq!(insta_shortcode_to_pk("_"), Some(63));
    assert_eq!(insta_shortcode_to_pk("A-"), Some(62));
}

#[test]
fn retarget_story_items_points_entries_at_tray() {
    let story_url = "https://www.instagram.com/stories/someuser/12345678901234567/";
    let mut pl = parse_playlist_json(
        &playlist_json(
            serde_json::json!([
                {"_type": "url_transparent", "id": "s1", "title": "Story 1", "webpage_url": story_url},
                {"_type": "url_transparent", "id": "s2", "title": "Story 2", "webpage_url": story_url},
            ]),
            story_url,
        ),
        story_url,
    )
    .expect("playlist");
    assert_eq!(pl.kind, PlaylistKind::Stories);
    retarget_story_items(story_url, &mut pl);
    assert_eq!(pl.items.len(), 2);
    for item in &pl.items {
        assert_eq!(item.page_url, "https://www.instagram.com/stories/someuser/");
    }
}

#[test]
fn retarget_story_items_leaves_highlights_alone() {
    // A highlight *is* the collection, so the collection URL stands.
    let hl_url = "https://www.instagram.com/stories/highlights/18090946048123978/";
    let mut pl = parse_playlist_json(
        &playlist_json(
            serde_json::json!([
                {"_type": "url_transparent", "id": "h1", "title": "HL 1", "webpage_url": hl_url},
            ]),
            hl_url,
        ),
        hl_url,
    )
    .expect("playlist");
    assert_eq!(pl.kind, PlaylistKind::Highlights);
    retarget_story_items(hl_url, &mut pl);
    assert_eq!(pl.items[0].page_url, hl_url);
}

#[test]
fn retarget_story_items_leaves_plain_playlists_alone() {
    let url = "https://www.youtube.com/playlist?list=PL1";
    let mut pl = parse_playlist_json(
        &playlist_json(
            serde_json::json!([
                {"id": "a1", "title": "A", "url": "a1", "webpage_url": "https://www.youtube.com/watch?v=a1"},
            ]),
            url,
        ),
        url,
    )
    .expect("playlist");
    retarget_story_items(url, &mut pl);
    assert_eq!(pl.items[0].page_url, "https://www.youtube.com/watch?v=a1");
}

// ── picked playlist entry ──────────────────────────────────────────

fn story_tray_json() -> serde_json::Value {
    serde_json::json!({
        "_type": "playlist",
        "id": "stories-tray",
        "webpage_url": "https://www.instagram.com/stories/someuser/",
        "entries": [
            {"_type": "url_transparent", "id": "s1", "title": "Story 1", "webpage_url": "https://www.instagram.com/stories/someuser/"},
            {"_type": "url_transparent", "id": "s2", "title": "Story 2", "webpage_url": "https://www.instagram.com/stories/someuser/"},
        ],
    })
}

#[test]
fn pick_playlist_entry_finds_picked_story() {
    let value = story_tray_json();
    let entry = pick_playlist_entry(&value, Some("s2")).expect("picked entry");
    assert_eq!(entry.get("id").and_then(|id| id.as_str()), Some("s2"));
    assert!(pick_playlist_entry(&value, None).is_none());
    assert!(pick_playlist_entry(&value, Some("gone")).is_none());
}

#[test]
fn picked_story_entry_parses_as_single_video() {
    // The worker-selected entry must survive the single-video parse into the Video model.
    let value = story_tray_json();
    let entry = pick_playlist_entry(&value, Some("s1")).expect("picked entry");
    let video = parse_single_video(entry).expect("video");
    assert_eq!(video.title, "Story 1");
}

#[test]
fn playlist_resolve_error_distinguishes_expired_from_routing_bug() {
    // A row picked from a playlist whose entry is gone: the story expired.
    let expired = format!("{}", playlist_resolve_error(Some("s1")));
    assert!(expired.contains("no longer available"), "{expired}");
    // Playlist-shaped output with no persisted pick: a routing bug.
    let routing = format!("{}", playlist_resolve_error(None));
    assert!(
        routing.contains("collection, not a single video"),
        "{routing}"
    );
}

#[test]
fn parse_playlist_ignores_null_entries() {
    // Null entries (private/deleted) fall through to the single-video path instead of erroring.
    let value = serde_json::json!({
        "_type": "playlist",
        "id": "PL9",
        "title": "Gone",
        "webpage_url": "https://www.youtube.com/playlist?list=PL9",
        "entries": null,
    });
    assert!(parse_playlist_json(&value, "https://www.youtube.com/playlist?list=PL9").is_none());
}

#[test]
fn parse_playlist_prefers_webpage_url() {
    // Flat entries carry the bare id in `url`: only http(s) values serve as page URL.
    let value = playlist_json(
        serde_json::json!([
            {"id": "a1", "title": "A", "url": "a1", "webpage_url": "https://www.youtube.com/watch?v=a1"},
            {"id": "b2", "title": "B", "url": "b2"},
        ]),
        "https://www.youtube.com/playlist?list=PL1",
    );
    let pl =
        parse_playlist_json(&value, "https://www.youtube.com/playlist?list=PL1").expect("playlist");
    assert_eq!(pl.items.len(), 1);
    assert_eq!(pl.items[0].page_url, "https://www.youtube.com/watch?v=a1");
}

#[test]
fn parse_playlist_caps_items_but_reports_total() {
    let entries: Vec<serde_json::Value> = (0..600)
        .map(|i| {
            serde_json::json!({
                "id": format!("v{i}"),
                "title": format!("T{i}"),
                "webpage_url": format!("https://www.youtube.com/watch?v=v{i}"),
            })
        })
        .collect();
    let value = playlist_json(
        serde_json::Value::Array(entries),
        "https://www.youtube.com/playlist?list=PL1",
    );
    let pl =
        parse_playlist_json(&value, "https://www.youtube.com/playlist?list=PL1").expect("playlist");
    assert_eq!(pl.items.len(), MAX_PLAYLIST_ITEMS);
    assert_eq!(pl.total, 600);
}

#[test]
fn probe_result_helpers_cover_both_variants() {
    let single = test_video_info("https://www.youtube.com/watch?v=x");
    assert_eq!(single.page_url(), "https://www.youtube.com/watch?v=x");
    assert_eq!(single.title(), "T");
    assert!(!single.fetchable());

    let items = vec![PlaylistItem {
        index: 1,
        id: "a1".into(),
        title: "First".into(),
        page_url: "https://www.youtube.com/watch?v=a1".into(),
        duration: None,
    }];
    let list = ProbeResult::Playlist(PlaylistInfo {
        id: "PL1".into(),
        title: "My List".into(),
        page_url: "https://www.youtube.com/playlist?list=PL1".into(),
        kind: PlaylistKind::Playlist,
        total: 1,
        items,
    });
    assert_eq!(list.page_url(), "https://www.youtube.com/playlist?list=PL1");
    assert_eq!(list.title(), "My List");
    assert!(list.fetchable());

    let empty = ProbeResult::Playlist(PlaylistInfo {
        id: "PL1".into(),
        title: "My List".into(),
        page_url: "https://www.youtube.com/playlist?list=PL1".into(),
        kind: PlaylistKind::Playlist,
        total: 0,
        items: vec![],
    });
    assert!(!empty.fetchable());
}

// ── tool versions ────────────────────────────────────────────────────

#[test]
fn parse_yt_dlp_version_matrix() {
    assert_eq!(parse_yt_dlp_version("2026.08.19"), Some([2026, 8, 19]));
    assert_eq!(parse_yt_dlp_version("  2025.01.01\n"), Some([2025, 1, 1]));
    assert_eq!(parse_yt_dlp_version("nightly"), None);
    assert_eq!(parse_yt_dlp_version(""), None);
    assert_eq!(parse_yt_dlp_version("2026.08"), None);
    assert_eq!(parse_yt_dlp_version("not.a.version"), None);
}

#[test]
fn version_floor_accepts_new_rejects_old() {
    assert!([2026, 8, 19] >= MIN_YTDLP_VERSION);
    assert!([2027, 1, 1] >= MIN_YTDLP_VERSION);
    assert!([2026, 1, 1] >= MIN_YTDLP_VERSION);
    assert!([2025, 12, 31] < MIN_YTDLP_VERSION);
}

fn fake_tool(dir: &std::path::Path, name: &str, first_line: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\necho '{first_line}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Fake ffmpeg like the real one: only `-version` works. Guards the flag plumbing that once broke every attempt while fakes stayed green.
fn fake_ffmpeg_strict(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("ffmpeg");
    std::fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = \"-version\" ]; then echo 'ffmpeg version n9.0.1'; else echo \"Unrecognized option '$1'.\" >&2; exit 8; fi\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn ensure_tool_versions_accepts_fresh_pair() {
    let dir = std::env::temp_dir().join(format!("grab-versions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let yt = fake_tool(&dir, "yt-dlp", "2026.08.19");
    let ff = fake_ffmpeg_strict(&dir);
    let libs = yt_dlp::client::deps::Libraries::new(yt, ff);
    let (yt_v, ff_v) = crate::runtime::tokio_rt()
        .block_on(ensure_tool_versions(&libs))
        .expect("fresh pair passes");
    assert_eq!(yt_v, "2026.08.19");
    assert!(ff_v.starts_with("ffmpeg version"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_tool_versions_refuses_stale_yt_dlp() {
    let dir = std::env::temp_dir().join(format!("grab-versions-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let yt = fake_tool(&dir, "yt-dlp", "2024.10.07");
    let ff = fake_ffmpeg_strict(&dir);
    let libs = yt_dlp::client::deps::Libraries::new(yt, ff);
    let res = crate::runtime::tokio_rt().block_on(ensure_tool_versions(&libs));
    assert!(
        matches!(res, Err(VideoError::Message(_))),
        "stale binary must fail with the actionable message, got {res:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_tool_versions_refuses_missing_binary() {
    let libs = yt_dlp::client::deps::Libraries::new(
        "/nonexistent-grab-test/yt-dlp".into(),
        "/nonexistent-grab-test/ffmpeg".into(),
    );
    let res = crate::runtime::tokio_rt().block_on(ensure_tool_versions(&libs));
    assert!(matches!(res, Err(VideoError::MissingLibraries(_))));
}

// ── default video filename ───────────────────────────────────────────

#[test]
fn default_video_filename_carries_no_video_id() {
    // No id in the name: the source URL in the `comment` tag keeps it recoverable.
    assert_eq!(default_video_filename("Clip", false, None), "Clip.mp4");
    assert_eq!(default_video_filename("Clip", true, None), "Clip.m4a");
    // Sanitizing is the intake's job: `a/b` survives here, so this is still not a safe path.
    assert_eq!(default_video_filename("a/b", false, None), "a/b.mp4");
    assert_eq!(default_video_filename("", true, None), ".m4a");
}

#[test]
fn default_video_filename_remux_ext() {
    // Remux target decides the video extension; audio-only ignores it.
    assert_eq!(
        default_video_filename("Clip", false, Some("mkv")),
        "Clip.mkv"
    );
    assert_eq!(
        default_video_filename("Clip", true, Some("mkv")),
        "Clip.m4a"
    );
}

#[test]
fn container_truth_name_corrects_stale_ext() {
    use std::path::Path;
    // Native webm merge under an mp4 intake name: same stem, truer ext.
    assert_eq!(
        container_truth_name(
            Path::new("/dl/Clip [x].mp4"),
            Path::new("/st/grab-media.webm"),
        )
        .as_deref(),
        Some("Clip [x].webm")
    );
    // Agreement (even case-insensitively) and exotic intake names stay.
    assert_eq!(
        container_truth_name(Path::new("/dl/Clip.mp4"), Path::new("/st/grab-media.MP4")),
        None
    );
    assert_eq!(
        container_truth_name(Path::new("/dl/talk.mkv"), Path::new("/st/grab-media.webm")),
        None
    );
    // Unknown containers and extensionless sides never rename.
    assert_eq!(
        container_truth_name(Path::new("/dl/Clip.mp4"), Path::new("/st/grab-media.bin")),
        None
    );
    assert_eq!(
        container_truth_name(Path::new("/dl/Clip"), Path::new("/st/grab-media.webm")),
        None
    );
    assert_eq!(
        container_truth_name(Path::new("/dl/.mp4"), Path::new("/st/grab-media.webm")),
        None
    );
    // Multi-dot stems keep everything but the last extension.
    assert_eq!(
        container_truth_name(
            Path::new("/dl/my.clip.v2.mp4"),
            Path::new("/st/grab-media.mkv"),
        )
        .as_deref(),
        Some("my.clip.v2.mkv")
    );
    assert_eq!(
        container_truth_name(Path::new("/dl/Clip.m4a"), Path::new("/st/grab-media.webm"))
            .as_deref(),
        Some("Clip.webm")
    );
}

// ── video format options ─────────────────────────────────────────────

fn test_video(formats: serde_json::Value) -> yt_dlp::model::Video {
    serde_json::from_value(serde_json::json!({
        "id": "x",
        "title": "T",
        "age_limit": 0,
        "live_status": "not_live",
        "playable_in_embed": true,
        "extractor": "generic",
        "extractor_key": "Generic",
        "_version": {"version": "2026.08.19", "repository": "yt-dlp"},
        "formats": formats,
    }))
    .expect("test video must parse")
}

fn test_format_full(
    id: &str,
    vcodec: &str,
    acodec: &str,
    height: Option<u32>,
    filesize: Option<i64>,
    protocol: &str,
    drm: bool,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "format": id,
        "format_id": id,
        "protocol": protocol,
        "ext": "mp4",
        "url": format!("https://cdn.example/{id}"),
        "vcodec": vcodec,
        "acodec": acodec,
        "http_headers": {},
    });
    if drm {
        v["has_drm"] = serde_json::json!(true);
    }
    match (height, filesize) {
        (Some(h), Some(n)) => {
            v["height"] = h.into();
            v["filesize"] = n.into();
        }
        (Some(h), None) => {
            v["height"] = h.into();
        }
        _ => {}
    }
    v
}

#[test]
fn video_format_options_lists_best_per_height() {
    let video = test_video(serde_json::json!([
        test_format_full(
            "v360-vp9",
            "vp9",
            "none",
            Some(360),
            Some(10_000_000),
            "https",
            false
        ),
        test_format_full(
            "v1080-vp9",
            "vp9",
            "none",
            Some(1080),
            Some(200_000_000),
            "https",
            false
        ),
        test_format_full(
            "v1080-avc",
            "avc1.640028",
            "none",
            Some(1080),
            Some(180_000_000),
            "https",
            false
        ),
        test_format_full(
            "v720",
            "avc1.64001f",
            "none",
            Some(720),
            Some(90_000_000),
            "https",
            false
        ),
        test_format_full(
            "v720-av01",
            "av01.0.08M.08",
            "none",
            Some(720),
            Some(60_000_000),
            "https",
            false
        ),
        test_format_full(
            "a-only",
            "none",
            "opus",
            None,
            Some(8_000_000),
            "https",
            false
        ),
        test_format_full(
            "hls",
            "avc1.640028",
            "none",
            Some(1080),
            Some(180_000_000),
            "m3u8_native",
            false
        ),
        test_format_full(
            "drm",
            "avc1.640028",
            "none",
            Some(480),
            Some(40_000_000),
            "https",
            true
        ),
        test_format_full(
            "muxed",
            "avc1.640028",
            "mp4a.40.2",
            Some(720),
            Some(95_000_000),
            "https",
            false
        ),
    ]));
    let opts = video_format_options(&video, true);
    // Newest codec wins each height; audio-only, HLS, DRM and muxed never list; tallest first.
    assert_eq!(
        opts.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["v1080-vp9", "v720-av01", "v360-vp9"]
    );
    assert_eq!(opts[0].label, "1080p · vp9 · 200.0 MB");
    assert_eq!(opts[0].height, 1080);
    assert_eq!(opts[1].label, "720p · av01 · 60.0 MB");
}

#[test]
fn unavailable_detail_counts_rejections() {
    let mut no_link = test_format_full("n", "avc1", "none", Some(720), None, "https", false);
    no_link.as_object_mut().unwrap().remove("url");
    let formats: Vec<yt_dlp::model::format::Format> = serde_json::from_value(serde_json::json!([
        test_format_full("v", "avc1", "none", Some(720), None, "https", false),
        test_format_full("h", "avc1", "none", Some(720), None, "m3u8_native", false),
        test_format_full("d", "avc1", "none", Some(720), None, "https", true),
        no_link,
    ]))
    .unwrap();
    let message = match VideoError::unavailable_detail(&formats) {
        VideoError::Message(m) => m,
        other => panic!("expected message, got {other:?}"),
    };
    assert!(message.contains("4 listed"), "{message}");
    assert!(message.contains("manifest: 1"), "{message}");
    assert!(message.contains("DRM: 1"), "{message}");
    assert!(message.contains("no link: 1"), "{message}");
    assert!(message.contains("video-only: 1"), "{message}");
    let empty = match VideoError::unavailable_detail(&[]) {
        VideoError::Message(m) => m,
        other => panic!("expected message, got {other:?}"),
    };
    assert!(empty.contains("listed none"), "{empty}");
}

#[test]
fn hls_selection_prefers_capped_height() {
    let formats: Vec<yt_dlp::model::format::Format> = serde_json::from_value(serde_json::json!([
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
        test_format_full("https", "avc1", "none", Some(720), None, "https", false),
    ]))
    .unwrap();
    // Closest at or above the cap; tallest when capped above all. Selections carry the id so runners pin the variant.
    let sel = select_hls_format(&formats, Some(720)).expect("hls");
    assert_eq!(sel.format_id, "h1080");
    assert_eq!(
        select_hls_format(&formats, Some(2160))
            .expect("hls")
            .format_id,
        "h1080"
    );
    // Best takes the tallest; https entries never select as HLS.
    assert_eq!(
        select_hls_format(&formats, None).expect("hls").format_id,
        "h1080"
    );
    assert!(select_hls_format(&[], Some(720)).is_none());
    assert_eq!(
        find_hls_format(&formats, "h480").expect("pin").format_id,
        "h480"
    );
    assert!(find_hls_format(&formats, "https").is_none());
    assert!(find_hls_format(&formats, "gone").is_none());
}

#[test]
fn explicit_nulls_parse_to_defaults() {
    // TikTok shape: explicit nulls where the model wants maps/arrays.
    let mut nulls = serde_json::json!({
        "id": "tk",
        "title": "T",
        "thumbnails": serde_json::Value::Null,
        "subtitles": serde_json::Value::Null,
        "formats": serde_json::Value::Null,
    });
    sanitize_video_json(&mut nulls);
    let nulls_video: Video = serde_json::from_value(nulls).expect("nulls parse");
    assert!(nulls_video.formats.is_empty());
    assert!(nulls_video.thumbnails.is_empty());
}

#[test]
fn hls_format_spec_names_height_and_pin() {
    // bv* leads so direct muxed files win; trailing /b catches audio-only pages.
    assert_eq!(hls_format_spec("best", None), "bv*+ba/b");
    assert_eq!(hls_format_spec("1080p", None), "bv*[height<=1080]+ba/b");
    assert_eq!(hls_format_spec("mystery", None), "bv*[height<=1080]+ba/b");
    assert_eq!(hls_format_spec("1080p", Some("hls-99")), "hls-99+ba/b");
    assert_eq!(
        hls_format_spec("1080p", Some("   ")),
        "bv*[height<=1080]+ba/b"
    );
}

#[test]
fn ytdlp_template_parses_absolute_counts() {
    let p = parse_ytdlp_template("[Grab];downloading;47448064;52428800;52428800;1293945;4")
        .expect("progress");
    assert_eq!(p.downloaded, Some(47448064));
    assert_eq!(p.total, Some(52428800));
    assert!((p.speed.expect("speed") - 1293945.0).abs() < 1e-6);
    assert_eq!(p.eta, Some(4));
    // Estimate folds in when the total is unknown; NA means unknown.
    let p = parse_ytdlp_template("[Grab];downloading;100;NA;200;NA;Unknown").expect("progress");
    assert_eq!(p.downloaded, Some(100));
    assert_eq!(p.total, Some(200));
    assert_eq!(p.speed, None);
    assert_eq!(p.eta, None);
    // Finished lines still carry final counts; error lines carry none.
    let p = parse_ytdlp_template("[Grab];finished;52428800;52428800;52428800;NA;0").expect("done");
    assert_eq!(p.downloaded, Some(52428800));
    assert_eq!(parse_ytdlp_template("[Grab];error;NA;NA;NA;NA;NA"), None);
    // Foreign lines never parse, including bare paths, so the after_move sniffer keeps working.
    assert_eq!(parse_ytdlp_template("[download] 45.2% of 50MiB"), None);
    assert_eq!(parse_ytdlp_template("[Merger] Merging"), None);
    assert_eq!(parse_ytdlp_template("[info] x"), None);
    assert_eq!(parse_ytdlp_template("garbage"), None);
    assert_eq!(parse_ytdlp_template("/tmp/grab/abc.mp4"), None);
    assert!(is_ytdlp_merge_line("[Merger] Merging formats"));
    assert!(is_ytdlp_merge_line("[ExtractAudio] Destination"));
    assert!(!is_ytdlp_merge_line("[download] 10% of 1MiB"));
    assert_eq!(
        parse_ytdlp_after_move("/tmp/grab/abc.mp4"),
        Some("/tmp/grab/abc.mp4")
    );
    assert_eq!(parse_ytdlp_after_move("[download] 10%"), None);
    assert_eq!(parse_ytdlp_after_move(""), None);
    let p = parse_ytdlp_template("[Grab];downloading;805306368;1610612736;1610612736;1048576;768")
        .expect("progress");
    assert_eq!(p.downloaded, Some(805306368));
    assert_eq!(p.total, Some(1610612736));
    assert_eq!(p.eta, Some(768));
}

#[test]
fn grid_needs_rebuild_on_any_growth() {
    // Any refined-up total rebuilds: a stale smaller grid would read full
    // while the bar still shows partial. Zero/unknown never rebuilds, and
    // flat or downward wobble keeps the grid (the bar's total is a sticky
    // max, so marks and bar stay consistent there).
    assert!(grid_needs_rebuild(None, 640_000_000));
    assert!(grid_needs_rebuild(Some(640_000_000), 1_100_000_000));
    assert!(grid_needs_rebuild(Some(640_000_000), 640_000_001));
    assert!(!grid_needs_rebuild(Some(640_000_000), 640_000_000));
    assert!(!grid_needs_rebuild(Some(1_100_000_000), 640_000_000));
    assert!(!grid_needs_rebuild(Some(640_000_000), 0));
    assert!(!grid_needs_rebuild(None, 0));
}

#[test]
fn piece_marks_cover_prefix_once() {
    let mut marked = 0u64;
    assert!(piece_marks(100, &mut marked, 50).is_empty());
    assert_eq!(piece_marks(100, &mut marked, 250), vec![0, 1]);
    assert!(piece_marks(100, &mut marked, 250).is_empty());
    assert_eq!(
        piece_marks(100, &mut marked, 1000),
        vec![2, 3, 4, 5, 6, 7, 8, 9]
    );
    assert!(piece_marks(0, &mut marked, 1000).is_empty());
}

#[test]
fn leg_changed_ignores_wobble_restarts_legs() {
    // First known total inits the map; unknown (0) never does.
    assert!(leg_changed(None, 0, 9_000_000, Some(0)));
    assert!(!leg_changed(None, 0, 0, Some(0)));
    assert!(leg_changed(Some(0), 0, 9_000_000, Some(0)));
    // Stable totals across lines: same file, no restart.
    assert!(!leg_changed(
        Some(9_000_000),
        4_000_000,
        9_000_000,
        Some(4_100_000)
    ));
    // HLS estimate wobble with climbing bytes is the same file, never a restart (used to clear the block map).
    assert!(!leg_changed(
        Some(9_000_000),
        1_000_000,
        19_000_000,
        Some(1_100_000)
    ));
    assert!(!leg_changed(
        Some(19_000_000),
        8_000_000,
        14_600_000,
        Some(8_200_000)
    ));
    // New leg (audio after video): much smaller total AND downloaded back near zero.
    assert!(leg_changed(
        Some(19_000_000),
        19_000_000,
        2_000_000,
        Some(0)
    ));
    assert!(leg_changed(
        Some(19_000_000),
        19_000_000,
        2_000_000,
        Some(100_000)
    ));
    // Total drop without a byte reset is wobble, not a leg.
    assert!(!leg_changed(
        Some(19_000_000),
        19_000_000,
        2_000_000,
        Some(18_900_000)
    ));
    // Bigger second leg with reset bytes restarts on a fresh grid.
    assert!(leg_changed(
        Some(19_000_000),
        19_000_000,
        40_000_000,
        Some(0)
    ));
    // Same growth with continuous bytes is estimate refinement.
    assert!(!leg_changed(
        Some(19_000_000),
        19_000_000,
        40_000_000,
        Some(19_000_000)
    ));
    // Unknown bytes count as reset; a stable total never restarts regardless.
    assert!(leg_changed(Some(19_000_000), 19_000_000, 2_000_000, None));
    assert!(!leg_changed(Some(19_000_000), 8_000_000, 19_000_000, None));
}

#[test]
fn picker_lists_hls_gap_heights() {
    let video = test_video(serde_json::json!([
        test_format_full("v720", "avc1", "none", Some(720), None, "https", false),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
    ]));
    let opts = video_format_options(&video, true);
    assert_eq!(
        opts.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["h1080", "v720"]
    );
    assert_eq!(opts[0].label, "1080p · HLS");
}

#[test]
fn video_info_carries_live_flag() {
    let mut video = test_video(serde_json::json!([]));
    assert!(!VideoInfo::from(&video, "https://x.com/u/status/1", true).is_live);
    video.is_live = Some(true);
    assert!(VideoInfo::from(&video, "https://x.com/u/status/1", true).is_live);
}

#[cfg(unix)]
#[test]
fn codec_rank_orders_newest_first() {
    assert!(codec_rank("av01.0.08M.08", true) < codec_rank("vp9", true));
    assert!(codec_rank("VP9", true) < codec_rank("hev1.1.6.L93", true));
    assert!(codec_rank("hvc1", true) < codec_rank("avc1.640028", true));
    assert!(codec_rank("avc1.640028", true) < codec_rank("theora", true));
    assert_eq!(codec_rank("av1", true), codec_rank("av01.0.05M.08", true));
}

#[test]
fn codec_rank_compatible_prefers_h264() {
    use yt_dlp::model::selector::VideoCodecPreference;
    assert!(codec_rank("avc1.640028", false) < codec_rank("vp9", false));
    assert!(codec_rank("VP9", false) < codec_rank("hev1.1.6.L93", false));
    assert!(codec_rank("hvc1", false) < codec_rank("av01.0.08M.08", false));
    assert!(codec_rank("av01.0.08M.08", false) < codec_rank("theora", false));
    assert_eq!(
        codec_priority_index("compatible"),
        1,
        "unknown values fall back to newest, not compatible"
    );
    assert_eq!(codec_priority_index("mystery"), 0);
    assert_eq!(codec_priority_value(9), CODEC_PRIORITY_NEWEST);
    assert!(matches!(
        codec_preference(false),
        VideoCodecPreference::AVC1
    ));
    assert!(matches!(codec_preference(true), VideoCodecPreference::AV1));
}

#[test]
fn video_format_options_empty_without_fetchable_video() {
    let video = test_video(serde_json::json!([test_format_full(
        "a-only",
        "none",
        "opus",
        None,
        Some(8_000_000),
        "https",
        false
    ),]));
    assert!(video_format_options(&video, true).is_empty());
}

#[test]
fn find_usable_format_matches() {
    let formats: Vec<yt_dlp::model::format::Format> = serde_json::from_value(serde_json::json!([
        test_format_full(
            "137",
            "avc1.640028",
            "none",
            Some(1080),
            Some(100),
            "https",
            false
        ),
        test_format_full(
            "hls",
            "avc1.640028",
            "none",
            Some(1080),
            Some(100),
            "m3u8_native",
            false
        ),
    ]))
    .unwrap();
    assert_eq!(
        find_usable_format(&formats, "137").map(|s| s.format_id),
        Some("137".to_string())
    );
    assert!(find_usable_format(&formats, "nope").is_none());
    assert!(find_usable_format(&formats, "hls").is_none());
}

#[test]
fn video_source_page_carries_format_pin() {
    let src = VideoSource::Page {
        page_url: "https://vimeo.com/99".into(),
        media_url: None,
        expires_at: None,
        quality: "1080p".into(),
        audio_only: false,
        is_live: false,
        video_format_id: Some("137".into()),
        playlist_item_id: None,
    };
    let json = serde_json::to_string(&src).unwrap();
    assert!(json.contains("\"video_format_id\":\"137\""));
    let back: VideoSource = serde_json::from_str(&json).unwrap();
    assert_eq!(back, src);
}

// ── distro package managers ──────────────────────────────────────────

#[test]
fn distro_packages_known_ids() {
    let fedora = "ID=fedora\nNAME=Fedora\n";
    assert_eq!(
        distro_packages(fedora).map(|d| (d.distro, d.install_all)),
        Some((
            "Fedora".to_string(),
            "sudo dnf install yt-dlp ffmpeg quickjs-ng".to_string(),
        ))
    );
    let ubuntu = "ID=ubuntu\nID_LIKE=debian\nNAME=Ubuntu\n";
    assert_eq!(
        distro_packages(ubuntu).map(|d| d.install_all),
        Some("sudo apt install yt-dlp ffmpeg quickjs-ng".to_string())
    );
    let arch = "ID=arch\nNAME=Arch\n";
    assert_eq!(
        distro_packages(arch).map(|d| d.install_all),
        Some("sudo pacman -S yt-dlp ffmpeg quickjs-ng".to_string())
    );
}

#[test]
fn distro_packages_quickjs() {
    // quickjs-ng joins the one-line command where a distro package is known.
    let neon = "ID=neon\nID_LIKE=\"ubuntu debian\"\nNAME=KDE neon\n";
    assert_eq!(
        distro_packages(neon).map(|d| d.install_all),
        Some("sudo apt install yt-dlp ffmpeg quickjs-ng".to_string())
    );
    // No known distro package (openSUSE, Void, Solus): left out rather than
    // showing a command that would fail.
    let void = "ID=void\nNAME=Void\n";
    assert_eq!(
        distro_packages(void).map(|d| d.install_all),
        Some("sudo xbps-install -S yt-dlp ffmpeg".to_string())
    );
    let tumbleweed = "ID=opensuse-tumbleweed\nNAME=openSUSE Tumbleweed\n";
    assert_eq!(
        distro_packages(tumbleweed).map(|d| d.install_all),
        Some("sudo zypper install yt-dlp ffmpeg".to_string())
    );
}

#[test]
fn distro_packages_id_like_fallback() {
    // Unknown derivative riding a known family (e.g. a Ubuntu respin
    // with its own ID) still resolves through ID_LIKE.
    let neon = "ID=neon\nID_LIKE=\"ubuntu debian\"\nNAME=KDE neon\n";
    let found = distro_packages(neon).expect("ID_LIKE fallback");
    assert_eq!(found.distro, "KDE neon");
    assert_eq!(
        found.install_all,
        "sudo apt install yt-dlp ffmpeg quickjs-ng"
    );
}

#[test]
fn distro_packages_unknown_is_none() {
    assert_eq!(distro_packages(""), None);
    assert_eq!(distro_packages("NAME=No ID here\n"), None);
    assert_eq!(distro_packages("ID=mysteryos\nNAME=Mystery\n"), None);
}

// ── browser cookies ──────────────────────────────────────────────────

#[test]
fn cookies_browser_index_round_trip() {
    assert_eq!(cookies_browser_index("none"), 0);
    assert_eq!(cookies_browser_index("firefox"), 5);
    assert_eq!(cookies_browser_value(5), "firefox");
    assert_eq!(cookies_browser_value(99), "none");
    // Unknown values fall back to off, never to a browser.
    assert_eq!(cookies_browser_index("chromium-beta"), 0);
    assert_eq!(cookies_browser_index(""), 0);
    assert_eq!(cookies_browser_labels().len(), COOKIES_BROWSERS.len());
}

#[test]
fn ytdlp_update_available_compares_releases() {
    assert!(ytdlp_update_available("2026.08.19", "2026.09.01"));
    assert!(!ytdlp_update_available("2026.09.01", "2026.09.01"));
    assert!(!ytdlp_update_available("2026.09.02", "2026.09.01"));
    // Unparseable tags never nag.
    assert!(!ytdlp_update_available("2026.09.01", "nightly"));
    assert!(!ytdlp_update_available("nightly", "2026.09.01"));
    assert!(!ytdlp_update_available("", ""));
}

#[test]
fn sparse_x_com_json_parses_to_video() {
    // x.com omits top-level scalars and ships sparse nested objects; every
    // one used to abort the preview with a missing-field JSON error.
    let mut v = serde_json::json!({
        "id": "abc",
        "title": "T",
        "webpage_url": "https://x.com/u/status/abc",
        "duration": 7.66599888973779,
        "view_count": 1200.0,
        "thumbnails": [{"url": "http://e/1.jpg"}],
        "chapters": [{"title": "c"}],
        "tags": ["t"],
        "subtitles": {"en": [{"url": "http://e/s"}]},
        "heatmap": [{"start_time": 0.0}],
        "formats": [
            {
                "url": "https://e/v.mp4",
                "http_headers": {},
                "fragments": [{"url": "http://e/f"}],
                "filesize": 180000000.0,
            },
            {"format_id": "hls-1", "protocol": "m3u8_native", "url": "https://e/m.m3u8"},
        ],
    });
    sanitize_video_json(&mut v);
    let video: Video = serde_json::from_value(v).expect("sparse page parses");
    assert_eq!(video.live_status, "");
    assert_eq!(video.age_limit, 0);
    assert_eq!(video.duration, Some(7));
    assert!(!video.playable_in_embed);
    assert!(video.thumbnails.is_empty());
    assert!(video.chapters.is_empty());
    assert_eq!(video.formats.len(), 2);
    assert_eq!(video.formats[0].format_id, "");
    assert_eq!(video.formats[1].format_id, "hls-1");
    // Missing top-level scalars get defaults too.
    let mut bare = serde_json::json!({"formats": []});
    sanitize_video_json(&mut bare);
    let bare: Video = serde_json::from_value(bare).expect("bare page parses");
    assert_eq!(bare.id, "");
    assert_eq!(bare.extractor_info.extractor, "");
}

#[test]
fn cookies_browser_spec_falls_back_to_bare_name() {
    // Hermetic: an empty config dir, so no real dev-machine profile leaks into
    // the assertion and the spec stays the bare name yt-dlp resolves itself.
    let dir = std::env::temp_dir().join(format!("grab-nocookies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".config")).unwrap();
    let _env = ScopedHostConfig::apply(&dir.join(".config"));
    assert_eq!(cookies_browser_spec("chrome"), Some("chrome".to_string()));
    assert_eq!(cookies_browser_spec("firefox"), Some("firefox".to_string()));
    assert_eq!(cookies_browser_spec("brave"), Some("brave".to_string()));
    assert_eq!(cookies_browser_spec("none"), None);
    assert_eq!(cookies_browser_spec(""), None);
    assert_eq!(cookies_browser_spec("mystery"), None);
    drop(_env);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scoped HOST_XDG_CONFIG_HOME override (its derived home dir is pinned
/// too), restored on drop. Serial suite only: env is process-global, same
/// precedent as ScopedEnv.
struct ScopedHostConfig {
    saved: Option<std::ffi::OsString>,
}

impl ScopedHostConfig {
    fn apply(config_home: &std::path::Path) -> Self {
        let saved = std::env::var_os("HOST_XDG_CONFIG_HOME");
        // SAFETY: serial suite; restored on drop below.
        unsafe {
            std::env::set_var("HOST_XDG_CONFIG_HOME", config_home);
        }
        Self { saved }
    }
}

impl Drop for ScopedHostConfig {
    fn drop(&mut self) {
        // SAFETY: same serial-suite context as apply().
        unsafe {
            match &self.saved {
                Some(v) => std::env::set_var("HOST_XDG_CONFIG_HOME", v),
                None => std::env::remove_var("HOST_XDG_CONFIG_HOME"),
            }
        }
    }
}

#[test]
fn browser_profile_prefers_default_over_numbered() {
    let dir = std::env::temp_dir().join(format!("grab-chromium-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for profile in ["Default", "Profile 1"] {
        let p = dir.join("BraveSoftware/Brave-Browser").join(profile);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("Cookies"), b"sqlite").unwrap();
    }
    let found = browser_profile_dir_in(&dir, &dir, "brave").expect("brave resolves");
    assert_eq!(found, dir.join("BraveSoftware/Brave-Browser/Default"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn chromium_subdirs_cover_browser_channels() {
    // One entry per family: stable, beta, nightly/canary, dev and known
    // rebranded variants resolve under it, most common first.
    assert_eq!(
        chromium_subdirs("brave"),
        &[
            "BraveSoftware/Brave-Browser",
            "BraveSoftware/Brave-Browser-Beta",
            "BraveSoftware/Brave-Browser-Nightly",
            "BraveSoftware/Brave-Origin-Beta",
            "BraveSoftware/Brave-Origin-Nightly",
            "BraveSoftware/Brave-Browser-Origin-Nightly",
        ]
    );
    assert!(chromium_subdirs("edge").contains(&"microsoft-edge-canary"));
    assert!(chromium_subdirs("opera").contains(&"opera-developer"));
    assert!(chromium_subdirs("vivaldi").contains(&"vivaldi-snapshot"));
    assert!(chromium_subdirs("mystery").is_empty());
}

#[test]
fn browser_override_dirs_match_lookup_roots() {
    // Every dir the override grants must be a root the cookie lookup probes:
    // chromium families mirror chromium_subdirs under ~/.config, firefox and
    // zen mirror browser_profile_dir_in's candidates.
    assert_eq!(
        browser_override_dirs("brave"),
        [
            "~/.config/BraveSoftware/Brave-Browser",
            "~/.config/BraveSoftware/Brave-Browser-Beta",
            "~/.config/BraveSoftware/Brave-Browser-Nightly",
            "~/.config/BraveSoftware/Brave-Origin-Beta",
            "~/.config/BraveSoftware/Brave-Origin-Nightly",
            "~/.config/BraveSoftware/Brave-Browser-Origin-Nightly",
        ]
    );
    assert_eq!(browser_override_dirs("whale"), ["~/.config/naver-whale"]);
    assert_eq!(
        browser_override_dirs("firefox"),
        [
            "~/.mozilla/firefox",
            "~/.config/mozilla/firefox",
            "~/snap/firefox/common/.mozilla/firefox",
        ]
    );
    assert_eq!(browser_override_dirs("zen"), ["~/.zen", "~/.config/zen"]);
    assert!(browser_override_dirs("none").is_empty());
    assert!(browser_override_dirs("mystery").is_empty());
}

#[test]
fn browser_override_command_covers_all_channels() {
    assert_eq!(
        browser_override_command_for("chrome", "io.github.houssemko.Grab").as_deref(),
        Some(
            "flatpak override --user --filesystem=~/.config/google-chrome:ro \
             --filesystem=~/.config/google-chrome-beta:ro \
             --filesystem=~/.config/google-chrome-unstable:ro \
             io.github.houssemko.Grab"
        )
    );
    assert_eq!(
        browser_override_command_for("zen", "io.github.houssemko.Grab").as_deref(),
        Some(
            "flatpak override --user --filesystem=~/.zen:ro \
             --filesystem=~/.config/zen:ro io.github.houssemko.Grab"
        )
    );
    assert_eq!(
        browser_override_command_for("firefox", "io.github.houssemko.Grab").as_deref(),
        Some(
            "flatpak override --user --filesystem=~/.mozilla/firefox:ro \
             --filesystem=~/.config/mozilla/firefox:ro \
             --filesystem=~/snap/firefox/common/.mozilla/firefox:ro \
             io.github.houssemko.Grab"
        )
    );
    assert_eq!(
        browser_override_command_for("none", "io.github.houssemko.Grab"),
        None
    );
    assert_eq!(
        browser_override_command_for("mystery", "io.github.houssemko.Grab"),
        None
    );
}

#[test]
fn browser_profile_falls_through_to_beta_channel() {
    // No stable install: a beta-only tree still resolves under the
    // same entry.
    let dir = std::env::temp_dir().join(format!("grab-bravebeta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let p = dir.join("BraveSoftware/Brave-Browser-Beta/Default");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("Cookies"), b"sqlite").unwrap();
    let found = browser_profile_dir_in(&dir, &dir, "brave").expect("brave beta resolves");
    assert_eq!(found, dir.join("BraveSoftware/Brave-Browser-Beta/Default"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn browser_profile_prefers_freshest_channel() {
    // Both channels present: the freshest `Cookies` database wins, so an
    // actively-used beta is no longer shadowed by a stale stable install.
    let dir = std::env::temp_dir().join(format!("grab-bravefresh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let stable = dir.join("BraveSoftware/Brave-Browser/Default");
    let beta = dir.join("BraveSoftware/Brave-Browser-Beta/Default");
    for p in [&stable, &beta] {
        std::fs::create_dir_all(p).unwrap();
        std::fs::write(p.join("Cookies"), b"sqlite").unwrap();
    }
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(stable.join("Cookies"))
        .unwrap()
        .set_modified(stale)
        .unwrap();
    let found = browser_profile_dir_in(&dir, &dir, "brave").expect("brave resolves");
    assert_eq!(found, beta);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn browser_profile_fresh_stable_beats_stale_beta() {
    // Freshness decides, not channel order: a fresh stable still wins over
    // a stale beta.
    let dir = std::env::temp_dir().join(format!("grab-bravestale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let stable = dir.join("BraveSoftware/Brave-Browser/Default");
    let beta = dir.join("BraveSoftware/Brave-Browser-Beta/Default");
    for p in [&stable, &beta] {
        std::fs::create_dir_all(p).unwrap();
        std::fs::write(p.join("Cookies"), b"sqlite").unwrap();
    }
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(beta.join("Cookies"))
        .unwrap()
        .set_modified(stale)
        .unwrap();
    let found = browser_profile_dir_in(&dir, &dir, "brave").expect("brave resolves");
    assert_eq!(found, stable);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn browser_profile_tied_mtimes_keep_channel_order() {
    // Equal mtimes: the stable sort keeps channel order, so the outcome
    // stays deterministic (stable first).
    let dir = std::env::temp_dir().join(format!("grab-bravetie-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let pinned = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for sub in [
        "BraveSoftware/Brave-Browser/Default",
        "BraveSoftware/Brave-Browser-Beta/Default",
    ] {
        let p = dir.join(sub);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("Cookies"), b"sqlite").unwrap();
        std::fs::File::options()
            .write(true)
            .open(p.join("Cookies"))
            .unwrap()
            .set_modified(pinned)
            .unwrap();
    }
    let found = browser_profile_dir_in(&dir, &dir, "brave").expect("brave resolves");
    assert_eq!(found, dir.join("BraveSoftware/Brave-Browser/Default"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn browser_profile_unknown_browser_resolves_nothing() {
    let dir = std::env::temp_dir().join(format!("grab-nobrowser-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(browser_profile_dir_in(&dir, &dir, "mystery"), None);
    assert_eq!(browser_profile_dir_in(&dir, &dir, "none"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn firefox_profile_prefers_default_section() {
    let dir = std::env::temp_dir().join(format!("grab-firefox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let base = dir.join(".mozilla/firefox");
    for profile in ["aaaa1111.other", "bbbb2222.main"] {
        let p = base.join(profile);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cookies.sqlite"), b"sqlite").unwrap();
    }
    std::fs::write(
        base.join("profiles.ini"),
        "[Profile0]\nName=other\nIsRelative=1\nPath=aaaa1111.other\n\n\
         [Profile1]\nName=main\nIsRelative=1\nPath=bbbb2222.main\nDefault=1\n",
    )
    .unwrap();
    let found = browser_profile_dir_in(&dir.join(".config"), &dir, "firefox").expect("ff resolves");
    assert_eq!(found, base.join("bbbb2222.main"));
    let spec = {
        let _env = ScopedHostConfig::apply(&dir.join(".config"));
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        cookies_browser_spec("firefox")
    };
    assert_eq!(spec, Some(format!("firefox:{}", found.display())));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zen_profile_resolves_under_dot_zen_and_uses_firefox_spec() {
    let dir = std::env::temp_dir().join(format!("grab-zen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let base = dir.join(".zen");
    let profile = base.join("abc123.default");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("cookies.sqlite"), b"sqlite").unwrap();
    std::fs::write(
        base.join("profiles.ini"),
        "[Profile0]\nName=default\nIsRelative=1\nPath=abc123.default\nDefault=1\n",
    )
    .unwrap();
    let found = browser_profile_dir_in(&dir.join(".config"), &dir, "zen").expect("zen resolves");
    assert_eq!(found, profile);
    let spec = {
        let _env = ScopedHostConfig::apply(&dir.join(".config"));
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        cookies_browser_spec("zen")
    };
    // yt-dlp has no "zen" browser: the Firefox extractor reads the profile.
    assert_eq!(spec, Some(format!("firefox:{}", found.display())));
    assert_eq!(cookies_browser_index("zen"), 9);
    assert_eq!(cookies_browser_value(9), "zen");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cookies_browser_spec_pins_chromium_profile_path() {
    let dir = std::env::temp_dir().join(format!("grab-pin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let profile = dir.join(".config/BraveSoftware/Brave-Browser/Default");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("Cookies"), b"sqlite").unwrap();
    let _env = ScopedHostConfig::apply(&dir.join(".config"));
    assert_eq!(
        cookies_browser_spec("brave"),
        Some(format!("brave:{}", profile.display()))
    );
    // A browser with no profile on disk keeps the bare-name fallback.
    assert_eq!(cookies_browser_spec("chrome"), Some("chrome".to_string()));
    drop(_env);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── tool search order ────────────────────────────────────────────────

/// Scoped PATH + XDG_DATA_HOME override, restored on drop. Serial suite
/// only: the environment is process-global (see the queue-file helper).
struct ScopedEnv {
    path: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
}

impl ScopedEnv {
    fn apply(path: &str, xdg: &std::path::Path) -> Self {
        let saved = Self {
            path: std::env::var_os("PATH"),
            xdg: std::env::var_os("XDG_DATA_HOME"),
        };
        // SAFETY: serial suite; restored on drop below.
        unsafe {
            std::env::set_var("PATH", path);
            std::env::set_var("XDG_DATA_HOME", xdg);
        }
        saved
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        // SAFETY: same serial-suite context as apply().
        unsafe {
            match &self.path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            match &self.xdg {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }
}

fn fake_executable(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn user_installed_tools_win_over_bundle() {
    // User dir holds our own binaries, PATH leads nowhere (no Flatpak bundle
    // on a dev host); same order lets a user Update override the bundle.
    let dir = std::env::temp_dir().join(format!("grab-userlibs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let libs = dir.join("xdg").join("grab").join("libs");
    std::fs::create_dir_all(&libs).unwrap();
    fake_executable(&libs, "yt-dlp");
    fake_executable(&libs, "ffmpeg");
    let _env = ScopedEnv::apply("/nonexistent-grab-test", &dir.join("xdg"));
    let found = resolve_libraries().expect("user tools resolve");
    assert_eq!(found.youtube, libs.join("yt-dlp"));
    assert_eq!(found.ffmpeg, libs.join("ffmpeg"));
    drop(_env);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn toolchain_dir_prefers_complete_toolchain() {
    // A dir with only ffmpeg (an older user-lib install) must not shadow a
    // later dir shipping both: yt-dlp resolves ffprobe from --ffmpeg-location.
    let base = std::env::temp_dir().join(format!("grab-toolchain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let lone = base.join("lone");
    let full = base.join("full");
    let empty = base.join("empty");
    for d in [&lone, &full, &empty] {
        std::fs::create_dir_all(d).unwrap();
    }
    fake_executable(&lone, "ffmpeg");
    fake_executable(&full, "ffmpeg");
    fake_executable(&full, "ffprobe");
    assert_eq!(
        toolchain_dir_in(&[lone.clone(), full.clone(), empty]),
        Some(full.clone())
    );
    // No complete toolchain anywhere: None, and the caller falls back
    // to the resolved ffmpeg's own dir (previous behavior).
    assert_eq!(toolchain_dir_in(&[lone]), None);
    let _ = std::fs::remove_dir_all(&base);
}

/// Build a zip archive with the given (entry name, contents) pairs.
fn make_tool_zip(archive: &std::path::Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(archive).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, contents) in entries {
        zip.start_file(*name, options).unwrap();
        std::io::Write::write_all(&mut zip, contents).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn extract_ffmpeg_toolchain_pulls_both_binaries() {
    let base = std::env::temp_dir().join(format!("grab-fftools-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("out");
    std::fs::create_dir_all(&dir).unwrap();
    let archive = base.join("ffmpeg-linux-x86_64.zip");
    // Nested layout (bin/-style); the extractor matches by file name.
    make_tool_zip(
        &archive,
        &[("bin/ffmpeg", b"fake-ffmpeg"), ("ffprobe", b"fake-ffprobe")],
    );
    let ffmpeg = extract_ffmpeg_toolchain(&archive, &dir).expect("extracts");
    assert_eq!(ffmpeg, dir.join("ffmpeg"));
    assert_eq!(std::fs::read(dir.join("ffprobe")).unwrap(), b"fake-ffprobe");
    // Executable bits are set and the archive is cleaned up.
    use std::os::unix::fs::PermissionsExt as _;
    for tool in ["ffmpeg", "ffprobe"] {
        let mode = std::fs::metadata(dir.join(tool))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "{tool} is executable");
    }
    assert!(!archive.exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn extract_ffmpeg_toolchain_errors_without_ffmpeg() {
    let base = std::env::temp_dir().join(format!("grab-fftools-nofmpeg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("out");
    std::fs::create_dir_all(&dir).unwrap();
    let archive = base.join("ffmpeg-linux-x86_64.zip");
    make_tool_zip(&archive, &[("ffprobe", b"fake-ffprobe")]);
    assert!(extract_ffmpeg_toolchain(&archive, &dir).is_err());
    let _ = std::fs::remove_dir_all(&base);
}

// ── quickjs provisioning (default JS runtime) ──────────────────────────

#[test]
fn quickjs_download_url_matches_pinned_release_and_arch() {
    let url = quickjs_download_url().expect("test arch is supported");
    let arch = std::env::consts::ARCH;
    assert!(
        url.starts_with("https://github.com/quickjs-ng/quickjs/releases/download/"),
        "unexpected url: {url}"
    );
    assert!(
        url.ends_with(&format!("qjs-linux-{arch}")),
        "unexpected url: {url}"
    );
    assert!(
        !url.ends_with(".zip"),
        "qjs ships as a bare binary, not an archive: {url}"
    );
}

#[test]
fn quickjs_expected_sha256_mirrors_download_url() {
    // The integrity pin must exist exactly where a download URL exists.
    assert_eq!(
        quickjs_expected_sha256().is_some(),
        quickjs_download_url().is_some()
    );
    if let Some(hash) = quickjs_expected_sha256() {
        assert_eq!(hash.len(), 64, "SHA-256 must be 64 hex chars");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "non-hex char in pinned hash"
        );
    }
}

#[test]
fn ytdlp_identity_args_pins_quickjs_for_youtube_with_cookies() {
    let args = ytdlp_identity_args("firefox", None, "https://www.youtube.com/watch?v=abc");
    let pos = args
        .iter()
        .position(|a| a == "--no-js-runtimes")
        .expect("pins js runtimes");
    assert_eq!(args[pos + 1], "--js-runtimes");
    assert_eq!(args[pos + 2], "quickjs");
    // The pin must precede `--`: the page URL stays behind the separator.
    let sep = args
        .iter()
        .position(|a| a == "--")
        .expect("has -- separator");
    assert!(pos < sep, "runtime pin must precede `--`");
}

#[test]
fn ytdlp_identity_args_skips_quickjs_pin_off_youtube() {
    // The runtime pin is YouTube-only: other sites keep yt-dlp's own runtime
    // discovery, so a system runtime is never disabled there.
    for (browser, url) in [
        ("none", "https://vimeo.com/123"),
        ("firefox", "https://x.com/u/status/1"),
        ("none", "https://www.fakeyoutube.com/watch?v=abc"),
    ] {
        let args = ytdlp_identity_args(browser, None, url);
        assert!(
            !args.iter().any(|a| a == "--no-js-runtimes"),
            "no js pin for browser={browser} url={url}"
        );
    }
}

#[test]
fn is_youtube_url_matches_youtube_hosts() {
    for url in [
        "https://www.youtube.com/watch?v=abc",
        "https://m.youtube.com/watch?v=abc",
        "https://youtu.be/abc",
        "https://www.youtube-nocookie.com/embed/abc",
        "HTTPS://YOUTUBE.COM/watch?v=abc",
    ] {
        assert!(is_youtube_url(url), "youtube: {url}");
    }
    for url in [
        "https://vimeo.com/123",
        "https://www.fakeyoutube.com/watch?v=abc",
        "https://youtube.com.evil.example/x",
        "not a url",
        "",
    ] {
        assert!(!is_youtube_url(url), "not youtube: {url}");
    }
}

// ── stream planning (selection gating) ───────────────────────────────

/// TikTok shape: sparse mp4 with neither codec field (typed Unknown,
/// invisible to every crate selector) beside a separate music track.
fn tiktok_like_video() -> yt_dlp::model::Video {
    let sparse_mp4 = serde_json::json!({
        "format": "dl",
        "format_id": "dl",
        "protocol": "https",
        "ext": "mp4",
        "url": "https://cdn.example/dl.mp4",
        "http_headers": {},
    });
    test_video(serde_json::json!([
        sparse_mp4,
        test_format_full("music", "none", "mp4a.40.2", None, None, "https", false),
    ]))
}

/// x.com shape: muxed direct mp4s (AudioVideo type, invisible to the
/// video-only selector) beside HLS variants.
fn x_like_video() -> yt_dlp::model::Video {
    test_video(serde_json::json!([
        test_format_full(
            "http-320",
            "avc1.64001f",
            "mp4a.40.2",
            Some(320),
            None,
            "https",
            false
        ),
        test_format_full(
            "http-720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "https",
            false
        ),
        test_format_full(
            "hls-720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "m3u8_native",
            false
        ),
    ]))
}

#[test]
fn plan_adopts_unknown_video_despite_separate_audio() {
    // The TikTok gating bug: both fallbacks keyed off audio absence, so
    // the music track suppressed them and only the music survived.
    let video = tiktok_like_video();
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "dl");
    assert!(plan.hls_sel.is_none());
}

/// Instagram story shape: direct mp4s carry `vcodec` but no `acodec`
/// (the extractor only writes `acodec: none` when the media is truly
/// audio-less); some entries lack even `vcodec`. All are muxed single
/// files, so the plan must adopt the tallest in-cap one instead of
/// failing on the missing audio split.
fn instagram_story_like_video() -> yt_dlp::model::Video {
    let sparse = |id: &str, height: u32, vcodec: Option<&str>| {
        let mut v = serde_json::json!({
            "format": id,
            "format_id": id,
            "protocol": "https",
            "ext": "mp4",
            "url": format!("https://cdn.example/{id}.mp4"),
            "height": height,
            "http_headers": {},
        });
        if let Some(c) = vcodec {
            v["vcodec"] = serde_json::json!(c);
        }
        v
    };
    test_video(serde_json::json!([
        sparse("s1080", 1080, Some("h264")),
        sparse("s720", 720, Some("h264")),
        sparse("s480a", 480, None),
        sparse("s480b", 480, None),
        sparse("s360", 360, None),
    ]))
}

#[test]
fn plan_adopts_story_mp4_with_absent_acodec() {
    // The highlight bug: the story's direct mp4s were classified
    // video-only for the missing acodec, so the muxed adoption skipped
    // them and the row failed with "No suitable formats found".
    let video = instagram_story_like_video();
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "s1080");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_pinned_hls_wins_over_muxed_adoption() {
    // The x.com shadowing bug: the pin was dropped by the HTTPS-only
    // lookup and the muxed adoption then vetoed the HLS path.
    let video = x_like_video();
    let plan = plan_streams(&video, "720p", false, Some("hls-720"), true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none());
    let hls = plan.hls_sel.expect("pinned hls");
    assert_eq!(hls.height, Some(720));
}

#[test]
fn plan_muxed_only_still_adopts_without_pin() {
    // No pin, no splits: the muxed file adopts as before (HLS precedence
    // unchanged) — but at the requested height, not first-in-extractor-order.
    let video = x_like_video();
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "http-720");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_muxed_adoption_honors_height_cap() {
    // First-in-order used to win regardless of quality (x.com lists
    // ascending); the cap now picks smallest-at-or-above, tallest if none.
    let video = test_video(serde_json::json!([
        test_format_full(
            "m320",
            "avc1.64001f",
            "mp4a.40.2",
            Some(320),
            None,
            "https",
            false
        ),
        test_format_full(
            "m720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "https",
            false
        ),
        test_format_full(
            "m1080",
            "avc1.64001f",
            "mp4a.40.2",
            Some(1080),
            None,
            "https",
            false
        ),
    ]));
    for (quality, want) in [
        ("best", "m1080"),
        ("1080p", "m1080"),
        ("720p", "m720"),
        ("480p", "m720"),
    ] {
        let plan = plan_streams(&video, quality, false, None, true, 1);
        assert_eq!(
            plan.audio_sel.expect("adopted").format_id,
            want,
            "quality {quality}"
        );
    }
}

#[test]
fn plan_stale_hls_pin_degrades_to_muxed_adoption() {
    // A vanished HLS pin behaves like no pin: preset, then adoption
    // (at the dialog-picked height, not the lowest listing).
    let video = x_like_video();
    let plan = plan_streams(&video, "1080p", false, Some("gone"), true, 1);
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "http-720");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_splits_stay_split() {
    let video = test_video(serde_json::json!([
        test_format_full("v", "avc1.640028", "none", Some(1080), None, "https", false),
        test_format_full("a", "none", "mp4a.40.2", None, None, "https", false),
    ]));
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert_eq!(plan.video_sel.expect("video").format_id, "v");
    assert_eq!(plan.audio_sel.expect("audio").format_id, "a");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_audio_only_request_skips_video() {
    // Dialog audio-only choice: video selection skipped, best audio
    // kept even when a muxed file is also listed, HLS path never runs.
    let video = test_video(serde_json::json!([
        test_format_full("a", "none", "mp4a.40.2", None, None, "https", false),
        test_format_full(
            "m",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "https",
            false
        ),
    ]));
    let plan = plan_streams(&video, "1080p", true, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("audio").format_id, "a");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_hls_preset_still_serves_hls_only_pages() {
    let video = test_video(serde_json::json!([
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
    ]));
    let plan = plan_streams(&video, "720p", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none());
    assert_eq!(plan.hls_sel.expect("preset hls").height, Some(1080));
}

#[test]
fn plan_audio_only_request_reaches_hls() {
    // Audio-only on an HLS-only page must yield the HLS variant for the
    // extract path — never fail the row as unavailable.
    let video = test_video(serde_json::json!([
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
    ]));
    let plan = plan_streams(&video, "720p", true, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none());
    assert_eq!(plan.hls_sel.expect("hls for extract").height, Some(1080));
}

// ── quality for height ───────────────────────────────────────────────

#[test]
fn quality_for_height_buckets() {
    assert_eq!(quality_for_height(2160), "2160p");
    assert_eq!(quality_for_height(1440), "1440p");
    assert_eq!(quality_for_height(1080), "1080p");
    assert_eq!(quality_for_height(720), "720p");
    assert_eq!(quality_for_height(480), "480p");
    // Odd heights round to the closest bucket (ties up); out-of-range clamps, so the result is always recognized.
    assert_eq!(quality_for_height(632), "720p");
    assert_eq!(quality_for_height(900), "1080p");
    assert_eq!(quality_for_height(100), "480p");
    assert_eq!(quality_for_height(5000), "2160p");
    for h in [240, 360, 480, 720, 1080, 1440, 2160] {
        assert!(
            VIDEO_QUALITY_VALUES.contains(&quality_for_height(h)),
            "bucket for {h}"
        );
    }
}

// ── HLS candidate ordering ───────────────────────────────────────────

#[test]
fn hls_selection_ignores_extractor_order() {
    // Tallest-first extractor order must still resolve to the smallest height at or above the cap.
    let formats: Vec<yt_dlp::model::format::Format> = serde_json::from_value(serde_json::json!([
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h720",
            "avc1",
            "mp4a.40.2",
            Some(720),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
    ]))
    .unwrap();
    assert_eq!(
        select_hls_format(&formats, Some(720))
            .expect("hls")
            .format_id,
        "h720"
    );
    assert_eq!(
        select_hls_format(&formats, Some(480))
            .expect("hls")
            .format_id,
        "h480"
    );
    // Above every variant still takes the tallest.
    assert_eq!(
        select_hls_format(&formats, Some(2160))
            .expect("hls")
            .format_id,
        "h1080"
    );
}

// ── binary part downloads ────────────────────────────────────────────

fn direct_test_job() -> VideoJob {
    VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        playlist_item_id: None,
        quality: "720p".into(),
        audio_only: false,
        dest: std::path::PathBuf::from("/tmp/dl/v.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    }
}
#[test]
fn part_fallback_specs() {
    assert_eq!(part_fallback_spec("720p", true, false), "bv*[height<=720]");
    assert_eq!(part_fallback_spec("best", true, false), "bv*");
    assert_eq!(part_fallback_spec("720p", false, true), "ba/b");
    // Adopted singles degrade to best-single, never bare audio; genuine audio legs prefer audio.
    assert_eq!(part_fallback_spec("720p", false, false), "b");
}

// ── extraction spawn (no crate executor) ─────────────────────────────

#[test]
fn fetch_video_page_parses_dump_json() {
    // Fake yt-dlp emitting dump JSON: proves spawn, drain and parse without network.
    let dir = std::env::temp_dir().join(format!("grab-fakeextract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("info.json"),
        serde_json::json!({
            "id": "abc",
            "title": "T",
            "age_limit": 0,
            "live_status": "not_live",
            "playable_in_embed": true,
            "extractor": "generic",
            "extractor_key": "Generic",
            "_version": {"version": "2026.08.19", "repository": "yt-dlp"},
            "formats": [],
        })
        .to_string(),
    )
    .unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(&bin, "#!/bin/sh\ncat \"$(dirname \"$0\")/info.json\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let FetchedVideo::Single(video) = crate::runtime::tokio_rt()
        .block_on(fetch_video_page(
            &bin,
            "https://example.com/v",
            "none",
            std::time::Duration::from_secs(30),
            None,
            None,
        ))
        .expect("fake extract parses")
    else {
        panic!("single-video dump must parse as Single");
    };
    assert!(video.formats.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ── shared yt-dlp identity argv ──────────────────────────────────────

#[test]
fn identity_args_order_and_trim() {
    // Player-client workaround, cookies, trimmed UA, then `--` + page, identical every spawn.
    let argv = ytdlp_identity_args("none", Some("  Grab/1  "), "https://x.com/u/status/1");
    assert_eq!(
        argv,
        vec![
            "--extractor-args".to_string(),
            "youtube:player_client=-web".to_string(),
            "--user-agent".to_string(),
            "Grab/1".to_string(),
            "--".to_string(),
            "https://x.com/u/status/1".to_string(),
        ]
    );
    // Blank UA is omitted, never sent empty.
    let argv = ytdlp_identity_args("none", None, "https://x.com/u/status/1");
    assert_eq!(
        argv,
        vec![
            "--extractor-args".to_string(),
            "youtube:player_client=-web".to_string(),
            "--".to_string(),
            "https://x.com/u/status/1".to_string(),
        ]
    );
}

#[test]
fn identity_args_excludes_web_player_client() {
    // SABR workaround survives every spawn: extractor-arg pair always precedes `--`.
    let argv = ytdlp_identity_args("firefox", None, "https://youtu.be/abc");
    let pos = argv
        .iter()
        .position(|a| a == "--extractor-args")
        .expect("extractor-args flag present");
    assert_eq!(argv[pos + 1], "youtube:player_client=-web");
    assert!(argv.iter().position(|a| a == "--").unwrap() > pos + 1);
}

// ── best-overall muxed vs HLS ────────────────────────────────────────

/// Direct muxed files top out below the tallest HLS variant (the x.com shape Best undershot before the override).
fn muxed_below_hls_video() -> yt_dlp::model::Video {
    test_video(serde_json::json!([
        test_format_full(
            "m320",
            "avc1.64001f",
            "mp4a.40.2",
            Some(320),
            None,
            "https",
            false
        ),
        test_format_full(
            "m720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "https",
            false
        ),
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
    ]))
}

#[test]
fn plan_best_prefers_taller_hls_over_muxed() {
    let video = muxed_below_hls_video();
    let plan = plan_streams(&video, "best", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none(), "adoption yields to the variant");
    assert_eq!(plan.hls_sel.expect("hls wins").height, Some(1080));
}

#[test]
fn plan_cap_blocks_taller_hls() {
    // Capped 720p leaves the direct 720p file standing; capped 1080p lets the taller variant win.
    let video = muxed_below_hls_video();
    let plan = plan_streams(&video, "720p", false, None, true, 1);
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "m720");
    assert!(plan.hls_sel.is_none());
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert!(plan.audio_sel.is_none());
    assert_eq!(plan.hls_sel.expect("hls wins").height, Some(1080));
}

#[test]
fn plan_tie_keeps_direct_muxed() {
    // Equal heights: direct-file precedence is unchanged.
    let video = x_like_video();
    let plan = plan_streams(&video, "best", false, None, true, 1);
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "http-720");
    assert!(plan.hls_sel.is_none());
}

// ── default combo selection ──────────────────────────────────────────

fn test_options() -> Vec<VideoFormatOption> {
    [1080u32, 720, 480]
        .iter()
        .map(|h| VideoFormatOption {
            id: format!("v{h}"),
            label: format!("{h}p"),
            height: *h,
        })
        .collect()
}

#[test]
fn default_quality_index_preselects() {
    let opts = test_options();
    // Best and empty listings stay on the first (tallest-first) row.
    assert_eq!(default_quality_index(&opts, "best"), 0);
    assert_eq!(default_quality_index(&[], "720p"), 0);
    // Otherwise the closest listed height wins, 0-based.
    assert_eq!(default_quality_index(&opts, "1080p"), 0);
    assert_eq!(default_quality_index(&opts, "720p"), 1);
    assert_eq!(default_quality_index(&opts, "480p"), 2);
    // Between buckets the nearer height wins, ties go taller.
    assert_eq!(default_quality_index(&opts, "2160p"), 0);
    // Unknown stored values degrade like the extractor selector (1080p).
    assert_eq!(default_quality_index(&opts, "mystery"), 0);
    // Exact ties (odd extractor heights equidistant from the cap) go taller.
    let odd = [840u32, 600]
        .iter()
        .map(|h| VideoFormatOption {
            id: format!("v{h}"),
            label: format!("{h}p"),
            height: *h,
        })
        .collect::<Vec<_>>();
    assert_eq!(default_quality_index(&odd, "720p"), 0);
}

// ── yt-dlp live capture ──────────────────────────────────────────────

fn live_test_job() -> VideoJob {
    VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        playlist_item_id: None,
        quality: "720p".into(),
        audio_only: false,
        dest: std::path::PathBuf::from("/tmp/dl/v.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: true,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    }
}

#[test]
fn live_argv_pins_planner_id_in_mpegts() {
    let job = live_test_job();
    let out = std::path::Path::new("/tmp/staging/live.mp4");
    // Planner-resolved id rides along verbatim; yt-dlp's sort never gets a second vote.
    let argv = live_capture_argv(&job, "h1080", out);
    let f = argv.iter().position(|a| a == "-f").expect("has -f");
    assert_eq!(argv[f + 1], "h1080+ba/b");
    assert!(argv.contains(&"--hls-use-mpegts".to_string()));
    assert!(
        argv.windows(2)
            .any(|w| w == ["--fragment-retries", "infinite"])
    );
    // Record-now means the live edge: no live-from-start, no unbounded wait loop.
    assert!(!argv.iter().any(|a| a == "--live-from-start"));
    assert!(!argv.iter().any(|a| a == "--wait-for-video"));
    let o = argv.iter().position(|a| a == "-o").expect("has -o");
    assert_eq!(argv[o + 1], "/tmp/staging/live.mp4");
    assert_eq!(argv[argv.len() - 2], "--");
    assert_eq!(argv[argv.len() - 1], "https://x.com/u/status/1");
    let mut pinned = live_test_job();
    pinned.video_format_id = Some("h720".into());
    let argv = live_capture_argv(&pinned, "h720", out);
    let f = argv.iter().position(|a| a == "-f").expect("has -f");
    assert_eq!(argv[f + 1], "h720+ba/b");
}

#[test]
fn live_remux_argv_copies_with_fixup_and_carries_provenance() {
    // Map everything, stream-copy, ADTS fixup, faststart.
    // Dests are the `.part` remux targets production passes: ffmpeg can't guess a
    // container from that name, so `-f` must be pinned from the real extension.
    let argv = live_remux_argv(
        std::path::Path::new("/tmp/st/live.mp4.part"),
        std::path::Path::new("/tmp/st/final.mp4.part"),
        false,
        true,
        "https://x.com/watch?v=abc123",
    );
    assert!(argv.windows(2).any(|w| w == ["-map", "0"]));
    assert!(argv.windows(2).any(|w| w == ["-c", "copy"]));
    assert!(argv.windows(2).any(|w| w == ["-bsf:a", "aac_adtstoasc"]));
    assert!(argv.windows(2).any(|w| w == ["-f", "mp4"]));
    assert!(argv.windows(2).any(|w| w == ["-movflags", "+faststart"]));
    assert!(!argv.iter().any(|a| a == "-vn"));
    assert_eq!(argv[argv.len() - 2], "--");
    assert_eq!(argv[argv.len() - 1], "/tmp/st/final.mp4.part");
    // The bare retry drops the fixup.
    let argv = live_remux_argv(
        std::path::Path::new("/tmp/st/live.mp4.part"),
        std::path::Path::new("/tmp/st/final.mp4.part"),
        false,
        false,
        "https://x.com/watch?v=abc123",
    );
    assert!(!argv.iter().any(|a| a == "-bsf:a"));
    // Audio-only maps audio alone; m4a has no ffmpeg muxer, the mp4 muxer writes it.
    let argv = live_remux_argv(
        std::path::Path::new("/tmp/st/live.m4a.part"),
        std::path::Path::new("/tmp/st/final.m4a.part"),
        true,
        false,
        "https://x.com/watch?v=abc123",
    );
    assert!(argv.windows(2).any(|w| w == ["-map", "0:a?"]));
    assert!(argv.windows(2).any(|w| w == ["-f", "mp4"]));
    assert!(!argv.iter().any(|a| a == "-bsf:a"));
}

#[test]
fn live_remux_argv_stamps_the_source_url_so_the_id_survives() {
    // yt-dlp can't post-process a growing capture, so Grab remuxes itself: this is the only provenance pass, set explicitly.
    let argv = live_remux_argv(
        std::path::Path::new("/tmp/st/live.mp4.part"),
        std::path::Path::new("/tmp/st/final.mp4.part"),
        false,
        true,
        "  https://x.com/watch?v=abc123  ",
    );
    let at = argv
        .iter()
        .position(|a| a == "-metadata")
        .expect("live captures must carry their source URL");
    assert_eq!(
        argv[at + 1],
        "comment=https://x.com/watch?v=abc123",
        "the URL must be trimmed: a padded value is a different string to \
         anything reading the tag back"
    );
    // An output option, so it has to precede the output filename.
    let out_at = argv.iter().position(|a| a == "--").expect("output");
    assert!(
        at < out_at,
        "-metadata after the output filename is not an output option and \
         ffmpeg would treat it as a trailing input"
    );

    // A row with no usable page URL gets no half-formed tag.
    let argv = live_remux_argv(
        std::path::Path::new("/tmp/st/live.mp4.part"),
        std::path::Path::new("/tmp/st/final.mp4.part"),
        false,
        true,
        "   ",
    );
    assert!(
        !argv.iter().any(|a| a == "-metadata"),
        "an empty source URL must not become comment="
    );
}

/// Fake yt-dlp for live: emits one progress line, writes the `.part` shell (or fails barren).
fn fake_ytdlp_live(dir: &std::path::Path, fail: bool) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-live");
    std::fs::write(
        &bin,
        if fail {
            "#!/bin/sh\necho \"ERROR: [Video] 1: Got error 404\" >&2\nexit 1\n"
        } else {
            r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo "[Grab];downloading;3;100;100;1000;5"
printf 'tsbytes' > "$out.part"
exit 0
"#
        },
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Fake ffmpeg remux: copies its `-i` input to the trailing output.
fn fake_ffmpeg_copy(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ffmpeg-copy");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
input=""
prev=""
last=""
for a in "$@"; do
    if [ "$prev" = "-i" ]; then input="$a"; fi
    prev="$a"
    last="$a"
done
cat "$input" > "$last"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_capture_adopts_part_and_remuxes() {
    let dir = std::env::temp_dir().join(format!("grab-fakelive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    // Natural end with a `.part` shell: adopted, remuxed, delivered.
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"tsbytes");
    assert!(!staging.exists(), "staging cleaned");
    // The row must leave Resolving the moment capture starts.
    let phases: Vec<String> = {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::engine_msg::EngineMsg::Phase(p) = msg {
                out.push(p);
            }
        }
        out
    };
    assert!(
        phases.iter().any(|p| p.contains("Recording")),
        "phases seen: {phases:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_capture_empty_fails_with_detail() {
    let dir = std::env::temp_dir().join(format!("grab-fakelive-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, true);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert!(e.to_string().contains("404"), "yt-dlp line surfaces: {e}"),
        ok => panic!("expected failure, got {ok:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp for an abortable capture: records a partial, writes the `.ytdl` state file, then sleeps until killed.
fn fake_ytdlp_slow(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-slow");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo "[Grab];downloading;1;100;100;1000;5"
printf 'partial' > "$out.part"
printf '{"downloader": {"current_fragment": {"index": 0}}}' > "$out.ytdl"
sleep 60
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_capture_stale_staging_never_adopts() {
    // Crashed-run leftovers are wiped first, so a barren run fails instead of delivering stale bytes.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("live.mp4"), b"stale").unwrap();
    let fake_yt = fake_ytdlp_live(&dir, true);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(res.is_err(), "barren run must fail, got {res:?}");
    assert!(!job.dest.exists(), "stale bytes must not deliver");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_capture_refuses_existing_dest() {
    // Overwrite pre-flight: a finished file at dest refuses before recording, requeueing under a fresh name.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-ow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    std::fs::write(&job.dest, b"already").unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert_eq!(e.to_string(), crate::engine_msg::DEST_EXISTS, "{e}"),
        ok => panic!("expected pre-flight refusal, got {ok:?}"),
    }
    assert!(
        !dir.join("v.live.mp4.part").exists(),
        "no capture shell: the fake must never have run"
    );
    assert!(!staging.exists(), "staging untouched by the refusal");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_capture_refusal_reclaims_stale_scratch() {
    // Crashed stem's shell would never be swept again (requeue uses a fresh name): the refusal is the last chance to reclaim it.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-reclaim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    std::fs::write(&job.dest, b"already").unwrap();
    std::fs::write(dir.join("v.live.mp4.part"), b"crashed").unwrap();
    std::fs::write(dir.join("v.live.mp4.ytdl"), b"fragment-3").unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert_eq!(e.to_string(), crate::engine_msg::DEST_EXISTS, "{e}"),
        ok => panic!("expected pre-flight refusal, got {ok:?}"),
    }
    assert_eq!(
        std::fs::read(&job.dest).unwrap(),
        b"already",
        "the finished file at dest must never be touched by a parts sweep"
    );
    assert!(
        !dir.join("v.live.mp4.part").exists(),
        "the crashed run's shell outlived its row"
    );
    assert!(
        !dir.join("v.live.mp4.ytdl").exists(),
        "the crashed run's state file outlived its row"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_capture_abort_adopts_partial() {
    // User stop mid-capture: recorded partial is adopted and remuxed, row completes Done.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-abort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_slow(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let res = crate::runtime::tokio_rt().block_on(async {
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
        let state = dir.join("v.live.mp4.ytdl");
        let seen = dir.join("abort-saw-state");
        tokio::spawn(async move {
            // Wait for the state file before stopping: a fixed sleep would race and pass vacuously.
            let mut observed = false;
            for _ in 0..200 {
                if state.exists() {
                    observed = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            if observed {
                std::fs::write(&seen, b"1").ok();
            }
            let _ = abort_tx.send(crate::video::StopIntent::Preserve);
        });
        run_live_ytdlp(
            &fake_yt,
            &fake_ff,
            &staging,
            &job,
            &AttemptGate::new(),
            "https://youtu.be/grabtest",
            "h1080",
            abort_rx,
            std::time::Duration::from_secs(30),
            tx,
        )
        .await
    });
    assert!(
        matches!(res, Ok(Some(_))),
        "abort must complete Done, got {res:?}"
    );
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"partial");
    // The fixture must really have produced the state file, or the sweep assertion passes vacuously.
    assert!(
        dir.join("abort-saw-state").exists(),
        "fixture never wrote the .ytdl before the abort: test is vacuous"
    );
    // A killed yt-dlp never removes its `.ytdl` file, so the post-capture sweep owns it.
    assert!(
        !dir.join("v.live.mp4.ytdl").exists(),
        "killed capture left its .ytdl state file behind"
    );
    assert!(
        !dir.join("v.live.mp4.part").exists(),
        "killed capture left its .part shell behind"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp that records `.part` + `.ytdl` and exits cleanly (the sweep must own the stranded state either way).
fn fake_ytdlp_live_with_state(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-live-state");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo "[Grab];downloading;8;100;100;1000;5"
printf 'recorded' > "$out.part"
printf '{"downloader": {"current_fragment": {"index": 7}}}' > "$out.ytdl"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Fake ffmpeg that fails, optionally after writing a partial output (which the non-recursive sweep must remove itself).
fn fake_ffmpeg_fail(dir: &std::path::Path, partial: bool) -> std::path::PathBuf {
    let bin = dir.join("fake-ffmpeg-fail");
    let body = if partial {
        r#"#!/bin/sh
last=""
for a in "$@"; do last="$a"; done
printf 'partial-remux' > "$last"
echo "ffmpeg: gave up halfway" >&2
exit 1
"#
    } else {
        "#!/bin/sh\necho \"ffmpeg: nope\" >&2\nexit 1\n"
    };
    std::fs::write(&bin, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_capture_remux_failure_sweeps_state_but_keeps_recording() {
    // Failed remux keeps the shell (only copy for salvage) but drops the `.ytdl` scratch.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-remuxfail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live_with_state(&dir);
    let fake_ff = fake_ffmpeg_fail(&dir, true);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(res.is_err(), "failed remux must fail the row, got {res:?}");
    assert!(
        !dir.join("v.live.mp4.ytdl").exists(),
        "failed remux left its .ytdl state file behind"
    );
    assert_eq!(
        std::fs::read(dir.join("v.live.mp4.part")).unwrap(),
        b"recorded",
        "the recording is the only copy: it must survive a failed remux"
    );
    assert!(!job.dest.exists(), "no file is delivered on a failed remux");
    // A failed ffmpeg can leave a partial `final.<n>.mp4`: worthless, and the non-recursive sweep must remove it.
    assert!(
        !staging.exists(),
        "a failed remux left its partial remux temp in staging"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp that records nothing but still writes a state file (barren exit must not strand it).
fn fake_ytdlp_live_barren_with_state(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-live-barren-state");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
printf '{"downloader": {"current_fragment": {"index": 0}}}' > "$out.ytdl"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_capture_barren_start_sweeps_state_file() {
    // Barren run records nothing: the state file must not outlive the row.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-barren-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live_barren_with_state(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(res.is_err(), "barren run must fail, got {res:?}");
    assert!(
        !dir.join("v.live.mp4.ytdl").exists(),
        "barren run left its .ytdl state file behind"
    );
    assert!(!job.dest.exists(), "no file is delivered from a barren run");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake ffmpeg that remuxes normally but first plants a file at dest, simulating a mid-capture name claim.
fn fake_ffmpeg_racing_dest(
    dir: &std::path::Path,
    dest: &std::path::Path,
    out: &std::path::Path,
) -> std::path::PathBuf {
    let bin = dir.join("fake-ffmpeg-race");
    std::fs::write(
        &bin,
        format!(
            r#"#!/bin/sh
input=""
prev=""
last=""
for a in "$@"; do
    if [ "$prev" = "-i" ]; then input="$a"; fi
    prev="$a"
    last="$a"
done
printf 'someone-else' > '{}'
# Seed the finalized-name path too: a clean yt-dlp exit renames its
# shell there, so the sweep has both media shapes to reclaim.
cat "$input" > '{}'
cat "$input" > "$last"
exit 0
"#,
            dest.display(),
            out.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_capture_lost_rename_race_sweeps_state() {
    // Mid-capture name claim fails the final rename and requeues; shell is redundant, state is scratch.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let fake_yt = fake_ytdlp_live_with_state(&dir);
    let fake_ff = fake_ffmpeg_racing_dest(&dir, &job.dest, &dir.join("v.live.mp4"));
    let staging = dir.join("staging");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert_eq!(e.to_string(), crate::engine_msg::DEST_EXISTS, "{e}"),
        ok => panic!("expected the lost rename race, got {ok:?}"),
    }
    assert!(
        !dir.join("v.live.mp4.ytdl").exists(),
        "lost rename race left its .ytdl state file behind"
    );
    // The row re-records under a fresh name, so both media paths must go.
    assert!(
        !dir.join("v.live.mp4").exists(),
        "lost rename race left the finished capture shell behind"
    );
    assert!(
        !dir.join("v.live.mp4.part").exists(),
        "lost rename race left the .part shell behind"
    );
    assert_eq!(
        std::fs::read(&job.dest).unwrap(),
        b"someone-else",
        "the file that won the name race must be left untouched"
    );
    assert!(
        !staging.exists(),
        "a requeued attempt must always reclaim staging"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake ffmpeg that remuxes, then replaces the dest directory with a file so the rename fails with ENOTDIR.
/// Structural trigger, never a permission bit: CI runs as root, where chmod is a no-op.
fn fake_ffmpeg_breaking_dest_dir(dest_dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dest_dir
        .parent()
        .expect("dest dir has a parent")
        .join("fake-ffmpeg-breakdestdir");
    std::fs::write(
        &bin,
        format!(
            r#"#!/bin/sh
input=""
prev=""
last=""
for a in "$@"; do
    if [ "$prev" = "-i" ]; then input="$a"; fi
    prev="$a"
    last="$a"
done
cat "$input" > "$last"
# The remux is safely in staging by now; break only the destination side.
# Fakes live outside this directory so nothing unlinks the running script.
rm -rf '{}'
: > '{}'
exit 0
"#,
            dest_dir.display(),
            dest_dir.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[cfg(target_os = "linux")]
#[test]
fn a_discard_signal_reaps_the_whole_recorder_group_and_delivers_nothing() {
    // Real runner: assert the full nested result and both leader + descendant, not just the timeout or leader.
    let dir = std::env::temp_dir().join(format!("grab-discardreal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live_abortable(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();
    let pidfile = dir.join("recorder-pid");
    let childpid_file = dir.join("recorder-child-pid");

    let task = crate::runtime::tokio_rt().spawn({
        let staging = staging.clone();
        let pidfile = pidfile.clone();
        async move {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            // Discard only once the recorder is genuinely up, so this
            // cannot pass by taking a stop-before-start path.
            tokio::spawn(async move {
                for _ in 0..500 {
                    if pidfile.exists() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                let _ = stop_tx.send(StopIntent::Discard);
            });
            run_live_ytdlp(
                &fake_yt,
                &fake_ff,
                &staging,
                &job,
                &AttemptGate::new(),
                "https://youtu.be/grabtest",
                "h1080",
                stop_rx,
                std::time::Duration::from_secs(600),
                tx,
            )
            .await
        }
    });

    let leader = read_pid(&pidfile);
    let mut cleanup = GroupCleanup(Some(leader));
    // Assert the full nested result: inner Err/JoinError must fail here, not read as a timeout.
    let outcome = crate::runtime::tokio_rt()
        .block_on(async { tokio::time::timeout(std::time::Duration::from_secs(30), task).await });
    let inner = outcome
        .expect(
            "the discard signal did not end the capture: it only finished \
                 because the wait timed out",
        )
        .expect("the capture task itself was cancelled or panicked");
    assert!(
        matches!(inner, Ok(None)),
        "a discarded capture must report no delivery, got {inner:?}"
    );
    assert!(
        !dest.exists(),
        "a discarded live capture was delivered: the row is gone, so a file \
         at the destination would have no row to belong to"
    );

    let descendant = crate::runtime::tokio_rt().block_on(async {
        for _ in 0..200 {
            if childpid_file.exists() {
                return read_pid(&childpid_file);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        libc::pid_t::from(0)
    });
    assert_ne!(
        descendant, 0,
        "the fixture never reported its descendant, so this cannot prove the \
         whole group died"
    );
    // Oracle is "can no longer execute": SIGKILLed processes stay visible as zombies until reaped. A zombie is dead.
    let mut leader_dead = false;
    let mut descendant_dead = false;
    crate::runtime::tokio_rt().block_on(async {
        for _ in 0..500 {
            leader_dead = !still_running(leader);
            descendant_dead = !still_running(descendant);
            if leader_dead && descendant_dead {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
    assert!(
        leader_dead,
        "the recorder (pid {leader}) can still run after a discard"
    );
    assert!(
        descendant_dead,
        "the recorder's descendant (pid {descendant}) can still run: only the \
         direct child was killed, so an ffmpeg would be orphaned"
    );
    cleanup.disarm();
    drop(cleanup);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake ffmpeg that blocks until the test releases the remux. The release file is the ordering guarantee a sleep can't give.
fn fake_ffmpeg_release_signalled(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ffmpeg-release-signalled");
    let marker = dir.join("ffmpeg-started");
    let release = dir.join("ffmpeg-release");
    std::fs::write(
        &bin,
        format!(
            r#"#!/bin/sh
input=""
prev=""
last=""
for a in "$@"; do
    if [ "$prev" = "-i" ]; then input="$a"; fi
    prev="$a"
    last="$a"
done
: > '{}'
while [ ! -e '{}' ]; do sleep 0.05; done
cat "$input" > "$last"
exit 0
"#,
            marker.display(),
            release.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn a_discard_that_lands_mid_remux_still_delivers_nothing() {
    // Removal during remux must claim the gate: the stop prompt sits unread, so only the pre-rename commit arbitrates delivery.
    let dir = std::env::temp_dir().join(format!("grab-discardremux-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A recorder that finishes on its own, so the discard lands after the select.
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_release_signalled(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();
    let marker = dir.join("ffmpeg-started");
    let release = dir.join("ffmpeg-release");
    let gate = AttemptGate::new();

    let task = crate::runtime::tokio_rt().spawn({
        let staging = staging.clone();
        let marker = marker.clone();
        let release = release.clone();
        let gate = std::sync::Arc::clone(&gate);
        async move {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            tokio::spawn({
                let gate = std::sync::Arc::clone(&gate);
                async move {
                    for _ in 0..500 {
                        if marker.exists() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    let _ = stop_tx.send(StopIntent::Discard);
                    // The prompt is unread this late, so only the gate claim stops delivery.
                    let _ = gate.discard();
                    // Unblock only after the discard, so the removal provably landed mid-remux.
                    let _ = std::fs::write(&release, b"release");
                }
            });
            run_live_ytdlp(
                &fake_yt,
                &fake_ff,
                &staging,
                &job,
                &gate,
                "https://youtu.be/grabtest",
                "h1080",
                stop_rx,
                std::time::Duration::from_secs(60),
                tx,
            )
            .await
        }
    });

    let inner = crate::runtime::tokio_rt()
        .block_on(async { tokio::time::timeout(std::time::Duration::from_secs(30), task).await })
        .expect("the run never finished")
        .expect("the task was cancelled or panicked");
    assert!(
        matches!(inner, Ok(None)),
        "a capture discarded during its remux must report no delivery, got {inner:?}"
    );
    assert!(
        !dest.exists(),
        "a removal that landed mid-remux still delivered: the gate was \
         never claimed, so the commit went ahead"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_non_live_sweep_keeps_a_live_recordings_remux() {
    // A rerouted retry shares staging with the live path: the finishing leg must step around the unplaceable recording (#178).
    let dir = std::env::temp_dir().join(format!("grab-sweepscope-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    // Scratch the finishing leg owns, and a recording it does not.
    std::fs::write(staging.join("manifest.json"), b"{}").unwrap();
    std::fs::write(staging.join("video.f137.mp4"), b"part").unwrap();
    std::fs::write(staging.join("final.1.mp4"), b"a live recording").unwrap();
    std::fs::write(staging.join("final.1.mp4.lease"), b"").unwrap();

    sweep_staging_preserving_recordings(&staging);

    assert_eq!(
        std::fs::read(staging.join("final.1.mp4")).unwrap(),
        b"a live recording",
        "a non-live leg destroyed a live attempt's completed remux"
    );
    assert!(
        staging.join("final.1.mp4.lease").exists(),
        "the lease marks a claimed remux slot and must survive with it"
    );
    assert!(
        !staging.join("manifest.json").exists() && !staging.join("video.f137.mp4").exists(),
        "the finishing leg's own scratch was not reclaimed"
    );

    // With the recordings gone the directory itself is reclaimed.
    std::fs::remove_file(staging.join("final.1.mp4")).unwrap();
    std::fs::remove_file(staging.join("final.1.mp4.lease")).unwrap();
    sweep_staging_preserving_recordings(&staging);
    assert!(
        !staging.exists(),
        "an emptied staging dir should be reclaimed, not left as litter"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crashed_remux_leaves_only_a_partial_and_it_is_reclaimable() {
    // A remux writes `final.<n>.<ext>.part` and renames only on success, so partials are sweepable by construction.
    let dir = std::env::temp_dir().join(format!("grab-partialremux-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("final.1.mp4.part"), b"half a remux").unwrap();
    std::fs::write(staging.join("final.2.mp4"), b"a completed recording").unwrap();
    std::fs::write(staging.join("final.2.mp4.lease"), b"").unwrap();
    std::fs::write(staging.join("manifest.json"), b"{}").unwrap();

    sweep_partial_remuxes(&staging);

    assert!(
        !staging.join("final.1.mp4.part").exists(),
        "a partial remux from a crashed attempt was not reclaimed"
    );
    assert_eq!(
        std::fs::read(staging.join("final.2.mp4")).unwrap(),
        b"a completed recording",
        "bounded retention must never cost a completed recording -- that is \
         the whole reason the partial is named differently"
    );
    assert!(staging.join("final.2.mp4.lease").exists(), "lease lost");
    assert!(
        staging.join("manifest.json").exists(),
        "this sweep is only about partials; other scratch is the finishing \
         leg's business"
    );
    // A file that merely contains "part" is not a partial remux.
    std::fs::write(staging.join("final.3.partial.mp4"), b"not a partial").unwrap();
    sweep_partial_remuxes(&staging);
    assert!(
        staging.join("final.3.partial.mp4").exists(),
        "the sweep matched a name that is not a partial remux"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_successful_live_remux_is_only_named_final_once_ffmpeg_succeeds() {
    // The rename is the mechanism: until then the file is a sweepable `.part`.
    let dir = std::env::temp_dir().join(format!("grab-remuxrename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("live.mp4.part");
    std::fs::write(&src, b"recorded").unwrap();
    let final_tmp = dir.join("final.1.mp4");
    let fake_ff = fake_ffmpeg_copy(&dir);

    let res = crate::runtime::tokio_rt().block_on(remux_live_capture(
        &fake_ff,
        &src,
        &final_tmp,
        false,
        std::time::Duration::from_secs(30),
        "https://x.com/watch?v=abc123",
    ));
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(
        std::fs::read(&final_tmp).unwrap(),
        b"recorded",
        "a successful remux must land under its final name"
    );
    assert!(
        !dir.join("final.1.mp4.part").exists(),
        "the partial was left behind after the rename"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_live_capture_in_flight_keeps_its_destination_stem_reserved() {
    // #179 overlap: the recorder's shell reserves the stem until teardown, so a new row can't claim the filename.
    let dir = std::env::temp_dir().join(format!("grab-stemreserve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("v.mp4");
    // The name a live capture actually writes beside the destination.
    let shell = dest_part_path(&dest, "live", "mp4").with_extension("mp4.part");
    std::fs::write(&shell, b"recording").unwrap();

    let names = dir_file_names(&dir);
    assert!(
        stem_reserved_in(&names, "v"),
        "intake could claim the stem while a live capture is still writing to \
         it, so a new row would land on the same filename"
    );
    assert!(
        !stem_reserved_in(&names, "other"),
        "an unrelated stem must still be claimable"
    );

    // Once the finalizer has swept, the stem is free again -- which is the
    // point of deferring: the reservation ends when the row really is gone.
    crate::video::clean_dest_parts(&dest);
    let names = dir_file_names(&dir);
    assert!(
        !stem_reserved_in(&names, "v"),
        "the stem stayed reserved after the row's scratch was reclaimed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_live_remux_leaves_no_partial_behind() {
    // A failure must leave no half-written file that looks like a recording.
    let dir = std::env::temp_dir().join(format!("grab-remuxfail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("live.mp4.part");
    std::fs::write(&src, b"recorded").unwrap();
    let final_tmp = dir.join("final.1.mp4");
    let fake_ff = fake_ffmpeg_fail(&dir, true);

    let res = crate::runtime::tokio_rt().block_on(remux_live_capture(
        &fake_ff,
        &src,
        &final_tmp,
        false,
        std::time::Duration::from_secs(30),
        "https://x.com/watch?v=abc123",
    ));
    assert!(res.is_err(), "the remux must fail");
    assert!(
        !final_tmp.exists(),
        "a failed remux published a file under the name a completed \
         recording would take"
    );
    assert!(
        !dir.join("final.1.mp4.part").exists(),
        "a failed remux left a partial behind for a later sweep to clean"
    );
    assert_eq!(
        std::fs::read(&src).unwrap(),
        b"recorded",
        "the raw capture was taken"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake recorder that writes a caller-chosen payload, so two attempts on the same row can be told apart.
/// Write an executable fake at `path`; kept separate so tests can place one outside the dest dir.
fn write_fake(path: &std::path::Path, body: &str) -> std::path::PathBuf {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path.to_path_buf()
}

/// A live recorder that writes `payload` into its `.part` shell and a state sidecar, then exits cleanly.
fn fake_ytdlp_live_payload(
    dir: &std::path::Path,
    bin_name: &str,
    payload: &str,
) -> std::path::PathBuf {
    write_fake(
        &dir.join(bin_name),
        &format!(
            r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
printf '{payload}' > "$out.part"
printf '{{"downloader": {{}}}}' > "$out.ytdl"
exit 0
"#,
            payload = payload
        ),
    )
}

#[test]
fn a_successful_retry_leaves_the_previous_attempts_remux_alone() {
    // Retry used to remux into the same `staging/final.<ext>` and wipe the dir, destroying the prior recording. Structural ENOTDIR trigger (CI runs as root).
    let base = std::env::temp_dir().join(format!("grab-retryremux-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("dest");
    let staging = base.join("staging");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    // Attempt 1 replaces `dir` with a file, so every other fake has to
    // live outside it.
    let break_ff = fake_ffmpeg_breaking_dest_dir(&dir);
    let first_yt = fake_ytdlp_live_payload(&dir, "fake-yt-first", "first-attempt");
    let second_yt = fake_ytdlp_live_payload(&base, "fake-yt-second", "second-attempt");
    let copy_ff = fake_ffmpeg_copy(&base);

    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");

    let (tx1, _rx1) = tokio::sync::mpsc::unbounded_channel();
    let (_a1, abort1) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let first = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &first_yt,
        &break_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort1,
        std::time::Duration::from_secs(30),
        tx1,
    ));
    // Must be the unexpected-rename branch: a requeue would sweep away
    // the very temp this guards.
    match first {
        Err(e) => assert_ne!(
            e.to_string(),
            crate::engine_msg::DEST_EXISTS,
            "the fixture must fail the rename itself, not the pre-flight or the \
             lost-name race: {e}"
        ),
        ok => panic!("a broken destination must fail the row, got {ok:?}"),
    }
    assert_eq!(
        std::fs::read(staging.join("final.1.mp4")).unwrap(),
        b"first-attempt",
        "a remux that could not be placed must be kept in staging"
    );

    // The obstruction clears and the user retries.
    std::fs::remove_file(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
    let (_a2, abort2) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let second = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &second_yt,
        &copy_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort2,
        std::time::Duration::from_secs(30),
        tx2,
    ));
    assert!(
        matches!(second, Ok(Some(_))),
        "the retry must now succeed, got {second:?}"
    );
    assert_eq!(
        std::fs::read(&job.dest).unwrap(),
        b"second-attempt",
        "the retry delivers its own recording"
    );
    assert_eq!(
        std::fs::read(staging.join("final.1.mp4")).unwrap(),
        b"first-attempt",
        "a successful retry destroyed the earlier attempt's completed remux: \
         attempt-scoped temp names plus a non-recursive sweep are what prevent this"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_remux_slot_is_claimed_exactly_once() {
    // Scan-then-claim races: the lease makes the slot claim atomic.
    let dir = std::env::temp_dir().join(format!("grab-remuxslot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let first = reserve_remux_temp(&staging, "mp4").unwrap();
    assert_eq!(first.file_name().unwrap(), "final.1.mp4");
    assert!(
        first.with_extension("mp4.lease").exists(),
        "the slot must be claimed with a lease, not merely observed free"
    );

    // A second claim cannot land on the same slot, even without a temp file yet.
    let second = reserve_remux_temp(&staging, "mp4").unwrap();
    assert_eq!(second.file_name().unwrap(), "final.2.mp4");
    assert_ne!(second, first, "two claims shared one remux slot");

    // An occupied temp is skipped too, not just an occupied lease.
    release_remux_lease(&first);
    std::fs::write(&first, b"an earlier recording").unwrap();
    let third = reserve_remux_temp(&staging, "mp4").unwrap();
    assert_eq!(
        third.file_name().unwrap(),
        "final.3.mp4",
        "an existing completed remux must never be handed out as a new slot"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_sweep_never_removes_another_attempts_remux() {
    // A failing attempt must not sweep an earlier attempt's completed remux (the only copy).
    let base = std::env::temp_dir().join(format!("grab-othersremux-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("dest");
    let staging = base.join("staging");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    let survivor = staging.join("final.1.mp4");
    std::fs::write(&survivor, b"earlier-attempt").unwrap();

    // A barren attempt records nothing, so it never reaches the remux.
    let fake_yt = fake_ytdlp_live_barren_with_state(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_a, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(res.is_err(), "a barren attempt must fail the row");
    assert_eq!(
        std::fs::read(&survivor).unwrap(),
        b"earlier-attempt",
        "a failing attempt swept an earlier attempt's completed remux out of staging"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn live_capture_rename_failure_keeps_completed_remux() {
    // A failed rename is not a requeue: the completed remux in staging is the only copy, so sweeping would destroy it.
    let base =
        std::env::temp_dir().join(format!("grab-fakelive-renamefail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("dest");
    let staging = base.join("staging");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let fake_yt = fake_ytdlp_live_with_state(&dir);
    let fake_ff = fake_ffmpeg_breaking_dest_dir(&dir);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    // Must be the rename-failure branch: a requeue would sweep the very file this guards.
    match res {
        Err(e) => assert_ne!(
            e.to_string(),
            crate::engine_msg::DEST_EXISTS,
            "the fixture must fail the rename itself, not the pre-flight or the \
             lost-name race, or this test covers the wrong branch: {e}"
        ),
        ok => panic!("a broken destination must fail the row, got {ok:?}"),
    }
    assert_eq!(
        std::fs::read(staging.join("final.1.mp4")).unwrap(),
        b"recorded",
        "the completed remux must survive a failed rename, not be swept"
    );
    assert!(staging.exists(), "staging must be left intact for salvage");
    let _ = std::fs::remove_dir_all(&base);
}

/// Fake yt-dlp for the live-edge retry: barren first attempt forces the downgrade; second records whether stale `.ytdl` survived.
fn fake_ytdlp_live_retry(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-live-retry");
    std::fs::write(
        &bin,
        format!(
            r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
n=0
[ -f '{count}' ] && n=$(cat '{count}')
n=$((n + 1))
printf '%s' "$n" > '{count}'
if [ "$n" = "1" ]; then
    printf 'fragment-3' > "$out.ytdl"
    exit 0
fi
if [ -f "$out.ytdl" ]; then
    printf 'stale' > '{marker}'
fi
printf 'recorded' > "$out.part"
exit 0
"#,
            count = dir.join("retry-count").display(),
            marker = dir.join("retry-saw-stale-state").display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_capture_retry_never_inherits_stale_state() {
    // A stale `.ytdl` would resume fragment 3 against a wiped shell: corrupt recording, not litter.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    job.live_from_start = true;
    let fake_yt = fake_ytdlp_live_retry(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        matches!(res, Ok(Some(_))),
        "the live-edge retry must deliver, got {res:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("retry-count")).unwrap(),
        "2",
        "the barren from-start attempt must have been retried once"
    );
    assert!(
        !dir.join("retry-saw-stale-state").exists(),
        "the retry inherited a stale .ytdl and would have resumed a deleted shell"
    );
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"recorded");
    assert!(
        !dir.join("v.live.mp4.ytdl").exists(),
        "state must not survive"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake recorder that reports its pid, lays scratch, then blocks (aborting the owner simulates shutdown). Linux-only: needs procfs.
#[cfg(target_os = "linux")]
fn fake_ytdlp_live_abortable(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-live-abortable");
    std::fs::write(
        &bin,
        format!(
            r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
printf 'partial' > "$out.part"
printf '{{"downloader": {{}}}}' > "$out.ytdl"
# Report the group leader (this shell) and a descendant in the same
# group, so the test can prove the *group* died rather than just the
# leader. A direct-child-only kill passes a leader-only oracle.
# Ordering: the descendant is spawned and reported first, the leader
# pid last, so a waiter that sees the leader marker knows the
# descendant already exists.
sleep 600 &
printf '%s' "$!" > '{childpid}'
printf '%s' "$$" > '{pidfile}'
wait
exit 0
"#,
            pidfile = dir.join("recorder-pid").display(),
            childpid = dir.join("recorder-child-pid").display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Test-only group SIGKILL on drop, so a failing abort test leaves no 600s orphan. Disarm once gone, so no recycled PGID is signalled.
#[cfg(target_os = "linux")]
struct GroupCleanup(Option<libc::pid_t>);

#[cfg(target_os = "linux")]
impl GroupCleanup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(target_os = "linux")]
impl Drop for GroupCleanup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: constant signal number; ESRCH (already dead) is harmless.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

/// Whether a pid can still execute: in `/proc` and not a zombie. Zombies stay visible until reaped, so pid-existence is the wrong oracle.
/// Fails closed: only `NotFound` reads as dead, so missing procfs can't fake a kill.
#[cfg(target_os = "linux")]
fn still_running(pid: libc::pid_t) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // `comm` may contain spaces/parens, so state follows the last ')'.
        Ok(stat) => match stat.rsplit_once(')') {
            Some((_, rest)) => !rest.trim_start().starts_with('Z'),
            None => true,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Oracle check: an exited-but-unreaped zombie must read as dead (the condition that broke CI).
#[cfg(target_os = "linux")]
#[test]
fn an_unreaped_exited_child_reads_as_dead() {
    let mut child = std::process::Command::new("sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id() as libc::pid_t;
    // Poll for zombie state: a fixed sleep could mistake "not exited yet" for "dead".
    let mut state = None;
    for _ in 0..500 {
        state = scheduler_state(pid);
        if matches!(state, Some('Z')) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        state,
        Some('Z'),
        "the child should have exited and become a zombie, but its scheduler \
         state was {state:?} — the fixture is not reproducing the CI condition"
    );
    // The pid is still very much present: this is exactly what the old
    // pid-existence oracle asserted against, and why it failed in CI.
    // SAFETY: signal 0 checks existence and delivers nothing.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the zombie should still hold its pid, since nothing reaped it"
    );
    assert!(
        !still_running(pid),
        "a zombie can never run again and must not read as running"
    );
    let _ = child.wait();
}

/// Scheduler state from `/proc/<pid>/stat`, or `None` when gone/unreadable (only ever confirms a state, never infers one).
#[cfg(target_os = "linux")]
fn scheduler_state(pid: libc::pid_t) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // State follows the last ')' since comm may contain spaces/parens.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.trim_start().chars().next()
}

/// Read a fixture-written pid, panicking if missing/malformed.
#[cfg(target_os = "linux")]
fn read_pid(path: &std::path::Path) -> libc::pid_t {
    for _ in 0..200 {
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(pid) = raw.trim().parse::<libc::pid_t>()
        {
            return pid;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("{} never appeared or held a pid", path.display());
}

// Linux-gated: the zombie oracle needs /proc; elsewhere pid-existence can't tell them apart.
#[cfg(target_os = "linux")]
#[test]
fn aborting_a_live_capture_kills_the_recorder() {
    // Tokio doesn't kill a child on handle drop, so shutdown needs a Drop guard killing the whole group.
    let dir = std::env::temp_dir().join(format!("grab-abortlive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live_abortable(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();
    let pidfile = dir.join("recorder-pid");

    let recorder = crate::runtime::tokio_rt().spawn({
        let staging_for_task = staging.clone();
        async move {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            // Never fires: the only way this ends is the abort below.
            let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
            // A long stall timeout so the capture cannot finish on its own.
            let _ = run_live_ytdlp(
                &fake_yt,
                &fake_ff,
                &staging_for_task,
                &job,
                &AttemptGate::new(),
                "https://youtu.be/grabtest",
                "h1080",
                abort_rx,
                std::time::Duration::from_secs(600),
                tx,
            )
            .await;
        }
    });

    // Wait until the fake is up before aborting: aborting a not-yet-spawned task passes vacuously. Both pids prove a group kill.
    let leader = read_pid(&pidfile);
    let mut cleanup = GroupCleanup(Some(leader));
    let childpid_file = dir.join("recorder-child-pid");
    let descendant = crate::runtime::tokio_rt().block_on(async {
        for _ in 0..200 {
            if childpid_file.exists() {
                return read_pid(&childpid_file);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the fake recorder never reported its descendant pid");
    });
    assert!(
        still_running(leader),
        "the recorder must be running before the abort"
    );
    assert!(
        still_running(descendant),
        "the fixture's descendant must be running before the abort"
    );

    recorder.abort();
    crate::runtime::tokio_rt().block_on(async {
        let _ = recorder.await;
    });

    // Poll both leader and descendant for "can no longer execute"; pid-absence is the wrong oracle (zombies stay visible).
    let mut leader_dead = false;
    let mut descendant_dead = false;
    crate::runtime::tokio_rt().block_on(async {
        for _ in 0..500 {
            leader_dead = !still_running(leader);
            descendant_dead = !still_running(descendant);
            if leader_dead && descendant_dead {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
    // Assert before disarming, so the guard still reaps the 600s fixture on failure.
    assert!(
        leader_dead,
        "the recorder (pid {leader}) can still run after the abort of its task: an \
         app shutdown would leave yt-dlp executing with no supervisor"
    );
    assert!(
        descendant_dead,
        "the recorder's descendant (pid {descendant}) can still run: only the \
         direct child was killed, so an ffmpeg would be orphaned on every shutdown"
    );
    // Both are dead, so release the PGID: nothing left to signal.
    cleanup.disarm();
    drop(cleanup);

    // File half: partial media is kept, scratch state must go, nothing is delivered.
    assert!(
        !dir.join("v.live.mp4.ytdl").exists(),
        "the abort left yt-dlp's state file behind: a later attempt would resume \
         fragment N against a shell Grab wipes first, producing a corrupt recording"
    );
    // Staging is NOT swept on abort: it may park a completed remux another attempt reuses.
    assert!(
        staging.exists(),
        "staging must be left alone on an abort: a blanket sweep would destroy a \
         parked completed remux from an earlier attempt"
    );
    assert_eq!(
        std::fs::read(dir.join("v.live.mp4.part")).unwrap(),
        b"partial",
        "the partial recording must not be destroyed by an involuntary shutdown"
    );
    assert!(
        !dest.exists(),
        "nothing is delivered from an aborted capture"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake recorder that stays blocked, so only the group kill under test can end it.
fn fake_sleeper(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-sleeper");
    std::fs::write(&bin, "#!/bin/sh\nsleep 600\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[cfg(unix)]
#[test]
fn reap_child_reaps_the_signalled_child() {
    // Signalling isn't reaping: an unreaped child lingers as a zombie holding its pid until waited on (ESRCH).
    let dir = std::env::temp_dir().join(format!("grab-fakesleeper-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sleeper = fake_sleeper(&dir);

    // Spawn inside the runtime: tokio's process API needs a reactor.
    let pid = crate::runtime::tokio_rt().block_on(async {
        let mut child = ytdlp_command(&sleeper).spawn().unwrap();
        let pid = child.id().expect("freshly spawned child has a pid") as libc::pid_t;
        let mut group = ProcessGroupGuard::new(&child);
        reap_child(&mut child, &mut group).await;
        assert!(
            child.id().is_none(),
            "the child was signalled but never waited on: its pid is still cached. \
             A SIGKILLed `sleep` exits at once, so the reap must have returned"
        );
        // reap_child disarms: the reaped PGID is no longer ours to signal.
        assert!(
            !group.is_armed(),
            "reap_child must disarm the group guard: the reaped leader's PGID \
             is no longer ours to signal"
        );
        pid
    });

    // Supplementary pid probe: corroboration only, not race-free (pid could recycle in the gap).
    // SAFETY: signal 0 only probes existence.
    let probe = unsafe { libc::kill(pid, 0) };
    assert_eq!(
        probe, -1,
        "the recorder is still in the process table: it was signalled but never reaped \
         (pid {pid} survived the kill)"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "the recorder's pid must be released, not left held by a zombie"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp for the stuck-full row: scripted wobble, then writes `-o` output and prints its path.
fn fake_ytdlp_hls_scripted(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-hls-scripted");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo '[Grab];downloading;1000000;2000000;2000000;NA;NA'
echo '[Grab];downloading;2000000;2000000;2000000;NA;NA'
echo '[Grab];downloading;2000000;100000000;100000000;NA;NA'
echo '[Grab];downloading;44000000;100000000;100000000;NA;NA'
echo '[Grab];downloading;46000000;40000000;40000000;NA;NA'
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$out"
printf '%s\n' "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn hls_map_survives_estimate_wobble() {
    // Stuck-full regression: downward wobble must leave the map at ~46%, not flood it.
    use crate::engine_msg::EngineMsg;
    let dir = std::env::temp_dir().join(format!("grab-fakehls-wobble-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls_scripted(&dir);
    let staging = dir.join("staging");
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    let mut inits = Vec::new();
    let mut marked = std::collections::HashSet::new();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            EngineMsg::SegmentsInit { total } => {
                inits.push(total);
                marked.clear();
            }
            EngineMsg::PieceDone(idx) => {
                marked.insert(idx);
            }
            _ => {}
        }
    }
    // One init per total; the downward wobble must not rebuild the grid.
    assert_eq!(inits, vec![2_000_000u64, 100_000_000u64], "{inits:?}");
    // 46 MB of 100 MB: marked cells over live grid, never ~full.
    let cells = 100_000_000u64.div_ceil(crate::file_names::piece_len(100_000_000)) as f64;
    let frac = marked.len() as f64 / cells;
    assert!(
        (0.35..0.6).contains(&frac),
        "map fraction {frac} ({} marks), expected ~0.46",
        marked.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp modeling an HLS estimate that refines UP by less than 2x:
/// the grid is built for 640 MB, the download reaches 639.7 MB (grid nearly
/// full), then the total is refined to 1.1 GB. The grid must rebuild instead
/// of staying flood-lit at ~100% while the bar shows ~56%.
fn fake_ytdlp_hls_upward_wobble(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-hls-upwobble");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo '[Grab];downloading;1000000;640000000;640000000;NA;NA'
echo '[Grab];downloading;639700000;640000000;640000000;NA;NA'
echo '[Grab];downloading;639700000;1100000000;1100000000;NA;NA'
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$out"
printf '%s\n' "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn hls_map_rebuilds_on_upward_wobble() {
    // Flood-lit regression: a refined-up total under 2x must rebuild the grid;
    // the map must track the bar (~58%) instead of reading full at ~56%.
    use crate::engine_msg::EngineMsg;
    let dir = std::env::temp_dir().join(format!("grab-fakehls-upwobble-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls_upward_wobble(&dir);
    let staging = dir.join("staging");
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    let mut inits = Vec::new();
    let mut marked = std::collections::HashSet::new();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            EngineMsg::SegmentsInit { total } => {
                inits.push(total);
                marked.clear();
            }
            EngineMsg::PieceDone(idx) => {
                marked.insert(idx);
            }
            _ => {}
        }
    }
    // The 640 MB -> 1.1 GB refinement must rebuild the grid.
    assert_eq!(inits, vec![640_000_000u64, 1_100_000_000u64], "{inits:?}");
    // 639.7 MB of 1.1 GB: marked cells over the live grid, never ~full.
    let cells = 1_100_000_000u64.div_ceil(crate::file_names::piece_len(1_100_000_000)) as f64;
    let frac = marked.len() as f64 / cells;
    assert!(
        (0.5..0.65).contains(&frac),
        "map fraction {frac} ({} marks), expected ~0.58",
        marked.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp that prints one progress line then goes silent: the stall
/// watchdog must kill it instead of parking the row until a wall clock.
fn fake_ytdlp_hls_goes_silent(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-hls-silent");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
echo '[Grab];downloading;1000000;2000000;2000000;NA;NA'
sleep 30
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Fake yt-dlp that keeps talking: steady progress lines must keep resetting
/// the stall watchdog, so a slow-but-progressing download is never killed.
fn fake_ytdlp_hls_keeps_talking(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-hls-talking");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
i=0
while [ $i -lt 10 ]; do
    echo '[Grab];downloading;1000000;2000000;2000000;NA;NA'
    i=$((i+1))
    sleep 0.5
done
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$out"
printf '%s\n' "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn hls_stall_watchdog_kills_silent_download() {
    // One progress line, then silence: the 2s stall budget must fail the
    // attempt with "stalled", not wait out a wall clock.
    let dir = std::env::temp_dir().join(format!("grab-hls-stall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls_goes_silent(&dir);
    let staging = dir.join("staging");
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(2),
        tx,
    ));
    let err = res.expect_err("silent yt-dlp should stall out");
    assert!(
        err.to_string().contains("stalled"),
        "expected a stall failure, got {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hls_stall_watchdog_spares_progressing_download() {
    // A line every 0.5s against a 2s stall budget: the attempt must survive
    // ~5s of "downloading" and finish normally.
    let dir = std::env::temp_dir().join(format!("grab-hls-talking-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls_keeps_talking(&dir);
    let staging = dir.join("staging");
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(2),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hls_stall_watchdog_spares_silent_merge() {
    // Merge marker on stdout, then ~4s of silence on both streams against a
    // 2s stall budget: yt-dlp captures ffmpeg's output instead of forwarding
    // it, so a real merge looks exactly like this. The attempt must survive
    // and finish normally instead of dying as `stalled`.
    let dir = std::env::temp_dir().join(format!("grab-hls-silmerge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls_silent_merge(&dir);
    let staging = dir.join("staging");
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(2),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp that emits a merge marker on stdout, then stays silent on both
/// streams (like a real merge: ffmpeg's output is captured, not forwarded),
/// then writes the output and exits 0.
fn fake_ytdlp_hls_silent_merge(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-hls-silmerge");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo '[Merger] Merging formats into "v.mp4"'
sleep 4
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$out"
printf '%s\n' "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Fake yt-dlp for VOD HLS: logs argv, expands `%(ext)s`, writes bytes for discover.
fn fake_ytdlp_hls(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-hls");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo "$@" >> "$(dirname "$out").argv.log"
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn vod_hls_pins_planner_variant_id() {
    // Best-match with no pin still downloads the planner's pick verbatim.
    let dir = std::env::temp_dir().join(format!("grab-fakehls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls(&dir);
    let staging = dir.join("staging");
    let job = VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        playlist_item_id: None,
        quality: "best".into(),
        audio_only: false,
        dest: dir.join("v.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let gate = AttemptGate::new();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &gate,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"hlsbytes");
    assert!(
        gate.was_delivered(),
        "the HLS leg delivered a file without recording it: the orphan \
         finalizer would never know to reclaim it"
    );
    // Parts download beside the finished file, never into staging.
    let logged = std::fs::read_to_string(dir.with_extension("argv.log")).unwrap();
    assert!(logged.contains("v.hls."), "{logged}");
    let f = logged
        .split_whitespace()
        .position(|a| a == "-f")
        .expect("has -f");
    assert_eq!(
        logged.split_whitespace().nth(f + 1).expect("spec"),
        "h1080+ba/b",
        "{logged}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn vod_hls_refuses_existing_dest() {
    // Existing finished file refuses before transfer, so the pump requeues under a fresh name.
    let dir = std::env::temp_dir().join(format!("grab-fakehls-ow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls(&dir);
    let staging = dir.join("staging");
    let job = VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        playlist_item_id: None,
        quality: "best".into(),
        audio_only: false,
        dest: dir.join("v.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    };
    std::fs::write(&job.dest, b"already").unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert_eq!(e.to_string(), crate::engine_msg::DEST_EXISTS, "{e}"),
        ok => panic!("expected pre-flight refusal, got {ok:?}"),
    }
    assert!(
        !dir.with_extension("argv.log").exists(),
        "no argv log: the fake must never have run"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn format_selection_line_detection() {
    assert!(is_format_selection_line(
        "[info] 1234567890: Downloading 1 format(s): h1080+ba/b"
    ));
    assert!(is_format_selection_line(
        "[info] x: Downloading 2 format(s): 399+251"
    ));
    assert!(!is_format_selection_line(
        "[download] 100% of 10MiB in 00:01"
    ));
    assert!(!is_format_selection_line("[info] Downloading video info"));
    assert!(!is_format_selection_line(""));
}

#[test]
fn format_lines_survive_split_reads() {
    // A selection line split across reads still traces without loss.
    let mut pending = String::new();
    trace_format_lines(&mut pending, b"[info] abc: Download");
    assert_eq!(pending, "[info] abc: Download");
    trace_format_lines(&mut pending, b"ing 1 format(s): h1\n[download] x\n");
    assert!(pending.is_empty(), "complete lines drain: {pending:?}");
    trace_format_lines(&mut pending, b"");
    assert!(pending.is_empty());
}

#[test]
fn hls_format_spec_rejects_hostile_pins() {
    // Hostile pins (`,`, `[]`, `()`) fall through to the height rule instead of widening `-f`.
    assert_eq!(
        hls_format_spec("1080p", Some("hls-99,hls-720")),
        "bv*[height<=1080]+ba/b"
    );
    assert_eq!(
        hls_format_spec("1080p", Some("best[height=1080]")),
        "bv*[height<=1080]+ba/b"
    );
    assert_eq!(
        hls_format_spec("1080p", Some("(hls-99)")),
        "bv*[height<=1080]+ba/b"
    );
    // Dashes, underscores and colons are legitimate extractor id chars.
    assert_eq!(
        hls_format_spec("1080p", Some("hls-720_p:1")),
        "hls-720_p:1+ba/b"
    );
}

#[test]
fn picker_label_sanitizes_remote_codec() {
    // Remote codec strings strip bidi/newlines to label-safe chars (max 16), so rows can't be spoofed.
    let video = test_video(serde_json::json!([test_format_full(
        "evil",
        "avc1\u{202e}gnp8001\u{000a}FREE",
        "none",
        Some(720),
        None,
        "https",
        false
    ),]));
    let opts = video_format_options(&video, true);
    assert_eq!(opts.len(), 1);
    assert_eq!(opts[0].label, "720p · avc1gnp8001FREE");
    // Empty-after-filter degrades to the placeholder.
    let video = test_video(serde_json::json!([test_format_full(
        "weird",
        "...",
        "none",
        Some(720),
        None,
        "https",
        false
    ),]));
    let opts = video_format_options(&video, true);
    assert_eq!(opts[0].label, "720p · ?");
}

#[test]
fn plan_unknown_adoption_is_first_match() {
    // Height-less unknown files keep the old first-in-extractor-order pick;
    // only files with a known height go through the height-aware selection.
    let video = test_video(serde_json::json!([
        serde_json::json!({
            "format": "low",
            "format_id": "low",
            "protocol": "https",
            "ext": "mp4",
            "url": "https://cdn.example/low.mp4",
            "http_headers": {},
        }),
        serde_json::json!({
            "format": "high",
            "format_id": "high",
            "protocol": "https",
            "ext": "mp4",
            "url": "https://cdn.example/high.mp4",
            "http_headers": {},
        }),
        test_format_full("music", "none", "mp4a.40.2", None, None, "https", false),
    ]));
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "low");
}

#[test]
fn plan_unknown_adoption_picks_tallest() {
    // Regression: some extractors list direct files ascending beside taller
    // manifest variants; the old first-match pick took the lowest file and
    // suppressed the manifest path entirely, so one run downloaded 240p
    // and the next — without the direct files — 720p.
    let direct = |id: &str, h: u32| {
        serde_json::json!({
            "format": id,
            "format_id": id,
            "protocol": "https",
            "ext": "mp4",
            "height": h,
            "url": format!("https://cdn.example/{id}.mp4"),
            "http_headers": {},
        })
    };
    let video = test_video(serde_json::json!([
        direct("240p", 240),
        direct("480p", 480),
        direct("720p", 720),
        test_format_full(
            "hls-240",
            "avc1.64001f",
            "mp4a.40.2",
            Some(240),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "hls-480",
            "avc1.64001f",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "hls-720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "m3u8_native",
            false
        ),
    ]));
    let plan = plan_streams(&video, "best", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    // Tallest direct file wins; the equal-height variant does not override it.
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "720p");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_unknown_short_direct_loses_to_taller_hls() {
    // A short direct file no longer pins the download when a taller in-cap
    // manifest variant exists: the HLS override covers the unclassified
    // adoption like the muxed one.
    let video = test_video(serde_json::json!([
        serde_json::json!({
            "format": "240p",
            "format_id": "240p",
            "protocol": "https",
            "ext": "mp4",
            "height": 240,
            "url": "https://cdn.example/240p.mp4",
            "http_headers": {},
        }),
        test_format_full(
            "hls-720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "m3u8_native",
            false
        ),
    ]));
    let plan = plan_streams(&video, "best", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none());
    assert_eq!(plan.hls_sel.expect("hls wins").height, Some(720));
}

#[test]
fn plan_stale_pin_to_unlisted_id_resolves_as_split() {
    // Stale pins to unlisted-but-fetchable splits resolve as splits paired with audio.
    let video = test_video(serde_json::json!([
        test_format_full("v1080-vp9", "vp9", "none", Some(1080), None, "https", false),
        test_format_full(
            "v1080-avc",
            "avc1.640028",
            "none",
            Some(1080),
            None,
            "https",
            false
        ),
        test_format_full("music", "none", "mp4a.40.2", None, None, "https", false),
    ]));
    let listed: Vec<String> = video_format_options(&video, true)
        .iter()
        .map(|o| o.id.clone())
        .collect();
    assert!(
        !listed.contains(&"v1080-avc".to_string()),
        "fixture must keep the pin unlisted: {listed:?}"
    );
    let plan = plan_streams(&video, "1080p", false, Some("v1080-avc"), true, 1);
    let v = plan.video_sel.expect("stale pin resolves");
    assert_eq!(v.format_id, "v1080-avc");
    assert!(plan.audio_sel.is_some(), "split pairs with audio");
}

#[test]
fn fetch_video_page_surfaces_stderr_tail() {
    // Nonzero exit: the last non-blank stderr line becomes the detail.
    let dir = std::env::temp_dir().join(format!("grab-fakefail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(
        &bin,
        "#!/bin/sh\necho 'noise line' >&2\necho 'boom detail' >&2\necho '' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let res = crate::runtime::tokio_rt().block_on(fetch_video_page(
        &bin,
        "https://example.com/v",
        "none",
        std::time::Duration::from_secs(30),
        None,
        None,
    ));
    match res {
        Err(e) => assert!(e.to_string().contains("boom detail"), "{e}"),
        ok => panic!("expected fetch error, got {ok:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_video_page_rejects_garbage_stdout() {
    // Zero exit with non-JSON stdout is a parse error, not a phantom video.
    let dir = std::env::temp_dir().join(format!("grab-fakegarbage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(&bin, "#!/bin/sh\necho 'not json'\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let res = crate::runtime::tokio_rt().block_on(fetch_video_page(
        &bin,
        "https://example.com/v",
        "none",
        std::time::Duration::from_secs(30),
        None,
        None,
    ));
    assert!(res.is_err(), "garbage stdout must not parse");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp that records argv to `args.txt` and prints `stdout`.
fn fake_argv_dump_bin(dir_name: &str, stdout: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{dir_name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("out.txt"), stdout).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}/args.txt\"\ncat \"{}/out.txt\"\n",
            dir.display(),
            dir.display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn fake_bin_argv(bin: &std::path::Path) -> String {
    std::fs::read_to_string(bin.parent().unwrap().join("args.txt")).unwrap()
}

#[test]
fn fetch_video_page_resolves_without_flat_playlist() {
    // Download resolve must fully extract: `--flat-playlist` once returned stub-shaped stories.
    let bin = fake_argv_dump_bin(
        "grab-noflat",
        &serde_json::json!({
            "id": "abc",
            "title": "T",
            "age_limit": 0,
            "live_status": "not_live",
            "playable_in_embed": true,
            "extractor": "generic",
            "extractor_key": "Generic",
            "_version": {"version": "2026.08.19", "repository": "yt-dlp"},
            "formats": [],
        })
        .to_string(),
    );
    let FetchedVideo::Single(_video) = crate::runtime::tokio_rt()
        .block_on(fetch_video_page(
            &bin,
            "https://example.com/v",
            "none",
            std::time::Duration::from_secs(30),
            None,
            None,
        ))
        .expect("fake extract parses")
    else {
        panic!("single-video dump must parse as Single");
    };
    let argv = fake_bin_argv(&bin);
    assert!(
        !argv.lines().any(|l| l == "--flat-playlist"),
        "download resolve must not pass --flat-playlist, got:\n{argv}"
    );
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

#[test]
fn expand_child_target_routes_entries() {
    fn item(id: &str, page_url: &str) -> PlaylistItem {
        PlaylistItem {
            index: 1,
            id: id.to_string(),
            title: "t".to_string(),
            page_url: page_url.to_string(),
            duration: None,
        }
    }
    let tray = "https://www.instagram.com/stories/someuser/";
    // Story segments address their own pages, not the tray.
    assert_eq!(
        expand_child_target(tray, tray, &item("aye83DjauH", tray)),
        Some("https://www.instagram.com/stories/someuser/482584233761418119/".to_string())
    );
    // Ordinary entries keep their listed pages…
    assert_eq!(
        expand_child_target(
            "https://www.youtube.com/playlist?list=pl",
            "https://www.youtube.com/playlist?list=pl",
            &item("v1", "https://www.youtube.com/watch?v=v1")
        ),
        Some("https://www.youtube.com/watch?v=v1".to_string())
    );
    // …but never the probed collection itself (self-nesting guard keeps the collection error).
    assert_eq!(expand_child_target(tray, tray, &item("!!!", tray)), None);
    assert_eq!(
        expand_child_target(
            "https://www.youtube.com/playlist?list=pl",
            "https://www.youtube.com/playlist?list=pl",
            &item("pl", "https://www.youtube.com/playlist?list=pl")
        ),
        None
    );
    // Canonical drift: the guard checks both probed and extractor URLs.
    assert_eq!(
        expand_child_target(
            "https://www.youtube.com/playlist?list=pl/",
            "https://www.youtube.com/playlist?list=pl",
            &item("pl", "https://www.youtube.com/playlist?list=pl")
        ),
        None
    );
    // Unusable entries skip via the page-URL fallback path.
    assert_eq!(
        expand_child_target(
            "https://www.youtube.com/playlist?list=pl",
            "https://www.youtube.com/playlist?list=pl",
            &item("s9", "not a url")
        ),
        None
    );
}

#[test]
fn dump_json_flat_playlist_flag_per_caller() {
    // Dialog probe keeps --flat-playlist; single-item resolve must not.
    for (name, flat, want) in [
        ("grab-flatprobe-on", true, true),
        ("grab-flatprobe-off", false, false),
    ] {
        let bin = fake_argv_dump_bin(name, r#"{"id":"x","title":"T"}"#);
        crate::runtime::tokio_rt()
            .block_on(fetch_raw_dump_json(
                &bin,
                "https://example.com/v",
                "none",
                std::time::Duration::from_secs(30),
                None,
                flat,
            ))
            .expect("fake dump parses");
        let argv = fake_bin_argv(&bin);
        assert_eq!(
            argv.lines().any(|l| l == "--flat-playlist"),
            want,
            "flat={flat} argv:\n{argv}"
        );
        let _ = std::fs::remove_dir_all(bin.parent().unwrap());
    }
}

#[test]
fn fetch_video_page_returns_playlist_for_expansion() {
    // Collection URLs expand for dialog-less rows, never parse as empty videos.
    let bin = fake_argv_dump_bin(
        "grab-playlistshape",
        &serde_json::json!({
            "_type": "playlist",
            "id": "pl",
            "title": "Stories",
            "extractor": "instagram",
            "extractor_key": "Instagram",
            "entries": [
                {"_type": "url", "id": "s1", "title": "Video by onlyhopeyy",
                 "webpage_url": "https://www.instagram.com/stories/x/1/"},
            ],
        })
        .to_string(),
    );
    let res = crate::runtime::tokio_rt().block_on(fetch_video_page(
        &bin,
        "https://example.com/stories",
        "none",
        std::time::Duration::from_secs(30),
        None,
        None,
    ));
    match res {
        Ok(FetchedVideo::Playlist(pl)) => {
            assert_eq!(pl.items.len(), 1);
            assert_eq!(
                pl.items[0].page_url,
                "https://www.instagram.com/stories/x/1/"
            );
        }
        other => panic!("playlist-shaped output must expand, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

#[test]
fn sanitize_missing_codec_fields_still_parse() {
    // Sparse extractors omitting codec keys parse as adoptable Unknown, never a row killer.
    let mut value = serde_json::json!({
        "id": "sparse1",
        "title": "S",
        "formats": [
            {"format_id": "dl", "url": "https://cdn.example/dl.mp4"},
        ],
    });
    sanitize_video_json(&mut value);
    let video: yt_dlp::model::Video = serde_json::from_value(value).expect("sparse parses");
    assert_eq!(video.formats.len(), 1);
    let plan = plan_streams(&video, "best", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "dl");
}

#[test]
fn resume_plan_unknown_total_resumes_bytes_on_disk() {
    // No total means no overlong judgment: temp bytes resume, never wipe.
    let dir = test_manifest_dir("unknown-total");
    let dest = dir.join("Clip.mp4");
    let staging = test_staging(&dir);
    std::fs::write(staging.join("grab-media.mp4"), vec![0u8; 49]).unwrap();
    let m = test_manifest();
    let mut q = test_query(Some(&m), &dest, &staging);
    q.total = None;
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_proxy_env_sets_nothing_when_direct() {
    // Direct spawns set no proxy routing and inherit none; needs a reactor context.
    let out = crate::runtime::tokio_rt().block_on(async {
        let mut cmd = tokio::process::Command::new("env");
        cmd.env_remove("NO_PROXY").env_remove("no_proxy");
        cmd.env_remove("HTTP_PROXY").env_remove("http_proxy");
        cmd.env_remove("HTTPS_PROXY").env_remove("https_proxy");
        cmd.env_remove("ALL_PROXY").env_remove("all_proxy");
        apply_proxy_env(&mut cmd, None);
        cmd.output().await.expect("env runs")
    });
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("NO_PROXY=") || l.starts_with("no_proxy=")),
        "direct spawn leaks proxy env"
    );
}

#[test]
fn apply_proxy_env_stamps_no_proxy_when_proxied() {
    let proxy = crate::download::DownloadOptions {
        connections: 4,
        limit_rate: String::new(),
        proxy_mode: "manual".into(),
        proxy_type: "socks5".into(),
        proxy_host: "127.0.0.1".into(),
        proxy_port: 9050,
        cookies_browser: String::new(),
    }
    .proxy_config()
    .expect("well-formed")
    .expect("proxied");
    let out = crate::runtime::tokio_rt().block_on(async {
        let mut cmd = tokio::process::Command::new("env");
        cmd.env_remove("NO_PROXY").env_remove("no_proxy");
        apply_proxy_env(&mut cmd, Some(&proxy));
        cmd.output().await.expect("env runs")
    });
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().any(|l| l.starts_with("NO_PROXY=")),
        "proxied spawn missing NO_PROXY"
    );
}

#[test]
fn proxy_cli_args_passes_plain_url() {
    let proxy = crate::download::DownloadOptions {
        connections: 4,
        limit_rate: String::new(),
        proxy_mode: "manual".into(),
        proxy_type: "socks5".into(),
        proxy_host: "127.0.0.1".into(),
        proxy_port: 9050,
        cookies_browser: String::new(),
    }
    .proxy_config()
    .expect("well-formed")
    .expect("proxied");
    // yt-dlp gets one --proxy URL with no userinfo, so no credentials reach argv.
    assert_eq!(
        proxy_cli_args(Some(&proxy)),
        vec![
            "--proxy".to_string(),
            "socks5h://127.0.0.1:9050".to_string()
        ]
    );
    // ...and nothing when direct.
    assert!(proxy_cli_args(None).is_empty());
}

#[test]
fn dest_part_paths_sit_beside_finished_file() {
    // `<stem>.<kind>.<ext>` in the dest dir; deterministic across attempts for crash-resume.
    let dest = std::path::Path::new("/tmp/dl/Clip.mp4");
    assert_eq!(
        dest_part_path(dest, "video", "mp4"),
        std::path::Path::new("/tmp/dl/Clip.video.mp4")
    );
    assert_eq!(
        dest_part_path(dest, "audio", "webm"),
        std::path::Path::new("/tmp/dl/Clip.audio.webm")
    );
    assert_eq!(
        dest_part_path(dest, "hls", "%(ext)s"),
        std::path::Path::new("/tmp/dl/Clip.hls.%(ext)s")
    );
    assert_eq!(
        dest_part_path(dest, "live", "mp4"),
        std::path::Path::new("/tmp/dl/Clip.live.mp4")
    );
}

#[test]
fn ytdlp_output_template_doubles_percent_in_stem() {
    // `-o` is printf-style: literal `%` must be `%%` or the template misparses.
    assert_eq!(
        ytdlp_output_template(std::path::Path::new("/tmp/dl/100%.hls.%(ext)s")),
        "/tmp/dl/100%%.hls.%(ext)s"
    );
    assert_eq!(
        ytdlp_output_template(std::path::Path::new("/tmp/dl/Clip.hls.%(ext)s")),
        "/tmp/dl/Clip.hls.%(ext)s"
    );
}

#[test]
fn ytdlp_output_template_round_trips_existing_double_percent() {
    // A stem that already contains `%%` doubles again: yt-dlp renders
    // `%%%%` back to `%%`, so the on-disk name is unchanged.
    assert_eq!(
        ytdlp_output_template(std::path::Path::new("/tmp/dl/50%%off.live.mp4")),
        "/tmp/dl/50%%%%off.live.mp4"
    );
}

#[test]
fn dest_part_path_keeps_literal_percent_for_real_paths() {
    // The escape lives at the `-o` boundary only: on-disk part names keep
    // the single `%` so is_grab_part matches what yt-dlp renders.
    let dest = std::path::Path::new("/tmp/dl/100%.mp4");
    assert_eq!(
        dest_part_path(dest, "hls", "%(ext)s"),
        std::path::Path::new("/tmp/dl/100%.hls.%(ext)s")
    );
}

#[test]
fn hls_argv_escapes_percent_in_stem() {
    let mut job = direct_test_job();
    job.dest = std::path::PathBuf::from("/tmp/dl/100%.mp4");
    let dest = job.dest.clone();
    let argv = hls_download_argv(
        &job,
        "h1080",
        std::path::Path::new("/usr/bin/ffmpeg"),
        &dest,
    );
    let o = argv.iter().position(|a| a == "-o").expect("-o");
    assert_eq!(argv[o + 1], "/tmp/dl/100%%.hls.%(ext)s");
}

#[test]
fn live_capture_argv_escapes_percent_in_stem() {
    let job = live_test_job();
    let out = std::path::Path::new("/tmp/staging/100%.live.mp4");
    let argv = live_capture_argv(&job, "h720", out);
    let o = argv.iter().position(|a| a == "-o").expect("-o");
    assert_eq!(argv[o + 1], "/tmp/staging/100%%.live.mp4");
}

#[test]
fn remux_video_labels_are_not_translated() {
    // Container names are proper nouns: they must never go through
    // gettext (their msgids were missing from the POT anyway).
    assert_eq!(
        remux_video_labels(),
        vec![
            "Off".to_string(),
            "MP4".to_string(),
            "MKV".to_string(),
            "WebM".to_string(),
        ]
    );
}

#[test]
fn clean_dest_parts_keeps_finished_and_foreign_files() {
    let dir = std::env::temp_dir().join(format!("grab-cleanparts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("Clip.mp4");
    let keep = [
        "Clip.mp4",
        "Clip.srt",
        // Collected subtitle sidecars live outside the part namespace.
        "Clip.en.srt",
        "Other.video.mp4",
        "Clip.video-notes.txt",
    ];
    let drop = [
        "Clip.video.mp4",
        "Clip.video.mp4.part",
        "Clip.audio.webm",
        "Clip.audio.webm.part",
        "Clip.hls.mp4",
        "Clip.live.mp4.part",
        // Stale sidecars beside part files are part-namespace litter.
        "Clip.video.en.srt",
        "Clip.hls.en.srt",
    ];
    for n in keep.iter().chain(drop.iter()) {
        std::fs::write(dir.join(n), b"x").unwrap();
    }
    clean_dest_parts(&dest);
    for n in keep {
        assert!(dir.join(n).exists(), "{n} must survive");
    }
    for n in drop {
        assert!(!dir.join(n).exists(), "{n} must go");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_builders_use_machine_progress_and_ignore_config() {
    // Every yt-dlp spawn parses `--progress-template` lines (never human
    // prose) and ignores ambient user configs that could reshape argv.
    let out = std::path::Path::new("/tmp/staging/video.mp4");
    let job = direct_test_job();
    for argv in [
        unified_download_argv(
            &job,
            "v123+a456/bv*+ba/b",
            true,
            "mp4",
            std::path::Path::new("/usr/bin/ffmpeg"),
            out,
        ),
        live_capture_argv(&job, "h720", out),
        hls_download_argv(&job, "h720", std::path::Path::new("/usr/bin/ffmpeg"), out),
    ] {
        assert!(argv.contains(&"--ignore-config".to_string()), "{argv:?}");
        let t = argv
            .iter()
            .position(|a| a == "--progress-template")
            .expect("template flag");
        assert_eq!(argv[t + 1], YTDLP_PROGRESS_TEMPLATE, "{argv:?}");
        // The `web` player-client exclusion rides on every spawn: SABR
        // URL-less formats break the whole run otherwise (yt-dlp#12482).
        let e = argv
            .iter()
            .position(|a| a == "--extractor-args")
            .expect("extractor-args flag");
        assert_eq!(argv[e + 1], "youtube:player_client=-web", "{argv:?}");
        let sep = argv.iter().position(|a| a == "--").expect("separator");
        assert!(sep > e + 1, "{argv:?}");
    }
}
// ── subtitle sidecars ────────────────────────────────────────────────
#[test]
fn hls_argv_takes_subtitle_flags() {
    // The resolved subtitle language rides on the media leg: subtitle fetch
    // flags plus `--embed-subs` when the embed preference is on.
    let mut job = direct_test_job();
    job.quality = "best".into();
    job.subtitles = Some("ar".into());
    job.embed_subs = true;
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    for t in SUBTITLE_TOKENS {
        assert!(argv.iter().any(|a| a == *t), "{t} missing: {argv:?}");
    }
    assert!(argv.contains(&"--sub-langs".to_string()));
    let i = argv.iter().position(|a| a == "--sub-langs").unwrap();
    assert_eq!(argv[i + 1], "ar");
    assert!(
        argv.iter().any(|a| a == "--embed-subs"),
        "--embed-subs missing: {argv:?}"
    );
    // Merge flags stay alongside the subtitle flags.
    assert!(argv.contains(&"--merge-output-format".to_string()));
}

#[test]
fn every_download_path_embeds_metadata() {
    // The id is no longer in the filename, so only the file's own tags keep
    // it (`--embed-metadata` writes `webpage_url` into `comment`). It used
    // to be passed only when merging, leaving every other path untagged.
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let ff = std::path::Path::new("/usr/bin/ffmpeg");

    let mut merged = direct_test_job();
    merged.quality = "1080".into();
    let m = unified_download_argv(&merged, "a456/ba/b", true, "", ff, out);
    assert!(
        m.contains(&"--embed-metadata".to_string()),
        "merged path lost its tags"
    );

    // Progressive single stream: not merging, and previously untagged.
    let mut progressive = direct_test_job();
    progressive.quality = "best".into();
    let p = unified_download_argv(&progressive, "a456/ba/b", false, "", ff, out);
    assert!(
        p.contains(&"--embed-metadata".to_string()),
        "a progressive download has no tags, so its id is lost with the filename"
    );

    // Audio-only: same reasoning, and the most common id-bearing case.
    let mut audio = direct_test_job();
    audio.quality = "best".into();
    audio.audio_only = true;
    let a = unified_download_argv(&audio, "a456/ba/b", false, "", ff, out);
    assert!(
        a.contains(&"--embed-metadata".to_string()),
        "an audio-only download has no tags, so its id is lost with the filename"
    );

    // The HLS and unified legs share the builder; the live leg has its own
    // argv and is covered by its own test.
    let mut hls = direct_test_job();
    hls.quality = "best".into();
    hls.audio_only = true;
    let h = hls_download_argv(&hls, "h1080", ff, std::path::Path::new("/tmp/dl/v.mkv"));
    assert!(
        h.contains(&"--embed-metadata".to_string()),
        "the HLS leg never embedded metadata, so its id is lost with the filename"
    );
}

#[test]
fn unified_argv_extracts_audio_for_audio_only() {
    // Dialog audio-only choice on a direct row: extract to m4a (native
    // containers vary by codec) instead of merging; never pass subtitle flags.
    let mut job = direct_test_job();
    job.audio_only = true;
    job.subtitles = Some("en".into());
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "a456/ba/b",
        false,
        "",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(argv.contains(&"--extract-audio".to_string()));
    let af = argv
        .iter()
        .position(|a| a == "--audio-format")
        .expect("format");
    assert_eq!(argv[af + 1], "m4a");
    assert!(!argv.iter().any(|a| a == "--merge-output-format"));
    assert_no_subtitle_tokens(&argv);
}

#[test]
fn hls_argv_extracts_audio_for_audio_only() {
    // Dialog audio-only choice on an HLS row: extract to m4a instead
    // of merging, and never pass subtitle flags.
    let mut job = direct_test_job();
    job.quality = "best".into();
    job.audio_only = true;
    job.subtitles = Some("en".into());
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(argv.contains(&"--extract-audio".to_string()));
    let af = argv
        .iter()
        .position(|a| a == "--audio-format")
        .expect("format");
    assert_eq!(argv[af + 1], "m4a");
    assert!(!argv.iter().any(|a| a == "--merge-output-format"));
    assert_no_subtitle_tokens(&argv);
}

#[test]
fn live_capture_argv_never_takes_subtitles() {
    // Captioning a live edge is not a download: even with a language
    // configured, live captures must not pass subtitle flags.
    let mut job = live_test_job();
    job.subtitles = Some("en".into());
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert_no_subtitle_tokens(&argv);
}

// ── subtitle embedding ───────────────────────────────────────────────

#[test]
fn unified_argv_embeds_subs_when_enabled() {
    // yt-dlp's own embedder: `--embed-subs` rides on the media leg when the
    // user opted in, alongside the subtitle fetch flags.
    let mut job = direct_test_job();
    job.embed_subs = true;
    job.subtitles = Some("en".into());
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "v123+a456/bv*+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(
        argv.iter().any(|a| a == "--embed-subs"),
        "--embed-subs missing: {argv:?}"
    );
    for t in SUBTITLE_TOKENS {
        assert!(argv.iter().any(|a| a == *t), "{t} missing: {argv:?}");
    }
}

#[test]
fn hls_argv_embeds_subs_when_enabled() {
    // Same as the unified leg: `--embed-subs` on the media leg when enabled.
    let mut job = direct_test_job();
    job.quality = "best".into();
    job.embed_subs = true;
    job.subtitles = Some("en".into());
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(
        argv.iter().any(|a| a == "--embed-subs"),
        "--embed-subs missing: {argv:?}"
    );
    for t in SUBTITLE_TOKENS {
        assert!(argv.iter().any(|a| a == *t), "{t} missing: {argv:?}");
    }
}

#[test]
fn live_capture_argv_never_takes_embed_subs() {
    // Live captures record raw transport streams; no post-processing
    // leg exists, so the embed flag stays off even when enabled.
    let mut job = live_test_job();
    job.embed_subs = true;
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--embed-subs"));
}

#[test]
fn unified_argv_leaves_fragments_serial() {
    // yt-dlp legs stay at the serial fragment default even when the app's
    // own segmented engine runs hot: fragment floods trip 429s.
    let job = direct_test_job();
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--concurrent-fragments"));
}

#[test]
fn unified_argv_cuts_sponsors_when_enabled() {
    // Opt-in post-processing: the "sponsor" category only, nothing else.
    let mut job = direct_test_job();
    job.sponsorblock_remove = true;
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    let pos = argv
        .iter()
        .position(|a| a == "--sponsorblock-remove")
        .expect("flag");
    assert_eq!(argv[pos + 1], "sponsor");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(
        pos < sep,
        "sponsorblock flag must precede the URL separator"
    );
    // Default off: no trace of the flag.
    job.sponsorblock_remove = false;
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--sponsorblock-remove"));
}

#[test]
fn unified_argv_marks_sponsors_when_enabled() {
    // Opt-in post-processing: the "sponsor" category only, nothing else.
    let mut job = direct_test_job();
    job.sponsorblock_mark = true;
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    let pos = argv
        .iter()
        .position(|a| a == "--sponsorblock-mark")
        .expect("flag");
    assert_eq!(argv[pos + 1], "sponsor");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(
        pos < sep,
        "sponsorblock flag must precede the URL separator"
    );
    // Default off: no trace of the flag.
    job.sponsorblock_mark = false;
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--sponsorblock-mark"));
}

#[test]
fn hls_argv_leaves_fragments_serial() {
    // Same serial default as every other yt-dlp leg: the connections
    // setting is the app engine's own.
    let job = direct_test_job();
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(!argv.iter().any(|a| a == "--concurrent-fragments"));
}

#[test]
fn hls_argv_cuts_sponsors_when_enabled() {
    let mut job = direct_test_job();
    job.sponsorblock_remove = true;
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    let pos = argv
        .iter()
        .position(|a| a == "--sponsorblock-remove")
        .expect("flag");
    assert_eq!(argv[pos + 1], "sponsor");
    // Default off: no trace of the flag.
    job.sponsorblock_remove = false;
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(!argv.iter().any(|a| a == "--sponsorblock-remove"));
}

#[test]
fn hls_argv_marks_sponsors_when_enabled() {
    let mut job = direct_test_job();
    job.sponsorblock_mark = true;
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    let pos = argv
        .iter()
        .position(|a| a == "--sponsorblock-mark")
        .expect("flag");
    assert_eq!(argv[pos + 1], "sponsor");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(
        pos < sep,
        "sponsorblock flag must precede the URL separator"
    );
    // Default off: no trace of the flag.
    job.sponsorblock_mark = false;
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(!argv.iter().any(|a| a == "--sponsorblock-mark"));
}

#[test]
fn hls_argv_remuxes_video_when_enabled() {
    let mut job = direct_test_job();
    job.remux_video = Some("mkv".to_string());
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    let pos = argv
        .iter()
        .position(|a| a == "--remux-video")
        .expect("flag");
    assert_eq!(argv[pos + 1], "mkv");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(pos < sep, "remux flag must precede the URL separator");
    // Default off: no trace of the flag.
    job.remux_video = None;
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(!argv.iter().any(|a| a == "--remux-video"));
}

#[test]
fn live_capture_argv_never_remuxes_video() {
    // Live captures merge straight to disk: even opted in, the flag
    // must not appear (the builder structurally ignores the field).
    let mut job = live_test_job();
    job.remux_video = Some("mkv".to_string());
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--remux-video"));
}

#[test]
fn live_capture_argv_has_no_concurrent_fragments() {
    // Every yt-dlp leg runs fragments at the serial default; live
    // edge recording never tuned it in the first place.
    let job = live_test_job();
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--concurrent-fragments"));
}

#[test]
fn live_capture_argv_never_cuts_sponsors() {
    // The live edge cannot know future segments: even opted in, live
    // captures must not pass the flag.
    let mut job = live_test_job();
    job.sponsorblock_remove = true;
    job.sponsorblock_mark = true;
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--sponsorblock-remove"));
    assert!(!argv.iter().any(|a| a == "--sponsorblock-mark"));
}

#[test]
fn unified_argv_embeds_chapters_when_enabled() {
    // Opt-in post-processing: chapter markers land in the finished file.
    let mut job = direct_test_job();
    job.embed_chapters = true;
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    let pos = argv
        .iter()
        .position(|a| a == "--embed-chapters")
        .expect("flag");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(pos < sep, "chapters flag must precede the URL separator");
    // Default off: no trace of the flag.
    job.embed_chapters = false;
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--embed-chapters"));
}

#[test]
fn unified_argv_remuxes_video_when_enabled() {
    // Opt-in post-processing: the finished file is remuxed into the
    // chosen container without re-encoding.
    let mut job = direct_test_job();
    job.remux_video = Some("mkv".to_string());
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    let pos = argv
        .iter()
        .position(|a| a == "--remux-video")
        .expect("flag");
    assert_eq!(argv[pos + 1], "mkv");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(pos < sep, "remux flag must precede the URL separator");
    // Default off: no trace of the flag.
    job.remux_video = None;
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--remux-video"));
}

#[test]
fn remux_video_active_allowlists_targets() {
    // A hand-edited dconf value outside the ComboRow's list resolves to
    // `None`, so yt-dlp never receives an unrecognised target.
    assert_eq!(remux_video_active("off"), None);
    assert_eq!(remux_video_active("mkv"), Some("mkv".to_string()));
    assert_eq!(remux_video_active("MP4"), Some("mp4".to_string()));
    assert_eq!(remux_video_active("avi"), None);
    assert_eq!(remux_video_active(""), None);
}

#[test]
fn hls_argv_embeds_chapters_when_enabled() {
    let mut job = direct_test_job();
    job.embed_chapters = true;
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(argv.iter().any(|a| a == "--embed-chapters"));
    // Default off: no trace of the flag.
    job.embed_chapters = false;
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(!argv.iter().any(|a| a == "--embed-chapters"));
}

#[test]
fn audio_only_rows_still_get_chapters() {
    // Unlike --embed-subs, chapters apply to audio-only rows too (m4a carries
    // them); pinned so a flag-block reorder can't silently drop the flag.
    let mut job = direct_test_job();
    job.audio_only = true;
    job.embed_chapters = true;
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bestaudio",
        false,
        "m4a",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(argv.iter().any(|a| a == "--embed-chapters"));
}

#[test]
fn live_capture_argv_never_embeds_chapters() {
    // Live captures record raw transport streams: no post-processing leg
    // exists, so even opted in this flag must not appear.
    let mut job = live_test_job();
    job.embed_chapters = true;
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--embed-chapters"));
}

#[test]
fn vod_and_live_argv_never_emit_removed_tuning_flags() {
    // The old Advanced tuning knobs are gone: all left at yt-dlp defaults,
    // so none of their flags may appear on any leg.
    let gone = [
        "--retries",
        "--retry-sleep",
        "--sleep-interval",
        "--max-sleep-interval",
        "--sleep-requests",
        "--socket-timeout",
        "--throttled-rate",
        "--extractor-retries",
        "--user-agent",
    ];
    let job = direct_test_job();
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let unified = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let hls = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    let live = live_capture_argv(
        &live_test_job(),
        "h720",
        std::path::Path::new("/tmp/dl/v.live.ts"),
    );
    for argv in [&unified, &hls, &live] {
        for flag in gone {
            assert!(
                !argv.iter().any(|a| a == flag),
                "{flag} must not be emitted"
            );
        }
    }
}

#[test]
fn unified_argv_ratelimit_when_set() {
    // Opt-in throttle: the shared speed limit caps VOD legs via
    // `--ratelimit` (plain bytes; yt-dlp accepts the raw rate).
    let mut job = direct_test_job();
    job.speed_limit = Some(512_000);
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    let pos = argv.iter().position(|a| a == "--ratelimit").expect("flag");
    assert_eq!(argv[pos + 1], "512000");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(pos < sep, "ratelimit must precede the URL separator");
    // Default off: no trace of the flag.
    job.speed_limit = None;
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--ratelimit"));
}

#[test]
fn unified_argv_mtime_when_set() {
    // Opt-in fidelity: the finished file takes the server's Last-Modified date
    // instead of the download time.
    let mut job = direct_test_job();
    job.keep_server_date = true;
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    let pos = argv.iter().position(|a| a == "--mtime").expect("flag");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(pos < sep, "mtime must precede the URL separator");
    // Default off: no trace of the flag.
    job.keep_server_date = false;
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--mtime"));
}

#[test]
fn hls_argv_ratelimit_when_set() {
    let mut job = direct_test_job();
    job.speed_limit = Some(2_097_152);
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    let pos = argv.iter().position(|a| a == "--ratelimit").expect("flag");
    assert_eq!(argv[pos + 1], "2097152");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(pos < sep, "ratelimit must precede the URL separator");
    // Default off: no trace of the flag.
    job.speed_limit = None;
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(!argv.iter().any(|a| a == "--ratelimit"));
}

#[test]
fn live_capture_argv_never_ratelimit() {
    // Throttling an endless capture would fall behind the live edge, so
    // even opted in the flag must not appear.
    let mut job = live_test_job();
    job.speed_limit = Some(512_000);
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--ratelimit"));
}

#[test]
fn hls_argv_mtime_when_set() {
    let mut job = direct_test_job();
    job.keep_server_date = true;
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    let pos = argv.iter().position(|a| a == "--mtime").expect("flag");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(pos < sep, "mtime must precede the URL separator");
    // Default off: no trace of the flag.
    job.keep_server_date = false;
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(!argv.iter().any(|a| a == "--mtime"));
}

#[test]
fn live_capture_argv_never_mtime() {
    // The live path remuxes through ffmpeg after capture, clobbering any
    // mtime yt-dlp set: excluded like the other VOD-only opt-ins.
    let mut job = live_test_job();
    job.keep_server_date = true;
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--mtime"));
}

#[test]
fn audio_only_legs_never_pass_an_extraction_quality() {
    // The preference is gone: audio-only extraction always runs at
    // yt-dlp's own default, so the flag must not appear on any leg.
    let mut job = direct_test_job();
    job.audio_only = true;
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "ba/b",
        false,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(argv.contains(&"--extract-audio".to_string()));
    assert!(argv.contains(&"--audio-format".to_string()));
    assert!(!argv.iter().any(|a| a == "--audio-quality"));
    let dest = std::path::Path::new("/tmp/dl/v.m4a");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(argv.contains(&"--extract-audio".to_string()));
    assert!(argv.contains(&"--audio-format".to_string()));
    assert!(!argv.iter().any(|a| a == "--audio-quality"));
    // Video legs never extract, so the flag must not appear there either.
    job.audio_only = false;
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--audio-quality"));
}

#[test]
fn live_capture_argv_never_audio_quality() {
    // Live rows remux through ffmpeg after capture — there is no
    // yt-dlp extraction step, so the flag must not appear.
    let mut job = live_test_job();
    job.audio_only = true;
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--audio-quality"));
}

#[test]
fn live_capture_argv_live_from_start_when_enabled() {
    // Opt-in live capture: record from the beginning of the stream
    // instead of the live edge.
    let mut job = live_test_job();
    job.live_from_start = true;
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    let pos = argv
        .iter()
        .position(|a| a == "--live-from-start")
        .expect("flag");
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(pos < sep, "flag must precede the URL separator");
    // Disabled: no trace of the flag.
    job.live_from_start = false;
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--live-from-start"));
}

#[test]
fn fallback_to_live_edge_retries_unstopped_from_start_miss() {
    // The from-start attempt recorded nothing and wasn't stopped: one
    // retry from the live edge.
    assert!(fallback_to_live_edge(true, true, false, false));
}

#[test]
fn fallback_to_live_edge_never_overrides_or_repeats() {
    // Toggle off: the user's choice stands, no retry.
    assert!(!fallback_to_live_edge(true, false, false, false));
    // Not a live row: the builder never passes the flag anyway.
    assert!(!fallback_to_live_edge(false, true, false, false));
    // Stopped attempt: Stop must never come back as a fresh capture.
    assert!(!fallback_to_live_edge(true, true, true, false));
    // Already retried: exactly once, then the error stands.
    assert!(!fallback_to_live_edge(true, true, false, true));
}

#[test]
fn live_from_start_stays_off_non_live_rows() {
    // The flag is live-only: a non-live row never emits it, even with the
    // preference opted in — no live edge exists to rewind to.
    let mut job = direct_test_job();
    job.live_from_start = true;
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "bv+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    assert!(!argv.iter().any(|a| a == "--live-from-start"));
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert!(!argv.iter().any(|a| a == "--live-from-start"));
    // Even the live builder refuses a misrouted non-live row.
    let mut live_job = live_test_job();
    live_job.is_live = false;
    let argv = live_capture_argv(&live_job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert!(!argv.iter().any(|a| a == "--live-from-start"));
}

#[test]
fn subtitle_language_index_value_round_trip() {
    assert_eq!(subtitle_language_index("off"), 0);
    assert_eq!(subtitle_language_value(0), "off");
    assert_eq!(subtitle_language_index("en"), 1);
    assert_eq!(subtitle_language_value(1), "en");
    assert_eq!(subtitle_language_index("zh"), 17);
    assert_eq!(subtitle_language_value(17), "zh");
    // Unknown settings and bad indexes fall back to Off (the schema default).
    assert_eq!(subtitle_language_index("xx"), 0);
    assert_eq!(subtitle_language_index(""), 0);
    assert_eq!(subtitle_language_value(99), "off");
    assert_eq!(
        subtitle_language_labels().len(),
        SUBTITLE_LANGUAGE_VALUES.len()
    );
}

/// Subtitle tokens that must never leak onto non-video legs. (`--ffmpeg-location`
/// is excluded: HLS always carries it; the part/live builders must not.)
const SUBTITLE_TOKENS: &[&str] = &[
    "--write-subs",
    "--sub-langs",
    "--write-auto-subs",
    "--convert-subs",
];

fn assert_no_subtitle_tokens(argv: &[String]) {
    for t in SUBTITLE_TOKENS {
        assert!(!argv.iter().any(|a| a == *t), "{t} leaked: {argv:?}");
    }
}

#[test]
fn subtitle_lang_active_allowlist() {
    // The configured code resolves verbatim; `off`, empty and unknown codes
    // (hand-edited dconf) resolve to off — never passed to `--sub-langs`.
    assert_eq!(subtitle_lang_active("en"), Some("en".to_string()));
    assert_eq!(subtitle_lang_active("zh"), Some("zh".to_string()));
    assert_eq!(subtitle_lang_active("off"), None);
    assert_eq!(subtitle_lang_active(""), None);
    assert_eq!(subtitle_lang_active("xx"), None);
    assert_eq!(subtitle_lang_active(" "), None);
    // Normalization: surrounding whitespace and case fold onto the list.
    assert_eq!(subtitle_lang_active(" EN "), Some("en".to_string()));
    // Regex-shaped values never reach yt-dlp (`--sub-langs` accepts
    // patterns, so only exact allowlist hits pass).
    assert_eq!(subtitle_lang_active("en,fr"), None);
    assert_eq!(subtitle_lang_active("en.*"), None);
}

fn subtitle_available(langs: &[&str]) -> std::collections::HashSet<String> {
    langs.iter().map(|l| l.to_string()).collect()
}

#[test]
fn pick_subtitle_lang_prefers_exact_match() {
    // The preferred language wins when the video offers it verbatim.
    let avail = subtitle_available(&["en", "fr", "de"]);
    assert_eq!(
        pick_subtitle_lang(&avail, &["fr", "en"]),
        Some("fr".to_string())
    );
}

#[test]
fn pick_subtitle_lang_falls_back_to_english() {
    // Preferred language missing: English is the fallback when offered.
    let avail = subtitle_available(&["en", "es"]);
    assert_eq!(
        pick_subtitle_lang(&avail, &["de", "en"]),
        Some("en".to_string())
    );
}

#[test]
fn pick_subtitle_lang_accepts_region_variant() {
    // `en` matches `en-us`: the concrete offered key is returned so
    // `--sub-langs` matches exactly.
    let avail = subtitle_available(&["en-us", "fr"]);
    assert_eq!(
        pick_subtitle_lang(&avail, &["en", "en"]),
        Some("en-us".to_string())
    );
}

#[test]
fn pick_subtitle_lang_none_when_nothing_offered() {
    // Neither the preference nor English is available: no subtitles.
    let avail = subtitle_available(&["fr", "de"]);
    assert_eq!(pick_subtitle_lang(&avail, &["ja", "en"]), None);
    let empty = subtitle_available(&[]);
    assert_eq!(pick_subtitle_lang(&empty, &["en", "en"]), None);
}

#[test]
fn pick_subtitle_lang_rejects_prefix_collisions() {
    // `en` must not match `eng` — only a `-`/`_` region separator counts.
    let avail = subtitle_available(&["eng", "fr"]);
    assert_eq!(pick_subtitle_lang(&avail, &["en", "en"]), None);
}

/// Fake yt-dlp for the subtitle language probe: prints the given info JSON on
/// `--dump-json`, fails otherwise.
fn fake_ytdlp_probe(dir: &std::path::Path, json: &str, fail: bool) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-probe");
    let script = if fail {
        "#!/bin/sh\nexit 1\n".to_string()
    } else {
        "#!/bin/sh\nfor a in \"$@\"; do\n    if [ \"$a\" = \"--dump-json\" ]; then\n        printf '%s' 'JSON'\n        exit 0\n    fi\ndone\nexit 1\n"
            .replace("JSON", &json.replace('\'', "'\"'\"'"))
    };
    std::fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn probe_test_job(subtitles: Option<&str>) -> VideoJob {
    let mut job = direct_test_job();
    job.subtitles = subtitles.map(|s| s.to_string());
    job
}

#[test]
fn resolve_subtitle_lang_picks_preferred_when_offered() {
    let dir = std::env::temp_dir().join(format!("grab-probe-pref-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_probe(
        &dir,
        r#"{"subtitles":{"fr":[{"url":"http://x/fr","ext":"vtt"}]},"automatic_captions":{"en":[{"url":"http://x/en","ext":"vtt"}]}}"#,
        false,
    );
    let job = probe_test_job(Some("fr"));
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res =
        crate::runtime::tokio_rt().block_on(resolve_subtitle_lang(&fake, &job, &mut abort_rx));
    assert_eq!(res, Ok(Some("fr".to_string())));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_subtitle_lang_falls_back_to_english() {
    // Preferred language not offered, but English is (via automatic
    // captions): the English fallback wins.
    let dir = std::env::temp_dir().join(format!("grab-probe-en-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_probe(
        &dir,
        r#"{"subtitles":{"fr":[{"url":"http://x/fr","ext":"vtt"}]},"automatic_captions":{"en":[{"url":"http://x/en","ext":"vtt"}]}}"#,
        false,
    );
    let job = probe_test_job(Some("de"));
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res =
        crate::runtime::tokio_rt().block_on(resolve_subtitle_lang(&fake, &job, &mut abort_rx));
    assert_eq!(res, Ok(Some("en".to_string())));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_subtitle_lang_probe_failure_drops_subtitles() {
    // A dead probe (network, HTTP 429) yields no subtitles rather than
    // failing: the media download proceeds without subtitle flags.
    let dir = std::env::temp_dir().join(format!("grab-probe-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_probe(&dir, "", true);
    let job = probe_test_job(Some("en"));
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res =
        crate::runtime::tokio_rt().block_on(resolve_subtitle_lang(&fake, &job, &mut abort_rx));
    assert_eq!(res, Ok(None));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_subtitle_lang_no_preference_means_no_probe() {
    // Subtitles off: resolves to None without spawning anything.
    let job = probe_test_job(None);
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(resolve_subtitle_lang(
        std::path::Path::new("/nonexistent/yt-dlp"),
        &job,
        &mut abort_rx,
    ));
    assert_eq!(res, Ok(None));
}

#[test]
fn resolve_subtitle_lang_spawn_failure_drops_subtitles() {
    // The probe binary can't even start: the download must proceed without
    // subtitles (`Ok(None)`), not stop as if aborted (`Err`).
    let job = probe_test_job(Some("en"));
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(resolve_subtitle_lang(
        std::path::Path::new("/nonexistent/yt-dlp"),
        &job,
        &mut abort_rx,
    ));
    assert_eq!(res, Ok(None));
}

#[test]
fn sidecar_path_for_cases() {
    // yt-dlp drops `--write-subs` sidecars as `<out-stem>.<lang>.srt`
    // beside the `-o` path; Grab keeps the same shape beside the dest.
    assert_eq!(
        sidecar_path_for(std::path::Path::new("/tmp/dl/Clip.video.mp4"), "en"),
        std::path::Path::new("/tmp/dl/Clip.video.en.srt")
    );
    assert_eq!(
        sidecar_path_for(std::path::Path::new("/tmp/dl/Clip.mp4"), "ar"),
        std::path::Path::new("/tmp/dl/Clip.ar.srt")
    );
}

#[test]
fn finished_sidecars_escape_part_namespace() {
    // Structural: a collected `<stem>.<lang>.srt` can never match Grab's part
    // infixes, so `clean_dest_parts` leaves finished sidecars alone.
    for lang in SUBTITLE_LANGUAGE_VALUES {
        if *lang == "off" {
            continue;
        }
        let name = format!("Clip.{lang}.srt");
        assert!(!is_grab_part(&name, "Clip"), "{name} must not match");
    }
}

#[test]
fn collect_sidecar_moves_and_ignores_missing() {
    let dir = std::env::temp_dir().join(format!("grab-sidecar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("Clip.video.mp4");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dir.join("Clip.video.en.srt"), b"subs").unwrap();
    collect_sidecar(&sidecar_path_for(&out, "en"), &dest, "en");
    assert_eq!(std::fs::read(dir.join("Clip.en.srt")).unwrap(), b"subs");
    assert!(!dir.join("Clip.video.en.srt").exists());
    // A page that published no subtitles: missing source is a quiet no-op.
    collect_sidecar(&sidecar_path_for(&out, "fr"), &dest, "fr");
    assert!(!dir.join("Clip.fr.srt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp for HLS. Phase-aware like the real two-phase flow: the media leg
/// writes bytes, the `--skip-download` subtitle leg drops an `en` sidecar
/// beside the `-o` template (what the real binary does for `--write-subs`).
fn fake_ytdlp_hls_subs(dir: &std::path::Path) -> std::path::PathBuf {
    // Simulates yt-dlp for the HLS subtitle flow: the language probe
    // (`--dump-json`) reports en/fr subtitles; the media leg writes the media
    // file plus the `<stem>.hls.en.srt` sidecar that `--write-subs` produces.
    let bin = dir.join("fake-ytdlp-hls-subs");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
dump=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    if [ "$a" = "--dump-json" ]; then dump="1"; fi
    prev="$a"
done
if [ -n "$dump" ]; then
    printf '{"subtitles":{"en":[{"url":"http://x/en","ext":"vtt"}],"fr":[{"url":"http://x/fr","ext":"vtt"}]},"automatic_captions":{}}'
    exit 0
fi
stem="$(basename "$out" | sed 's/%(ext)s/mp4/')"
media="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$media"
side="$(printf '%s' "$stem" | sed 's/\.mp4$//').en.srt"
printf 'subbytes' > "$(dirname "$out")/$side"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn hls_collects_sidecar_beside_finished_file() {
    // End to end on the Critical path: the sidecar dropped as `<stem>.hls.en.srt`
    // must be collected as `<stem>.en.srt` — never orphaned under the part name.
    let dir = std::env::temp_dir().join(format!("grab-fakehls-subs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls_subs(&dir);
    let staging = dir.join("staging");
    let job = VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        playlist_item_id: None,
        quality: "best".into(),
        audio_only: false,
        dest: dir.join("v.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: Some("en".into()),
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"hlsbytes");
    assert_eq!(std::fs::read(dir.join("v.en.srt")).unwrap(), b"subbytes");
    assert!(
        !dir.join("v.hls.en.srt").exists(),
        "part-namespaced sidecar must be collected, not orphaned"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hls_embed_skips_sidecar_collection() {
    // Embed mode lets yt-dlp mux the tracks itself (`--embed-subs`): the part
    // sidecar it also wrote must be deleted outright, never collected.
    let dir = std::env::temp_dir().join(format!("grab-fakehls-embed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls_subs(&dir);
    let staging = dir.join("staging");
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    job.subtitles = Some("en".into());
    job.embed_subs = true;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"hlsbytes");
    assert!(
        !dir.join("v.en.srt").exists(),
        "embedded subtitles must not leave a sidecar"
    );
    assert!(
        !dir.join("v.hls.en.srt").exists(),
        "part-namespaced sidecar must not be orphaned"
    );
    assert!(
        !dir.join("v.mp4.embedtmp").exists(),
        "embed temp must be renamed away, not littered"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn subtitle_leg_failure_cannot_sink_media() {
    // The 429 regression, end to end: the subtitle language probe fails like
    // an HTTP 429. The download must still succeed with the media intact and
    // no subtitles — the probe failure only logs, and the media leg runs
    // without subtitle flags.
    let dir = std::env::temp_dir().join(format!("grab-fakehls-subfail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-subfail");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
skip=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    if [ "$a" = "--skip-download" ]; then skip="1"; fi
    prev="$a"
done
if [ -n "$skip" ]; then
    echo "ERROR: [Video] 1: Unable to download subtitles: HTTP Error 429: Too Many Requests" >&2
    exit 1
fi
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let staging = dir.join("staging");
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    job.subtitles = Some("en".into());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &bin,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        matches!(res, Ok(Some(_))),
        "subtitle 429 must not fail the download: {res:?}"
    );
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"hlsbytes");
    assert!(
        !dir.join("v.en.srt").exists(),
        "no subtitles were fetched, so no sidecar may appear"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp for the unified path: expands `%(ext)s`, prints template
/// progress + a merge line + the after_move path. Phase-aware like the real
/// two-phase flow: the media leg writes bytes, the `--skip-download` subtitle
/// leg writes only the sidecar.
fn fake_ytdlp(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
dump=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    if [ "$a" = "--dump-json" ]; then dump="1"; fi
    prev="$a"
done
echo "$@" >> "$(dirname "$out").argv.log"
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
stem="$(basename "$out" .mp4)"
if [ -n "$dump" ]; then
    printf '{"subtitles":{"en":[{"url":"http://x/en","ext":"vtt"}]},"automatic_captions":{}}'
    exit 0
fi
echo "[Grab];downloading;7;7;7;1000;0"
echo "[Merger] Merging formats"
echo "$out"
printf 'unified' > "$out"
printf 'subtitles' > "$(dirname "$out")/$stem.en.srt"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Fake yt-dlp that fails like a 403: surfaces the tail, no retry.
fn fake_ytdlp_fail(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-fail");
    std::fs::write(
        &bin,
        "#!/bin/sh\necho 'ERROR: [Video] 1: Unable to download: 403 Forbidden' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}
/// Fake yt-dlp for live that behaves like a killed recorder: bytes land in
/// the `.part` shell, no progress line, then a clean exit after a beat.
fn fake_ytdlp_live_shell_only(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-live-shell");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
printf 'tsbytes' > "$out.part"
sleep 2
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_part_shell_announces_recording_and_is_swept() {
    // Two live bugs in one: the watcher polled the `-o` path (empty until
    // finalize) instead of the growing `.part` shell; the shell also survived stop.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-shell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live_shell_only(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &AttemptGate::new(),
        "https://youtu.be/grabtest",
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"tsbytes");
    let phases: Vec<String> = {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::engine_msg::EngineMsg::Phase(p) = msg {
                out.push(p);
            }
        }
        out
    };
    assert!(
        phases.iter().any(|p| p.contains("Recording")),
        "shell growth must announce Recording, phases seen: {phases:?}"
    );
    assert!(
        !dir.join("v.live.mp4.part").exists(),
        "stopped capture must not leave its shell behind"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stem_reserved_in_matches_namespace_only() {
    let names: Vec<String> = [
        "Clip.video.mp4",
        "Clip.video.mp4.part",
        "Clip.audio.webm",
        "Clip.hls.mp4",
        "Clip.live.ts",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert!(stem_reserved_in(&names, "Clip"));
    // Finished files, foreign stems and lookalikes reserve nothing.
    // (Sidecars live in the `subs` list below — they reserve by design.)
    let clean: Vec<String> = [
        "Clip.mp4",
        "Other.video.mp4",
        "Clip.video-notes.txt",
        "Clip.mp4.part",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert!(!stem_reserved_in(&clean, "Clip"));
    assert!(!stem_reserved_in(&names, "Other"));
    // Dot boundary: a longer stem sharing the prefix does not match.
    assert!(!stem_reserved_in(&names, "Clippy"));
    // Empty stems never match, even against dotfiles shaped like parts.
    assert!(!stem_reserved_in(&[".video.mp4".to_string()], ""));
    assert!(!stem_reserved_in(&[], "Clip"));
    // Subtitle sidecars reserve the stem (delete trashes every offered code);
    // bare or unknown-code names do not.
    let subs: Vec<String> = ["Clip.en.srt", "Clip.fr.srt"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(stem_reserved_in(&subs, "Clip"));
    let bare: Vec<String> = ["Clip.srt", "Clip.eng.srt", "Clip.EN.srt"]
        // "eng" is unknown *today*: if it ever joins the offered list,
        // this case flips to reserving (correctly) — update then.
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(!stem_reserved_in(&bare, "Clip"));
}

#[test]
fn dir_file_names_snapshots_dir() {
    let dir = std::env::temp_dir().join(format!("grab-dirnames-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.iso"), b"x").unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let mut names = dir_file_names(&dir);
    names.sort();
    assert_eq!(names, vec!["a.iso".to_string(), "sub".to_string()]);
    assert!(dir_file_names(&dir.join("missing-dir")).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn collect_sidecar_never_clobbers() {
    // A foreign sidecar arriving mid-download survives collection: ours stays
    // beside the part file for row removal to sweep.
    let dir = std::env::temp_dir().join(format!("grab-sidecar-noclobber-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("Clip.video.mp4");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dir.join("Clip.video.en.srt"), b"ours").unwrap();
    std::fs::write(dir.join("Clip.en.srt"), b"foreign").unwrap();
    collect_sidecar(&sidecar_path_for(&out, "en"), &dest, "en");
    assert_eq!(std::fs::read(dir.join("Clip.en.srt")).unwrap(), b"foreign");
    assert_eq!(
        std::fs::read(dir.join("Clip.video.en.srt")).unwrap(),
        b"ours"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── unified direct downloads (fake yt-dlp) ───────────────────────────

#[test]
fn unified_runner_downloads_claims_and_collects() {
    // One spawn: merge flags on the wire, after_move discovery, atomic claim,
    // sidecar collected, Merging phase announced, progress reported.
    let dir = std::env::temp_dir().join(format!("grab-fakeunified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    job.subtitles = Some("en".into());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let gate = AttemptGate::new();
    let res = crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &gate,
        "v123+a456/bv*+ba/b",
        Some("mp4"),
        Some(7),
        &mut abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(7))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"unified");
    assert!(
        gate.was_delivered(),
        "the unified leg delivered a file without recording it: the orphan \
         finalizer would never know to reclaim it"
    );
    assert_eq!(std::fs::read(dir.join("v.en.srt")).unwrap(), b"subtitles");
    assert!(!staging.exists(), "staging cleaned");
    let mut progress = false;
    let mut merging = false;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            crate::engine_msg::EngineMsg::Progress { downloaded: 7, .. } => progress = true,
            crate::engine_msg::EngineMsg::Phase(p) if p.contains("Merging") => merging = true,
            _ => {}
        }
    }
    assert!(progress, "progress reported");
    assert!(merging, "merge phase announced");
    let logged = std::fs::read_to_string(dir.join("staging.argv.log")).unwrap();
    assert!(logged.contains("-f v123+a456/bv*+ba/b"), "{logged}");
    assert!(logged.contains("--merge-output-format mp4"), "{logged}");
    assert!(logged.contains("grab-media.%(ext)s"), "{logged}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_unified_leg_finishing_over_a_live_recording_leaves_it_alone() {
    // End-to-end proof for the wiring, not just the sweep helper: a VOD retry
    // shares the row's staging with the live path and used to end in
    // `remove_dir_all`, and the unplaceable recording is often the only copy.
    let dir = std::env::temp_dir().join(format!("grab-unified-keep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let recording = staging.join("final.1.mp4");
    std::fs::write(&recording, b"an unplaceable live recording").unwrap();
    std::fs::write(staging.join("final.1.mp4.lease"), b"").unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "v123+a456/bv*+ba/b",
        Some("mp4"),
        Some(7),
        &mut abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        matches!(res, Ok(Some(7))),
        "the VOD leg itself must succeed: {res:?}"
    );
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"unified");
    assert_eq!(
        std::fs::read(&recording).unwrap(),
        b"an unplaceable live recording",
        "a non-live leg finishing over a live recording destroyed it: the \
         row rerouted and the recording went with it"
    );
    assert!(recording.with_extension("mp4.lease").exists(), "lease lost");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unified_runner_surfaces_failure_tail() {
    // A 403-style failure surfaces yt-dlp's line and claims nothing.
    let dir = std::env::temp_dir().join(format!("grab-fakeunified-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_fail(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "v123+a456/bv*+ba/b",
        Some("mp4"),
        None,
        &mut abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert!(e.to_string().contains("403"), "tail surfaces: {e}"),
        ok => panic!("expected failure, got {ok:?}"),
    }
    assert!(!job.dest.exists(), "nothing claimed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unified_runner_abort_stays_quiet() {
    // Dropping the abort sender mid-spawn resolves Ok(None) — the
    // caller (pauser/canceller) owns the row state, so no error.
    let dir = std::env::temp_dir().join(format!("grab-fakeunified-abort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(&bin, "#!/bin/sh\nsleep 60\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let handle = std::thread::spawn(move || {
        crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
            &bin,
            std::path::Path::new("/usr/bin/ffmpeg"),
            &staging,
            &job,
            &AttemptGate::new(),
            "v123+a456/bv*+ba/b",
            Some("mp4"),
            None,
            &mut abort_rx,
            std::time::Duration::from_secs(30),
            tx,
        ))
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(abort_tx);
    let res = handle.join().expect("thread joins");
    assert!(matches!(res, Ok(None)), "abort is quiet, got {res:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unified_runner_refuses_existing_dest() {
    // Overwrite pre-flight at the claim: a finished file at dest fails the
    // row for requeue instead of clobbering it.
    let dir = std::env::temp_dir().join(format!("grab-fakeunified-ow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    std::fs::write(&job.dest, b"already").unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "v123+a456/bv*+ba/b",
        Some("mp4"),
        None,
        &mut abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert_eq!(e.to_string(), crate::engine_msg::DEST_EXISTS, "{e}"),
        ok => panic!("expected pre-flight refusal, got {ok:?}"),
    }
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"already");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unified_runner_rejects_empty_output() {
    // A zero-byte "completed" download must fail, not claim an empty
    // Done row: the old split path enforced the same contract.
    let dir = std::env::temp_dir().join(format!("grab-fakeunified-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-empty");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
: > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
        &bin,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "v123+a456/bv*+ba/b",
        Some("mp4"),
        None,
        &mut abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert!(e.to_string().contains("empty stream"), "got {e}"),
        ok => panic!("expected empty-stream failure, got {ok:?}"),
    }
    assert!(!job.dest.exists(), "nothing claimed");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── unified direct-download spec ─────────────────────────────────────

#[test]
fn unified_format_spec_matrix() {
    // Split: planner pair, then preset-video with the exact audio (so a lone
    // stale video id doesn't throw away good audio), then the full preset pair.
    assert_eq!(
        unified_format_spec(Some("v123"), "a456", "1080p", false),
        (
            "v123+a456/bv*[height<=1080]+a456/bv*[height<=1080]+ba/b".to_string(),
            true
        )
    );
    // Best quality (no height cap): unbounded video fallback.
    assert_eq!(
        unified_format_spec(Some("v1"), "a2", "best", false),
        ("v1+a2/bv*+a2/bv*+ba/b".to_string(), true)
    );
    // Adopted single file: exact id, best-single fallback (never audio).
    assert_eq!(
        unified_format_spec(None, "m789", "1080p", false),
        ("m789/b".to_string(), false)
    );
    // Dialog audio-only choice: exact audio track, audio fallback;
    // never merges, never drifts into video.
    assert_eq!(
        unified_format_spec(Some("v123"), "a456", "1080p", true),
        ("a456/ba/b".to_string(), false)
    );
    // Hostile ids degrade to preset chains instead of widening `-f`.
    assert_eq!(
        unified_format_spec(Some("v1/a2"), "a456", "1080p", false),
        ("bv*[height<=1080]+ba/b".to_string(), true)
    );
    assert_eq!(
        unified_format_spec(Some("v123"), "a[456]", "1080p", false),
        ("bv*[height<=1080]+ba/b".to_string(), true)
    );
    assert_eq!(
        unified_format_spec(Some(""), "a456", "1080p", false),
        ("bv*[height<=1080]+ba/b".to_string(), true)
    );
    assert_eq!(
        unified_format_spec(None, "a,456", "1080p", false),
        ("b".to_string(), false)
    );
}

#[test]
fn merge_output_ext_maps_supported_or_mp4() {
    assert_eq!(merge_output_ext("mp4"), "mp4");
    assert_eq!(merge_output_ext("webm"), "webm");
    assert_eq!(merge_output_ext("mkv"), "mkv");
    // Unsupported containers fall back to mp4 (a working file beats a
    // failed merge); matching is case-insensitive.
    assert_eq!(merge_output_ext("avi"), "mp4");
    assert_eq!(merge_output_ext("mov"), "mp4");
    assert_eq!(merge_output_ext("WEBM"), "webm");
    assert_eq!(merge_output_ext(""), "mp4");
}

#[test]
fn unified_candidate_names_claimable_output() {
    // Merge-fragment leftovers, `.part` shells, sidecars and metadata
    // droppings are never claimed, on either discovery path.
    assert!(unified_candidate("grab-media.mp4"));
    assert!(!unified_candidate("grab-media.f399.mp4"));
    assert!(!unified_candidate("grab-media.f251.webm"));
    assert!(!unified_candidate("grab-media.mp4.part"));
    assert!(!unified_candidate("grab-media.en.srt"));
    assert!(!unified_candidate("grab-media.ytdl"));
    assert!(!unified_candidate("grab-media.temp"));
    assert!(!unified_candidate("other.mp4"));
}

#[test]
fn is_ytdlp_fragment_names_merge_temps() {
    assert!(is_ytdlp_fragment("grab-media.f399.mp4"));
    assert!(is_ytdlp_fragment("grab-media.f251.webm"));
    assert!(!is_ytdlp_fragment("grab-media.mp4"));
    assert!(!is_ytdlp_fragment("grab-media.en.srt"));
    assert!(!is_ytdlp_fragment("grab-media.mp4.part"));
    assert!(!is_ytdlp_fragment("grab-media.f.mp4"));
    assert!(!is_ytdlp_fragment("grab-media.%(ext)s"));
}

#[test]
fn discover_unified_output_prefers_after_move_and_excludes() {
    // after_move wins when valid; otherwise the largest non-excluded
    // temp wins the scan, whatever else litters staging.
    let dir = std::env::temp_dir().join(format!("grab-discover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("grab-media.f399.mp4"), b"fragment").unwrap();
    std::fs::write(staging.join("grab-media.mp4.part"), b"partial").unwrap();
    std::fs::write(staging.join("grab-media.en.srt"), b"subs").unwrap();
    std::fs::write(staging.join("grab-media.mp4"), b"output12").unwrap();
    let out = staging.join("grab-media.mp4");
    assert_eq!(
        discover_unified_output(&staging, Some(out.to_str().unwrap())),
        Some(out.clone())
    );
    // after_move pointing outside staging (or at an excluded name)
    // falls back to the scan instead of claiming foreign bytes.
    assert_eq!(
        discover_unified_output(&staging, Some("/tmp/elsewhere.mp4")),
        Some(out.clone())
    );
    assert_eq!(
        discover_unified_output(
            &staging,
            Some(
                staging
                    .join("grab-media.en.srt")
                    .to_str()
                    .unwrap()
                    .to_string()
            )
            .as_deref()
        ),
        Some(out.clone())
    );
    // Nothing claimable at all: no output.
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(empty.join("grab-media.f1.mp4"), b"frag").unwrap();
    assert_eq!(discover_unified_output(&empty, None), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unified_argv_takes_subtitle_flags() {
    // The resolved subtitle language rides on the media leg: fetch flags plus
    // `--embed-subs` when the embed preference is on.
    let mut job = direct_test_job();
    job.subtitles = Some("en".into());
    job.embed_subs = true;
    let out = std::path::Path::new("/tmp/staging/grab-media.%(ext)s");
    let argv = unified_download_argv(
        &job,
        "v123+a456/bv*+a456/bv*+ba/b",
        true,
        "mp4",
        std::path::Path::new("/usr/bin/ffmpeg"),
        out,
    );
    for t in SUBTITLE_TOKENS {
        assert!(argv.iter().any(|a| a == *t), "{t} missing: {argv:?}");
    }
    assert!(
        argv.iter().any(|a| a == "--embed-subs"),
        "--embed-subs missing: {argv:?}"
    );
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert_eq!(argv[argv.len() - 1], "https://x.com/u/status/1");
    assert!(sep < argv.len() - 1, "{argv:?}");
}

/// Fake yt-dlp emitting two format legs like a real merged download: video
/// 0→100 + `finished`, audio 0→50 + `finished`, then merge line and after_move.
fn fake_ytdlp_two_legs(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-two-legs");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
echo "[Grab];downloading;0;100;100;1000;5"
echo "[Grab];downloading;100;100;100;1000;0"
echo "[Grab];finished;100;100;100;0;0"
echo "[Grab];downloading;0;50;50;1000;5"
echo "[Grab];downloading;50;50;50;1000;0"
echo "[Grab];finished;50;50;50;0;0"
echo "[Merger] Merging formats"
echo "$out"
printf 'twolegs' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn unified_runner_sums_two_leg_progress() {
    // `downloaded_bytes` resets per leg; banking each on its `finished` line
    // must report the SUM (100+50) — plain max would stall the bar at ~66%.
    let dir = std::env::temp_dir().join(format!("grab-faketwolegs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_two_legs(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &AttemptGate::new(),
        "v1+a2/bv*+a2/bv*+ba/b",
        Some("mp4"),
        Some(150),
        &mut abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    let mut max_seen = 0u64;
    while let Ok(msg) = rx.try_recv() {
        if let crate::engine_msg::EngineMsg::Progress { downloaded, .. } = msg {
            max_seen = max_seen.max(downloaded);
        }
    }
    assert_eq!(max_seen, 150, "progress must sum both legs");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn has_fetchable_media_matrix() {
    // Dialog probe criterion: mirror the worker's acceptance (plain
    // HTTPS, URL present, no DRM) without container specifics.
    let yes = test_video(serde_json::json!([test_format_full(
        "v",
        "avc1.640028",
        "none",
        Some(720),
        None,
        "https",
        false
    ),]));
    assert!(has_fetchable_media(&yes));
    // Empty extraction: plain file fallback.
    let none = test_video(serde_json::json!([]));
    assert!(!has_fetchable_media(&none));
    // DRM-only page: nothing fetchable. Explicit no-DRM behaves
    // like absent (the worker treats both as fetchable).
    let drm = test_video(serde_json::json!([test_format_full(
        "d",
        "avc1.640028",
        "none",
        Some(720),
        None,
        "https",
        true
    ),]));
    assert!(!has_fetchable_media(&drm));
    // Non-manifest transports (DASH segments, RTMP) are not fetchable.
    let dash = test_video(serde_json::json!([
        {
            "format": "d1",
            "format_id": "d1",
            "protocol": "http_dash_segments",
            "ext": "mp4",
            "url": "https://cdn.example/d1.mp4",
            "vcodec": "avc1.640028",
            "acodec": "none",
            "http_headers": {},
        },
    ]));
    assert!(!has_fetchable_media(&dash));
    // Explicit no-DRM: same as absent, per the DRM case above.
    let nodrm = test_video(serde_json::json!([
        {
            "format": "v",
            "format_id": "v",
            "protocol": "https",
            "ext": "mp4",
            "url": "https://cdn.example/v.mp4",
            "vcodec": "avc1.640028",
            "acodec": "none",
            "has_drm": false,
            "http_headers": {},
        },
    ]));
    assert!(has_fetchable_media(&nodrm));
    // HLS-manifest page: the worker pulls variants itself.
    let hls = test_video(serde_json::json!([test_format_full(
        "h",
        "avc1",
        "mp4a.40.2",
        Some(720),
        None,
        "m3u8_native",
        false
    ),]));
    assert!(has_fetchable_media(&hls));
}

#[test]
fn is_http_url_matrix() {
    assert!(is_http_url("https://example.com/f.iso"));
    assert!(is_http_url("http://example.com/v"));
    assert!(!is_http_url("magnet:?xt=urn:btih:abc"));
    assert!(!is_http_url("file:///tmp/x.mp4"));
    assert!(!is_http_url("not a url"));
    assert!(!is_http_url(""));
}

#[test]
fn is_direct_file_url_matrix() {
    // Obvious files skip the probe: archives, installers, documents,
    // and direct media (the plain engine handles those better anyway).
    for url in [
        "https://example.com/f.iso",
        "https://example.com/a.ZIP",
        "https://example.com/v.mp4?download=1",
        "https://example.com/song.mp3#t=10",
        "https://example.com/archive.tar.gz",
        "https://example.com/x.torrent",
        "https://example.com/setup.pkg",
        "http://example.com/doc.pdf",
        "https://example.com/com.brave.Origin.flatpakref",
        "https://example.com/app.AppImage",
        "https://example.com/photo.heic",
        "https://example.com/book.epub",
        "https://example.com/data.sqlite",
    ] {
        assert!(is_direct_file_url(url), "{url}");
    }
    // Pages, scripts, streams, feeds and odd schemes always probe. Extensionless
    // terminals are not extensions: `/md` must probe like any page.
    for url in [
        "https://www.youtube.com/watch?v=x",
        "https://example.com/article",
        "https://example.com/",
        "https://example.com",
        "https://example.com/download",
        "https://example.com/md",
        "https://example.com/raw",
        "https://example.com/page.html",
        "https://example.com/app.php",
        "https://example.com/feed.xml",
        "https://example.com/stream.m3u",
        "https://example.com/stream.m3u8",
        "https://example.com/list.pls",
        "https://example.com/seg.ts",
        "magnet:?xt=urn:btih:abc",
        "file:///tmp/x.mp4",
        "not a url",
        "",
    ] {
        assert!(!is_direct_file_url(url), "{url}");
    }
}

#[test]
fn direct_file_exts_stay_sorted() {
    // `binary_search` requires it; this fails the edit that appends
    // out of order instead of the user whose probe misroutes.
    let mut sorted = DIRECT_FILE_EXTS.to_vec();
    sorted.sort_unstable();
    if sorted != DIRECT_FILE_EXTS {
        let at = sorted
            .iter()
            .zip(DIRECT_FILE_EXTS.iter())
            .position(|(a, b)| a != b);
        panic!("DIRECT_FILE_EXTS out of order at {at:?}");
    }
}

#[test]
fn a_discard_before_the_commit_delivers_nothing() {
    // The live leg must consult the gate, not just the stop signal: the signal
    // is only a prompt, and a prompt can arrive after the worker stops looking.
    let dir = std::env::temp_dir().join(format!("grab-gate-discard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    assert!(
        gate.discard(),
        "the removal claims the row before the worker starts"
    );

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &gate,
        "https://youtu.be/grabtest",
        "h1080",
        stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        matches!(res, Ok(None)),
        "a discarded attempt must not deliver: {res:?}"
    );
    assert!(!dest.exists(), "a discarded capture was delivered");
    assert!(
        !gate.was_delivered(),
        "the gate must not record a delivery that never happened"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_commit_before_the_discard_delivers_and_is_recorded() {
    // The mirror: when the commit wins, the file *is* placed, and the gate
    // says so — which is what tells the finalizer there is an orphan.
    let dir = std::env::temp_dir().join(format!("grab-gate-commit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &gate,
        "https://youtu.be/grabtest",
        "h1080",
        stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        matches!(res, Ok(Some(_))),
        "an un-discarded attempt must deliver: {res:?}"
    );
    assert!(dest.exists(), "nothing was placed");
    assert!(gate.was_delivered(), "the gate must record the delivery");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_unified_leg_honours_a_discard_before_its_commit() {
    let dir = std::env::temp_dir().join(format!("grab-gate-unified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    assert!(gate.discard());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &gate,
        "v123+a456/bv*+ba/b",
        Some("mp4"),
        Some(7),
        &mut stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        matches!(res, Ok(None)),
        "a discarded VOD attempt must not deliver: {res:?}"
    );
    assert!(!dest.exists(), "a discarded VOD attempt was delivered");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_hls_leg_honours_a_discard_before_its_commit() {
    let dir = std::env::temp_dir().join(format!("grab-gate-hls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mkv");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    assert!(gate.discard());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &gate,
        "h1080",
        stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        matches!(res, Ok(None)),
        "a discarded HLS attempt must not deliver: {res:?}"
    );
    assert!(!dest.exists(), "a discarded HLS attempt was delivered");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_closed_stop_receiver_stops_delivery_rather_than_authorising_it() {
    // `Empty` means "no stop yet" and carrying on is right. `Closed` means
    // no sender remains to authorise anything, so it must fail closed.
    let dir = std::env::temp_dir().join(format!("grab-gate-closed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    // Sender dropped immediately: the receiver observes Closed.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<StopIntent>();
    drop(stop_tx);
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &gate,
        "https://youtu.be/grabtest",
        "h1080",
        stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        matches!(res, Ok(None)),
        "a closed receiver must stop quietly, not deliver and not fail: {res:?}"
    );
    assert!(
        !gate.was_delivered(),
        "a closed receiver authorised a delivery: nothing was left to permit it"
    );
    assert!(!dest.exists(), "a closed receiver delivered a file");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn a_group_that_refuses_to_quiesce_is_reported_rather_than_assumed() {
    // A descendant that ignores its group kill must be *reported*, not treated
    // as gone: the caller sweeps anyway, but the outcome stays observable.
    use std::os::unix::process::CommandExt as _;
    let dir = std::env::temp_dir().join(format!("grab-quiesce-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("still-writing");
    // A process in its own group that writes, then sleeps well past the wait.
    let script = dir.join("stubborn");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'x' >> '{}'\nsleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut child = std::process::Command::new(&script)
        .process_group(0)
        .spawn()
        .expect("stubborn descendant");
    let pgid = child.id() as i32;
    std::thread::sleep(std::time::Duration::from_millis(100));

    let quiesced = crate::runtime::tokio_rt().block_on(await_group_quiescence(
        pgid,
        std::time::Duration::from_millis(300),
    ));
    assert!(
        !quiesced,
        "a live process group was reported as quiesced, so a reclaim would run \
         under a writer that never stopped"
    );
    // Kill the whole group, not just the direct child: the script's
    // `sleep 30` grandchild shares the group and would leak.
    // SAFETY: constant signal number; ESRCH (already dead) is harmless.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
