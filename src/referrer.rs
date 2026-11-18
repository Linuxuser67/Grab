//! Canonical source-page identity shared by request metadata and preview caches.

use sha2::{Digest, Sha256};

/// Hash a validated source-page URL for cache identity without retaining its
/// path or query in cache keys. Fragments do not participate in HTTP requests.
pub(crate) fn cache_identity(raw: Option<&str>) -> Result<Option<[u8; 32]>, ()> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut source = url::Url::parse(raw).map_err(|_| ())?;
    if !matches!(source.scheme(), "http" | "https")
        || source.host_str().is_none()
        || !source.username().is_empty()
        || source.password().is_some()
    {
        return Err(());
    }
    source.set_fragment(None);
    Ok(Some(Sha256::digest(source.as_str().as_bytes()).into()))
}

#[cfg(test)]
mod tests {
    use super::cache_identity;

    #[test]
    fn cache_identity_hashes_the_full_source_url_without_its_fragment() {
        let source = cache_identity(Some("https://example.test/watch?id=one#player"))
            .expect("valid source URL")
            .expect("source hash");
        assert_eq!(
            Ok(Some(source)),
            cache_identity(Some("https://example.test/watch?id=one#other"))
        );
        assert_ne!(
            Ok(Some(source)),
            cache_identity(Some("https://example.test/watch?id=two"))
        );
        assert_ne!(
            Ok(Some(source)),
            cache_identity(Some("https://example.test/other?id=one"))
        );
        assert_eq!(Ok(None), cache_identity(None));
        assert!(cache_identity(Some("file:///etc/passwd")).is_err());
        assert!(cache_identity(Some("https://user:pass@example.test/watch")).is_err());
    }
}
