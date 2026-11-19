//! Canonical source-page identity shared by request metadata and preview caches.

use sha2::{Digest, Sha256};

const MAX_SOURCE_URL_LEN: usize = 2048;

/// Parse a bounded, credential-free HTTP(S) source-page URL.
///
/// Fragments do not participate in HTTP requests. Keeping validation here
/// ensures handoffs, request headers, and cache identities agree.
pub(crate) fn parse_source(raw: &str) -> Option<url::Url> {
    if raw.len() > MAX_SOURCE_URL_LEN {
        return None;
    }
    let mut source = url::Url::parse(raw.trim()).ok()?;
    if !matches!(source.scheme(), "http" | "https")
        || source.host_str().is_none()
        || !source.username().is_empty()
        || source.password().is_some()
    {
        return None;
    }
    source.set_fragment(None);
    Some(source)
}

/// Return the source origin without its path, query, or fragment.
pub(crate) fn origin_only(raw: &str) -> Option<String> {
    let mut source = parse_source(raw)?;
    source.set_path("/");
    source.set_query(None);
    source.set_fragment(None);
    Some(source.to_string())
}

/// Identity of the source context used by an outbound request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SourceIdentity {
    /// No source page was supplied.
    Absent,
    /// The canonical source origin used in the Referer header.
    Origin([u8; 32]),
    /// A supplied source page failed validation and must not share cache entries
    /// with an unbound request.
    Invalid,
}

/// Identify a source page by the validated origin used for outbound Referer headers.
#[must_use]
pub(crate) fn cache_identity(raw: Option<&str>) -> SourceIdentity {
    let Some(raw) = raw else {
        return SourceIdentity::Absent;
    };
    let Some(origin) = origin_only(raw) else {
        return SourceIdentity::Invalid;
    };
    SourceIdentity::Origin(Sha256::digest(origin.as_bytes()).into())
}

#[cfg(test)]
mod tests {
    use super::{SourceIdentity, cache_identity, origin_only, parse_source};

    #[test]
    fn parse_source_accepts_only_bounded_credential_free_http_urls() {
        assert_eq!(
            parse_source(" https://example.test/watch?id=one#player ")
                .expect("valid source URL")
                .as_str(),
            "https://example.test/watch?id=one"
        );
        assert!(parse_source("file:///etc/passwd").is_none());
        assert!(parse_source("javascript:alert(1)").is_none());
        assert!(parse_source("https://user:pass@example.test/watch").is_none());
        assert!(parse_source(&format!("https://example.test/{}", "x".repeat(2048))).is_none());
    }

    #[test]
    fn origin_only_removes_page_path_query_and_fragment() {
        assert_eq!(
            origin_only("https://example.test:8443/watch/private?token=secret#player"),
            Some("https://example.test:8443/".to_string())
        );
        assert_eq!(
            origin_only("http://example.test/path?token=secret"),
            Some("http://example.test/".to_string())
        );
        assert_eq!(origin_only("https://user:pass@example.test/watch"), None);
    }

    #[test]
    fn cache_identity_matches_the_canonical_request_origin() {
        let source = cache_identity(Some("https://example.test/watch?id=one#player"));
        assert!(matches!(source, SourceIdentity::Origin(_)));
        assert_eq!(
            source,
            cache_identity(Some("https://example.test/other/path?token=two#other"))
        );
        assert_ne!(
            source,
            cache_identity(Some("https://other.example.test/watch?id=one"))
        );
        assert_ne!(
            source,
            cache_identity(Some("http://example.test/watch?id=one"))
        );
        assert_eq!(SourceIdentity::Absent, cache_identity(None));
        assert_eq!(
            SourceIdentity::Invalid,
            cache_identity(Some("file:///etc/passwd"))
        );
        assert_eq!(
            SourceIdentity::Invalid,
            cache_identity(Some("https://user:pass@example.test/watch"))
        );
        assert_eq!(
            SourceIdentity::Invalid,
            cache_identity(Some(&"x".repeat(2049)))
        );
    }
}
