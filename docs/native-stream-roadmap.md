# Native stream implementation roadmap

This roadmap evolves Grab's native HLS/DASH support while retaining the existing
resource writer, destination-side `.part` files, XDG staging, checkpoint journal,
resume validation, bounded streaming HTTP client, URL-host guards, and final
publication path. Parser and media-format crates should be reused where they
provide a maintained, typed API; protocol planning and download lifecycle remain
owned by Grab.

## Constraints

- Static HLS segments remain sequential.
- Do not add automatic retries to native HLS or DASH transfers.
- Do not replace Grab's resource staging, ordered writer, journal, recovery, or
  destination publication with another downloader's temporary-file scheme.
- Keep all manifest, index, resource-count, and byte-size inputs bounded.
- Reject malformed, unsupported, encrypted, or ambiguous structures to yt-dlp
  rather than guessing at byte offsets or media continuity.
- Every new addressing mode must use Grab's existing byte-range response
  validation, per-origin host guard, staging writer, and resume identity.

## Phases

### 1. Static DASH SegmentBase and flat SIDX — in progress

Support a static single-period MP4 representation with an in-file
`SegmentBase@indexRange`, `Initialization@range`, and one flat SIDX. Fetch the
bounded index range, decode SIDX with the existing `mp4box` dependency, and
translate each direct reference into an ordinary Grab byte-range resource.
Hierarchical SIDX, separate initialization URLs, dynamic SegmentBase, non-zero
SegmentBase presentation-time offsets, and malformed or out-of-file references
remain on yt-dlp fallback.

Acceptance: checked offset arithmetic; validated SIDX timescale; strict
`206`/Content-Range validation; bounded index response; direct references only;
tests for exact/non-exact index ranges, initialization and media extents,
hierarchical references, overflow, and truncated responses; all media bytes flow
through the existing ordered writer and recovery journal.

### 2. Static multi-period DASH

Model each Period as an explicit sub-plan with independent inherited
BaseURL/addressing/timing. Create stable identities that include period identity
and addressing, and preserve period order for track assembly. Handle initialization
changes at Period boundaries explicitly; do not concatenate incompatible
initialization sections into one track. Keep unsupported transitions on yt-dlp
fallback.

Acceptance: ordered multi-period video/audio plans, period-local initialization,
time/byte-range validation, representation selection stable across MPD reorder,
and fixtures covering discontinuities and differing codec/init metadata.

### 3. Fragmented-MP4 subtitle extraction

Use typed ISO BMFF parsing to identify WVTT and STPP sample entries and extract
sample payloads/timing into a normalized subtitle representation (VTT/SRT).
Keep extraction separate from DASH resource planning and FFmpeg muxing. Preserve
language and track selection, and bound sample counts, cue sizes, and output bytes.

Acceptance: fixtures for WVTT/STPP, timestamp normalization, empty cues,
malformed fragments, and sidecar/embedded output behavior.

### 4. Explicit-key encrypted media support

Evaluate HLS SAMPLE-AES and fragmented-MP4 CENC decryption as separate features.
The first scope is user-supplied keys only. Do not add license-server discovery,
Widevine/PlayReady acquisition, browser secret extraction, or automatic key
fetching in this phase. Audit per-track encryption metadata and sample auxiliary
information before enabling any format; reject unsupported schemes and patterns.

Acceptance: validated track-scoped keys/IVs, correct clear/encrypted sample
boundaries, bounded parsing, no key leakage into logs or argv, and known-answer
fixtures for each enabled scheme.

### 5. Broader DASH index addressing

After flat SIDX has production evidence, evaluate nested/hierarchical SIDX,
representation-level indexes, BaseURL byte ranges, and SegmentBase timing
attributes such as presentation-time offsets. Each extension needs explicit
standards-based semantics and separate tests; unsupported forms remain fallback.

