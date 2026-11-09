//! Persistent cache for complete, static yt-dlp playlist metadata.
//!
//! The upstream yt_dlp::cache::PlaylistCache supplies Moka L1 and Redb L2.
//! Grab supplies the authentication-scoped key and adapts the crate's playlist
//! model to the smaller picker model used by the application.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;
use yt_dlp::cache::{CacheConfig, PlaylistCache};
use yt_dlp::model::playlist::{Playlist, PlaylistEntry};

use crate::media_types::{PlaylistInfo, PlaylistKind};

const PLAYLIST_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CLEAN_INTERVAL: Duration = Duration::from_secs(5 * 60);
const CACHE_KEY_PREFIX: &str = "grab-ytdlp-playlist-v1:";

static PLAYLIST_CACHE: OnceCell<PlaylistCache> = OnceCell::const_new();
static LAST_CLEAN_AT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CachedPlaylistKind {
    Playlist,
}

#[derive(Debug, Deserialize, Serialize)]
struct CacheMetadata {
    version: u8,
    kind: CachedPlaylistKind,
}

/// Derive a playlist-specific namespace from Grab's authentication-aware
/// extraction key. The crate's Redb backend indexes by both URL and playlist
/// ID, so the namespaced ID must be the same for both layers.
pub(crate) fn cache_key(extraction_key: &str) -> String {
    let digest = extraction_key
        .strip_prefix("grab-ytdlp-video-v1:")
        .unwrap_or(extraction_key);
    format!("{CACHE_KEY_PREFIX}{digest}")
}

/// Return a cached, complete playlist. Invalid or incompatible entries are
/// invalidated and treated as misses; cache failures never block extraction.
pub(crate) async fn lookup(key: &str) -> Option<PlaylistInfo> {
    let cache = playlist_cache().await?;
    clean_expired_if_due(cache).await;

    match cache.get(key).await {
        Ok(Some(playlist)) => match from_cached_playlist(key, playlist) {
            Some(info) => {
                tracing::debug!(item_count = info.items.len(), "yt-dlp playlist cache hit");
                Some(info)
            }
            None => {
                if let Err(error) = cache.invalidate(key).await {
                    tracing::warn!(error = %error, "could not invalidate incompatible yt-dlp playlist cache entry");
                }
                None
            }
        },
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(error = %error, "yt-dlp playlist cache read failed; probing normally");
            None
        }
    }
}

/// Store only complete, non-empty ordinary playlists. Story trays and
/// highlights are time-sensitive collections and are always resolved fresh.
pub(crate) async fn store(key: &str, info: &PlaylistInfo) {
    let Some(playlist) = to_cached_playlist(key, info) else {
        return;
    };
    let Some(cache) = playlist_cache().await else {
        return;
    };

    if let Err(error) = cache.put(key.to_string(), playlist).await {
        tracing::warn!(error = %error, "yt-dlp playlist cache write failed");
        return;
    }
    clean_expired_if_due(cache).await;
}

fn to_cached_playlist(key: &str, info: &PlaylistInfo) -> Option<Playlist> {
    if info.kind != PlaylistKind::Playlist
        || info.total == 0
        || info.total >= crate::video_probe::MAX_PLAYLIST_ITEMS
        || info.items.len() != info.total
    {
        return None;
    }

    let Ok(page_url) = url::Url::parse(&info.page_url) else {
        return None;
    };
    if !matches!(page_url.scheme(), "http" | "https")
        || crate::video_cache::has_rotating_auth_query(&page_url)
        || info.title.trim().is_empty()
        || info.items.iter().any(|item| {
            let Ok(url) = url::Url::parse(&item.page_url) else {
                return true;
            };
            !matches!(url.scheme(), "http" | "https")
                || item.id.is_empty()
                || item.title.trim().is_empty()
                || crate::video_cache::has_rotating_auth_query(&url)
        })
    {
        return None;
    }

    let metadata = CacheMetadata {
        version: 1,
        kind: CachedPlaylistKind::Playlist,
    };
    let description = serde_json::to_string(&metadata).ok()?;
    let entries = info
        .items
        .iter()
        .map(|item| PlaylistEntry {
            id: item.id.to_string(),
            title: item.title.to_string(),
            url: item.page_url.to_string(),
            index: Some(item.playlist_index as usize),
            duration: None,
            thumbnail: None,
            uploader: None,
            channel_id: None,
            availability: None,
        })
        .collect();

    Some(Playlist {
        id: key.to_string(),
        title: info.title.to_string(),
        description: Some(description),
        uploader: None,
        uploader_id: None,
        uploader_url: None,
        entries,
        video_count: Some(info.total),
        url: Some(info.page_url.to_string()),
    })
}

fn from_cached_playlist(key: &str, playlist: Playlist) -> Option<PlaylistInfo> {
    if playlist.id != key {
        return None;
    }

    let metadata: CacheMetadata = serde_json::from_str(playlist.description.as_deref()?).ok()?;
    if metadata.version != 1 || !matches!(metadata.kind, CachedPlaylistKind::Playlist) {
        return None;
    }

    let page_url = playlist.url?;
    let Ok(parsed_page_url) = url::Url::parse(&page_url) else {
        return None;
    };
    if !matches!(parsed_page_url.scheme(), "http" | "https")
        || crate::video_cache::has_rotating_auth_query(&parsed_page_url)
        || playlist.title.trim().is_empty()
        || playlist.entries.is_empty()
        || playlist.entries.len() >= crate::video_probe::MAX_PLAYLIST_ITEMS
    {
        return None;
    }

    let total = playlist.video_count.unwrap_or(playlist.entries.len());
    if total != playlist.entries.len() {
        return None;
    }

    let items = playlist
        .entries
        .into_iter()
        .enumerate()
        .map(|(position, entry)| {
            let parsed_url = url::Url::parse(&entry.url).ok()?;
            if entry.id.is_empty()
                || entry.title.trim().is_empty()
                || !matches!(parsed_url.scheme(), "http" | "https")
                || crate::video_cache::has_rotating_auth_query(&parsed_url)
            {
                return None;
            }
            let playlist_index = entry
                .index
                .and_then(|index| u32::try_from(index).ok())
                .filter(|index| *index > 0)
                .unwrap_or_else(|| {
                    (position.saturating_add(1)).min(u32::MAX as usize) as u32
                });
            Some(crate::media_types::PlaylistItem {
                id: entry.id.into_boxed_str(),
                title: entry.title.into_boxed_str(),
                page_url: entry.url.into_boxed_str(),
                playlist_index,
            })
        })
        .collect::<Option<Vec<_>>>()?;

    Some(PlaylistInfo {
        title: playlist.title.into_boxed_str(),
        page_url: page_url.into_boxed_str(),
        kind: PlaylistKind::Playlist,
        total,
        items: items.into_boxed_slice(),
    })
}

async fn playlist_cache() -> Option<&'static PlaylistCache> {
    match PLAYLIST_CACHE
        .get_or_try_init(|| async {
            let cache_dir =
                crate::video_cache::secure_cache_directory().map_err(|error| error.to_string())?;
            let database = cache_dir.join("playlists.redb");
            crate::video_cache::validate_cache_file_path(&database)
                .map_err(|error| error.to_string())?;
            let config = CacheConfig::builder().cache_dir(cache_dir).build();
            let cache = PlaylistCache::new(&config, Some(PLAYLIST_CACHE_TTL.as_secs()))
                .await
                .map_err(|error| error.to_string())?;
            crate::video_cache::secure_cache_file(&database).map_err(|error| error.to_string())?;
            Ok::<PlaylistCache, String>(cache)
        })
        .await
    {
        Ok(cache) => Some(cache),
        Err(error) => {
            tracing::warn!(error = %error, "yt-dlp playlist cache unavailable; probing normally");
            None
        }
    }
}

/// Redb expiry is lazy; periodically reclaim expired playlist entries.
async fn clean_expired_if_due(cache: &PlaylistCache) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let previous = LAST_CLEAN_AT.load(Ordering::Relaxed);
    if now.saturating_sub(previous) < CLEAN_INTERVAL.as_secs()
        || LAST_CLEAN_AT
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }

    if let Err(error) = cache.clean().await {
        LAST_CLEAN_AT.store(0, Ordering::Relaxed);
        tracing::warn!(error = %error, "yt-dlp playlist cache cleanup failed");
    }
}

#[cfg(test)]
mod tests {
    use super::{cache_key, from_cached_playlist, to_cached_playlist};
    use crate::media_types::{PlaylistInfo, PlaylistItem, PlaylistKind};

    fn playlist() -> PlaylistInfo {
        PlaylistInfo {
            title: "Collection".into(),
            page_url: "https://example.test/playlist".into(),
            kind: PlaylistKind::Playlist,
            total: 2,
            items: vec![
                PlaylistItem {
                    id: "first".into(),
                    title: "First item".into(),
                    page_url: "https://example.test/first".into(),
                    playlist_index: 1,
                },
                PlaylistItem {
                    id: "second".into(),
                    title: "Second item".into(),
                    page_url: "https://example.test/second".into(),
                    playlist_index: 2,
                },
            ]
            .into_boxed_slice(),
        }
    }

    #[test]
    fn playlist_adapter_round_trips_order_ids_titles_and_urls() {
        let key = cache_key("grab-ytdlp-video-v1:identity");
        let stored = to_cached_playlist(&key, &playlist()).expect("complete playlist");
        let restored = from_cached_playlist(&key, stored).expect("valid cached playlist");

        assert_eq!(restored.title.as_ref(), "Collection");
        assert_eq!(restored.page_url.as_ref(), "https://example.test/playlist");
        assert_eq!(restored.total, 2);
        assert_eq!(restored.items[0].id.as_ref(), "first");
        assert_eq!(restored.items[0].title.as_ref(), "First item");
        assert_eq!(restored.items[0].page_url, "https://example.test/first");
        assert_eq!(restored.items[1].id.as_ref(), "second");
        assert_eq!(restored.items[1].title.as_ref(), "Second item");
        assert_eq!(restored.items[1].page_url, "https://example.test/second");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn playlist_cache_round_trips_a_grab_key() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = yt_dlp::cache::CacheConfig::builder()
            .cache_dir(directory.path().to_path_buf())
            .build();
        let cache = yt_dlp::cache::PlaylistCache::new(&config, Some(60))
            .await
            .expect("create playlist cache");
        let key = cache_key("grab-ytdlp-video-v1:round-trip");
        let stored = to_cached_playlist(&key, &playlist()).expect("complete playlist");
        cache.put(key.clone(), stored).await.expect("store");
        let restored = cache.get(&key).await.expect("read").expect("hit");
        let info = from_cached_playlist(&key, restored).expect("valid cached playlist");
        assert_eq!(info.total, 2);
        assert_eq!(info.items[1].title.as_ref(), "Second item");
    }

    #[test]
    fn playlist_cache_id_is_namespaced_by_authentication_identity() {
        assert_ne!(
            cache_key("grab-ytdlp-video-v1:anonymous"),
            cache_key("grab-ytdlp-video-v1:authenticated")
        );
        assert!(cache_key("grab-ytdlp-video-v1:anonymous").starts_with("grab-ytdlp-playlist-v1:"));
    }

    #[test]
    fn incomplete_empty_oversized_and_ephemeral_collections_are_not_cached() {
        let mut info = playlist();
        info.total = 3;
        assert!(to_cached_playlist("key", &info).is_none());

        let mut info = playlist();
        info.total = 0;
        info.items = Box::default();
        assert!(to_cached_playlist("key", &info).is_none());

        let mut info = playlist();
        info.kind = PlaylistKind::Stories;
        assert!(to_cached_playlist("key", &info).is_none());

        let mut info = playlist();
        info.kind = PlaylistKind::Highlights;
        assert!(to_cached_playlist("key", &info).is_none());
    }

    #[test]
    fn rotating_or_non_http_item_urls_are_not_cached() {
        let mut info = playlist();
        info.items[0].page_url = "https://cdn.example/item?signature=rotating".into();
        assert!(to_cached_playlist("key", &info).is_none());

        let mut info = playlist();
        info.items[0].page_url = "ftp://example.test/item".into();
        assert!(to_cached_playlist("key", &info).is_none());
    }

    #[test]
    fn rotating_collection_urls_are_not_cached() {
        let mut info = playlist();
        info.page_url = "https://example.test/playlist?token=rotating".into();
        assert!(to_cached_playlist("key", &info).is_none());
    }

    #[test]
    fn cached_playlist_rejects_wrong_identity_and_corrupt_metadata() {
        let key = cache_key("grab-ytdlp-video-v1:identity");
        let stored = to_cached_playlist(&key, &playlist()).expect("complete playlist");
        assert!(from_cached_playlist("different-key", stored.clone()).is_none());

        let mut corrupted = stored;
        corrupted.description = Some("not cache metadata".to_string());
        assert!(from_cached_playlist(&key, corrupted).is_none());
    }
}
