use super::*;
use pretty_assertions::assert_eq;

const MAGNET: &str = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=test+file";
const BARE_HASH: &str = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3";

#[test]
fn classifies_magnets() {
    assert!(is_magnet("magnet:?xt=urn:btih:abc"));
    assert!(is_magnet("MAGNET:?xt=urn:btih:abc"));
    assert!(is_magnet("  magnet:?xt=urn:btih:abc"));
    assert!(!is_magnet("https://example.com/f.iso"));
    assert!(!is_magnet("magnetfoo"));
    assert!(!is_magnet(""));
}

#[test]
fn parses_valid_magnets() {
    let m = parse_magnet(MAGNET).unwrap();
    assert_eq!(m.name.as_deref(), Some("test file"));
    assert!(parse_magnet(BARE_HASH).is_ok());
    assert!(parse_magnet("magnet:?xt=urn:btih:xyz").is_err());
    assert!(parse_magnet("https://example.com/f.iso").is_err());
    assert!(parse_magnet("").is_err());
}

#[test]
fn classifier_never_panics_on_unicode() {
    assert!(!is_magnet("éééééééééé"));
    assert!(!is_magnet("é"));
    assert!(!is_magnet("magnet"));
}

#[test]
fn stub_prefers_name_then_hash() {
    assert_eq!(stub_name(MAGNET).as_deref(), Some("test file"));
    assert_eq!(
        stub_name(BARE_HASH).as_deref(),
        Some("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3")
    );
    assert_eq!(stub_name("magnet:?xt=urn:btih:xyz"), None);
}

#[test]
fn drop_selection_clears_finished_filter() {
    let url = "torrent:/tmp/grab-drop-test.torrent";
    stage_selection(url, vec![0, 2]);
    assert_eq!(get_selection(url), Some(vec![0, 2]));
    drop_selection(url);
    assert_eq!(get_selection(url), None);
    // Dropping a missing key is a no-op, never a panic.
    drop_selection(url);
}

#[test]
fn multifile_torrents_get_name_subfolder() {
    let dest = std::path::PathBuf::from("/tmp/dl");
    // Multi-file torrents land in a subfolder named after the torrent…
    let out = output_folder_for(&dest, Some("Cosmos Laundromat".to_string()), true, "abc123");
    assert_eq!(out, std::path::PathBuf::from("/tmp/dl/Cosmos Laundromat"));
    // …single-file torrents sit flat even when a name is known…
    assert_eq!(
        output_folder_for(&dest, Some("movie.mp4".to_string()), false, "abc123"),
        dest
    );
    // …and hostile names fall back to the info-hash, never the parent.
    let out = output_folder_for(&dest, Some("../evil".to_string()), true, "abc123");
    assert_eq!(out, std::path::PathBuf::from("/tmp/dl/abc123"));
    assert!(out.starts_with(&dest));
    assert_eq!(
        output_folder_for(&dest, None, true, "abc123"),
        std::path::PathBuf::from("/tmp/dl/abc123")
    );
}

#[test]
fn intake_plan_mirrors_engine_layout() {
    fn torrent_bytes(info: &str) -> Vec<u8> {
        // `info` leaves both dicts open; the two `e` close info then outer.
        format!("d8:announce32:https://tracker.example.com:80/a4:info{info}ee").into_bytes()
    }
    // Single file, sane name: flat layout.
    let single = torrent_bytes(
        "d6:lengthi5e4:name9:movie.mp412:piece lengthi16384e6:pieces20:01234567890123456789",
    );
    assert_eq!(intake_plan(&single), Some(("movie.mp4".to_string(), false)));
    // Multi-file: folder base is the torrent name.
    let multi = torrent_bytes(
        "d5:filesld6:lengthi5e4:pathl5:a.txteed6:lengthi5e4:pathl5:b.txteee4:name4:pack12:piece lengthi16384e6:pieces20:01234567890123456789",
    );
    assert_eq!(intake_plan(&multi), Some(("pack".to_string(), true)));
    // Hostile name: info-hash hex fallback, never a path.
    let evil = torrent_bytes("d6:lengthi0e4:name7:../evil12:piece lengthi16384e6:pieces0:");
    let (base, multi) = intake_plan(&evil).expect("parses");
    assert!(!multi);
    assert_eq!(base.len(), 40);
    assert!(base.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!base.contains('/'));
    // Garbage is not a torrent.
    assert_eq!(intake_plan(b"not a torrent"), None);
}

#[test]
fn blocklist_url_empty_disables() {
    assert_eq!(blocklist_url_of(""), Ok(None));
    assert_eq!(blocklist_url_of("   "), Ok(None));
}

#[test]
fn blocklist_url_accepts_http_schemes() {
    assert_eq!(
        blocklist_url_of("https://example.com/ipfilter.dat"),
        Ok(Some("https://example.com/ipfilter.dat".to_string()))
    );
    // Scheme match is case-insensitive; surrounding whitespace trims.
    assert_eq!(
        blocklist_url_of("  HTTP://example.com/list.txt "),
        Ok(Some("HTTP://example.com/list.txt".to_string()))
    );
}

#[test]
fn blocklist_url_rejects_non_http() {
    assert!(blocklist_url_of("ftp://example.com/list.txt").is_err());
    assert!(blocklist_url_of("example.com/list.txt").is_err());
    assert!(blocklist_url_of("not a url").is_err());
}

fn manual_proxy(ptype: &str) -> Option<crate::net_types::ResolvedProxy> {
    crate::download::DownloadOptions {
        limit_rate: String::new(),
        connections: 4,
        proxy_mode: "manual".into(),
        proxy_type: ptype.into(),
        proxy_host: "127.0.0.1".into(),
        proxy_port: 9050,
        cookies_browser: String::new(),
    }
    .proxy_config()
    .expect("well-formed manual proxy")
}

#[test]
fn torrent_socks_url_accepts_only_socks() {
    // socks5h normalizes to the exact scheme librqbit demands.
    let socks = manual_proxy("socks5").expect("proxied");
    assert_eq!(
        socks.torrent_socks_url().as_deref(),
        Some("socks5://127.0.0.1:9050")
    );
    // HTTP(S) proxies have no engine peer path: direct, never fail.
    let http = manual_proxy("http").expect("proxied");
    assert_eq!(http.torrent_socks_url(), None);
}

#[test]
fn torrent_net_plan_direct_passthrough() {
    let trackers = Some(vec![
        "udp://tracker.example:80".to_string(),
        "https://tracker.example/announce".to_string(),
    ]);
    let plan = plan_torrent_net(true, true, 6881, true, trackers.clone(), None);
    assert!(plan.dht);
    assert!(plan.lsd);
    assert_eq!(plan.listen_port, 6881);
    assert!(plan.upnp);
    assert_eq!(plan.trackers, trackers);
    assert_eq!(plan.socks_proxy, None);
}

#[test]
fn torrent_net_plan_upnp_defaults_off() {
    // UPnP port forwarding is no longer user-configurable; the plan always leaves it off.
    let plan = plan_torrent_net(true, true, 6881, false, None, None);
    assert!(plan.dht);
    assert_eq!(plan.listen_port, 6881);
    assert!(!plan.upnp);
    assert_eq!(plan.socks_proxy, None);
}

#[test]
fn torrent_net_plan_socks_darkens_unproxyable() {
    let socks = manual_proxy("socks5");
    let plan = plan_torrent_net(
        true,
        true,
        6881,
        true,
        Some(vec![
            "udp://tracker.example:80".to_string(),
            "https://tracker.example/announce".to_string(),
        ]),
        socks.as_ref(),
    );
    // DHT, LSD, listener and UDP trackers would leak around the tunnel.
    assert!(!plan.dht);
    assert!(!plan.lsd);
    assert_eq!(plan.listen_port, 0);
    assert!(!plan.upnp);
    assert_eq!(
        plan.trackers,
        Some(vec!["https://tracker.example/announce".to_string()])
    );
    assert_eq!(plan.socks_proxy.as_deref(), Some("socks5://127.0.0.1:9050"));
}

#[test]
fn torrent_net_plan_socks_all_udp_trackers_means_none() {
    let socks = manual_proxy("socks5");
    let plan = plan_torrent_net(
        true,
        true,
        6881,
        true,
        Some(vec!["udp://tracker.example:80".to_string()]),
        socks.as_ref(),
    );
    assert_eq!(plan.trackers, None);
    assert!(plan.socks_proxy.is_some());
}

#[test]
fn torrent_net_plan_http_proxy_stays_direct() {
    let http = manual_proxy("http");
    let plan = plan_torrent_net(true, true, 6881, true, None, http.as_ref());
    assert!(plan.dht);
    assert_eq!(plan.listen_port, 6881);
    assert!(plan.upnp);
    assert_eq!(plan.socks_proxy, None);
}

#[test]
fn stub_name_neutralizes_traversal() {
    // Attacker-controlled stems collapse to the final component; unsafe ones fall back to "torrent".
    assert_eq!(stub_name_for_file("../../evil.torrent"), "evil");
    assert_eq!(stub_name_for_file("subdir/name.torrent"), "name");
    assert_eq!(stub_name_for_file("..."), "torrent");
}

#[test]
fn output_folder_for_gates_hostile_names() {
    // Folder names come from torrent metadata: traversal/absolute fall back to info-hash hex.
    let dest = std::path::Path::new("/tmp/dl");
    assert_eq!(
        output_folder_for(dest, Some("../evil".into()), true, "abc123"),
        dest.join("abc123")
    );
    assert_eq!(
        output_folder_for(dest, Some("/abs".into()), true, "abc123"),
        dest.join("abc123")
    );
    assert_eq!(
        output_folder_for(dest, Some("Show.S01".into()), true, "abc123"),
        dest.join("Show.S01")
    );
    // Single-file torrents always sit flat.
    assert_eq!(
        output_folder_for(dest, Some("../evil".into()), false, "abc123"),
        dest.to_path_buf()
    );
}

#[test]
fn sweep_archives_keeps_referenced_only() {
    use std::collections::HashSet;
    let dir = std::env::temp_dir().join(format!("grab-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let kept = dir.join("a.torrent");
    let stale = dir.join("b.torrent");
    let other = dir.join("notes.txt");
    std::fs::write(&kept, b"a").unwrap();
    std::fs::write(&stale, b"b").unwrap();
    std::fs::write(&other, b"c").unwrap();
    let referenced: HashSet<String> = [format!("torrent:{}", kept.to_string_lossy())]
        .into_iter()
        .collect();
    sweep_archives_in(&dir, &referenced);
    assert!(kept.exists());
    assert!(!stale.exists());
    assert!(other.exists(), "non-torrent files are never swept");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_path_strips_spoof_chars() {
    assert_eq!(
        sanitize_display_path("dir/<b>file.iso</b>"),
        "dir/<b>file.iso</b>"
    );
    assert_eq!(sanitize_display_path("a\u{202E}exe.pdf"), "aexe.pdf");
    assert_eq!(sanitize_display_path("a\nb\tc"), "abc");
    assert_eq!(sanitize_display_path("v\u{2066}ideo"), "video");
    // U+200E/U+200F (LTR/RTL marks) were missing from the old local predicate;
    // the shared is_bidi_control must strip them too.
    assert_eq!(sanitize_display_path("a\u{200E}b\u{200F}c"), "abc");
    let long = "x".repeat(200);
    let shown = sanitize_display_path(&long);
    assert_eq!(shown.chars().count(), 121);
    assert!(shown.ends_with('…'));
    assert_eq!(sanitize_display_path(""), "");
}

#[test]
fn info_hash_for_url_resolves_magnets() {
    assert_eq!(
        info_hash_for_url(MAGNET).as_deref(),
        Some("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3")
    );
    assert_eq!(
        info_hash_for_url(BARE_HASH).as_deref(),
        Some("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3")
    );
    // Unresolvable inputs yield None, never a panic: the sweep skips them.
    assert_eq!(info_hash_for_url("magnet:?xt=urn:btih:xyz"), None);
    assert_eq!(info_hash_for_url("https://example.com/f.iso"), None);
    assert_eq!(info_hash_for_url("torrent:/no/such/file.torrent"), None);
}

#[test]
fn sweep_session_orphans_without_session_is_noop() {
    // No engine in tests: must return without creating a session as a side effect.
    let keep = std::collections::HashSet::new();
    crate::runtime::tokio_rt().block_on(sweep_session_orphans(&keep));
    assert!(session_handle().is_none());
}

#[test]
fn bps_maps_limits() {
    // Empty and zero mean unlimited; values beyond u32 stay unlimited instead of truncating.
    assert_eq!(bps(None), None);
    assert_eq!(bps(Some(0)), None);
    assert_eq!(bps(Some(500_000)), NonZeroU32::new(500_000));
    assert_eq!(bps(Some(u64::from(u32::MAX) + 1)), None);
}

#[test]
fn seed_limits_hit_matrix() {
    // Ratio rule only: uploaded past ratio × total finishes.
    let t0 = std::time::Instant::now();
    assert!(seed_limits_hit(2.0, 0, t0, 1000, 2000));
    assert!(!seed_limits_hit(2.0, 0, t0, 1000, 1999));
    // Disabled ratio (0.0) never fires, even fully seeded.
    assert!(!seed_limits_hit(0.0, 0, t0, 1000, 100000));
    // Zero total never fires the ratio rule (no division by zero).
    assert!(!seed_limits_hit(2.0, 0, t0, 0, 2000));
    // Time rule only: elapsed past the limit finishes.
    let old = t0 - std::time::Duration::from_secs(61 * 60);
    assert!(seed_limits_hit(0.0, 60, old, 1000, 0));
    assert!(!seed_limits_hit(0.0, 60, t0, 1000, 0));
}

#[test]
fn file_list_keeps_raw_path_beside_display_path() {
    // One short name, one past the 120-char display cap: display truncates, raw must not.
    let long = "x".repeat(200);
    // Info-dict keys sorted, as in the other synthetic torrents in this file.
    let mut b = b"d8:announce7:x-local4:infod5:filesl".to_vec();
    b.extend_from_slice(b"d6:lengthi2e4:pathl8:keep.isoee");
    b.extend_from_slice(format!("d6:lengthi3e4:pathl{}:{}ee", long.len(), long).as_bytes());
    b.extend_from_slice(b"e4:name4:test12:piece lengthi16384e6:pieces0:ee");
    let (_name, entries) = torrent_file_list(&b).expect("synthetic torrent must parse");
    assert_eq!(entries.len(), 2);
    let short = entries
        .iter()
        .find(|e| e.raw_path == "keep.iso")
        .expect("short entry");
    assert_eq!(short.display_path, "keep.iso");
    let long_entry = entries
        .iter()
        .find(|e| e.raw_path == long)
        .expect("long entry keeps its raw path");
    assert_eq!(long_entry.display_path.chars().count(), 121);
    assert!(long_entry.display_path.ends_with('…'));
    assert_eq!(long_entry.raw_path.chars().count(), 200);
}

#[test]
fn raw_path_collapses_distinct_invalid_utf8() {
    // `raw_path` is lossy UTF-8, not raw bytes: names differing only in invalid
    // UTF-8 collapse. Pins the known limitation documented on `TorrentFileEntry`.
    let mut b = b"d8:announce7:x-local4:infod5:filesl".to_vec();
    b.extend_from_slice(b"d6:lengthi2e4:pathl2:");
    b.extend_from_slice(b"a\xff");
    b.extend_from_slice(b"eed6:lengthi2e4:pathl2:");
    b.extend_from_slice(b"a\xfe");
    b.extend_from_slice(b"eee4:name4:test12:piece lengthi16384e6:pieces0:ee");
    let (_name, entries) = torrent_file_list(&b).expect("synthetic torrent must parse");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].raw_path, "a\u{fffd}");
    assert_eq!(entries[0].raw_path, entries[1].raw_path);
}

#[test]
fn deletion_target_uses_raw_path_not_display() {
    let folder = std::path::Path::new("/tmp/dl/Cosmos");
    let long = "x".repeat(200);
    let entry = TorrentFileEntry {
        raw_path: long.clone(),
        display_path: format!("{}…", "x".repeat(120)),
        length: 3,
    };
    // Target is the raw on-disk name: the truncated display path addresses nothing.
    assert_eq!(
        deletion_target(folder, 1, &std::collections::HashSet::new(), &entry),
        Some(folder.join(&long))
    );
}

#[test]
fn deletion_target_skips_kept_and_hostile_entries() {
    let folder = std::path::Path::new("/tmp/dl/Cosmos");
    let entry = TorrentFileEntry {
        raw_path: "keep.iso".to_string(),
        display_path: "keep.iso".to_string(),
        length: 2,
    };
    let mut keep = std::collections::HashSet::new();
    keep.insert(0);
    assert_eq!(deletion_target(folder, 0, &keep, &entry), None);
    let hostile = TorrentFileEntry {
        raw_path: "../evil.iso".to_string(),
        display_path: "../evil.iso".to_string(),
        length: 2,
    };
    assert_eq!(
        deletion_target(folder, 1, &std::collections::HashSet::new(), &hostile),
        None
    );
}

#[test]
fn read_torrent_bytes_enforces_single_bounded_open() {
    let dir = std::env::temp_dir().join(format!("grab-torrent-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let small = dir.join("small.torrent");
    std::fs::write(&small, b"d4:infod4:name4:testee").unwrap();
    assert_eq!(
        read_torrent_bytes(&small),
        Some(b"d4:infod4:name4:testee".to_vec())
    );
    // Over the cap: rejected, never read unbounded.
    let big = dir.join("big.torrent");
    let f = std::fs::File::create(&big).unwrap();
    f.set_len(MAX_TORRENT_BYTES + 1).unwrap();
    assert_eq!(read_torrent_bytes(&big), None);
    assert_eq!(read_torrent_bytes(&dir.join("missing.torrent")), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_archive_bytes_rejects_unresolvable_urls() {
    // Not a pseudo-URL, relative, or absolute path outside the archive dir: unresolvable.
    assert_eq!(read_archive_bytes("magnet:?xt=urn:btih:abc"), None);
    assert_eq!(read_archive_bytes("torrent:relative.torrent"), None);
    assert_eq!(read_archive_bytes("torrent:/tmp/evil.torrent"), None);
}

#[cfg(unix)]
#[test]
fn guard_output_folder_dedupes_planted_symlink() {
    let base = std::env::temp_dir().join(format!("grab-torrent-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let target =
        std::env::temp_dir().join(format!("grab-torrent-guard-target-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, base.join("Big Torrent")).unwrap();

    let guarded = guard_output_folder(&base, base.join("Big Torrent"));
    // Deduped past the squatter, never through it.
    assert_ne!(guarded, base.join("Big Torrent"));
    assert!(guarded.is_dir() && !guarded.is_symlink());
    // Nothing was written through the link.
    assert!(std::fs::read_dir(&target).unwrap().next().is_none());

    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&target);
}

#[test]
fn guard_output_folder_reuses_real_dir() {
    let base = std::env::temp_dir().join(format!("grab-torrent-reuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let existing = base.join("Big Torrent");
    std::fs::create_dir_all(&existing).unwrap();

    assert_eq!(guard_output_folder(&base, existing.clone()), existing);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn guard_output_folder_leaves_flat_dest_alone() {
    let base = std::env::temp_dir().join(format!("grab-torrent-flat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    // The dest dir itself (flat single-file download): no leaf to guard.
    assert_eq!(guard_output_folder(&base, base.clone()), base);
    let _ = std::fs::remove_dir_all(&base);
}
