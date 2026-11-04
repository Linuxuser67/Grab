# yt-dlp 2.9.1 Fetcher evaluation

## Scope

This is a controlled, test-only evaluation of yt_dlp::download::Fetcher from the exact yt-dlp = 2.9.1 version already pinned by Grab. It does not change the production downloader or add a dependency.

The compatibility probes use a shared reqwest::Client, a local HTTP server, and destinations inside tempfile directories. They do not access the network or write to user download locations.

## Result

**Do not replace Grab's plain HTTP engine with Fetcher 2.9.1.**

The positive probe confirms that Fetcher::with_client accepts and reuses an externally created Arc<reqwest::Client> and can download a normal range-capable HTTP resource. The negative probes show that this transport reuse is not enough to preserve Grab's download correctness and filesystem guarantees.

## Findings

1. **Range responses are not validated.** The parallel segment path calls error_for_status() and streams the response body at the requested offset. It does not require status 206, compare Content-Range with the requested range, or cap the body to the exact requested byte count. The probe demonstrates that a 200 OK full-object response to a segment request is accepted and can produce a mixed-version output.

2. **Same-size files bypass validator identity checks.** After a one-byte range probe, fetch_asset returns success when the existing path's length equals the server's reported total. The implementation does not compare ETag or Last-Modified and does not issue If-Range. The probe demonstrates that a local file from an old representation is retained even when the server supplies a new ETag.

3. **Resume detection is content sampling, not representation validation.** For an incomplete file, the segment logic samples the beginning and end of a region to decide whether it is already present. It does not establish that the local bytes belong to the current remote representation.

4. **Destination handling does not meet Grab's ownership policy.** The API writes directly to a PathBuf. For a new file, the upstream helper uses create(true) plus truncate(true), rather than create_new; for an existing path it opens the path for read/write and changes its length. The probe demonstrates that an existing destination symlink is followed and its target truncated/written.

5. **The retry setter is ineffective in 2.9.1.** with_retry_attempts changes a field used for Debug, while the probe and segment requests use a separate RetryPolicy::default() (three attempts). Passing zero still results in three range attempts. This conflicts with Grab's no-retry requirement for native downloads.

6. **The API boundary is too narrow for direct integration.** fetch_asset owns a filesystem path rather than accepting Grab's destination writer or a caller-owned writer. Progress is exposed through a non-fallible callback; the API does not expose Grab's checkpoint, byte-accounting, or publication protocol.

The relevant pinned upstream implementation is available in:
- [fetcher.rs](https://github.com/boul2gom/yt-dlp/blob/v2.9.1/src/download/engine/fetcher.rs)
- [parallel.rs](https://github.com/boul2gom/yt-dlp/blob/v2.9.1/src/download/engine/parallel.rs)
- [fs.rs](https://github.com/boul2gom/yt-dlp/blob/v2.9.1/src/utils/fs.rs)
- [retry.rs](https://github.com/boul2gom/yt-dlp/blob/v2.9.1/src/utils/network/retry.rs)

## Recommendation

Keep Grab's existing reqwest-based HTTP engine. The yt-dlp crate remains useful for extraction models, cache implementations, and shared request-client compatibility, but its 2.9.1 Fetcher is not a safe replacement for Grab's guarded transfer, resume, staging, and publication layers.

Re-evaluate only after an upstream release provides exact range/status/body validation, representation validators with If-Range, no-follow/no-clobber destination handling, a working retry policy setting compatible with zero retries, and a writer/cancellation integration point.
