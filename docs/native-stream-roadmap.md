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

### 3a. Fragmented-MP4 WVTT extraction — initial implementation

The selected static DASH WVTT representation is assembled through the existing
destination-part writer, then decoded separately from DASH planning and FFmpeg
muxing. Use mp4box sample tables for sample offsets and presentation timestamps.
Preserve WebVTT cue identifiers, settings, and payloads; normalize cue times to
a private WebVTT staging file. FFmpeg remains responsible for sidecar conversion
and embedded output.

The initial timestamp mapping is intentionally limited to a single zero-origin
Period using SegmentTemplate with no presentation-time offset. Non-zero Period
starts, non-zero presentation-time offsets, multi-Period presentations, other
addressing modes, edit lists, malformed or unsupported sample boxes, and
explicit cue-time extensions remain on yt-dlp fallback. Bound the assembled
track to 64 MiB, fragment sample count to 50,000, individual samples to 256 KiB,
total sample payload to 48 MiB, cue count to 50,000, cue text to 12 MiB, and
normalized output to 16 MiB. Empty cues are ignored; an entirely empty subtitle
track does not block media finalization.

Acceptance: parser tests for cue box parsing, timestamp normalization, empty
cues, malformed boxes, bounds, timestamp eligibility, and sidecar/embedded
FFmpeg handling.

### 3b. Fragmented-MP4 STPP extraction — bounded initial implementation

The selected static DASH STPP representation uses the existing destination-part
writer and is parsed with the project's existing mp4box and quick-xml
dependencies. STPP sample entries must declare the supported TTML namespace,
the subtitle handler must be subt, and the track must contain one fragmented
STPP sample description. Tracks with edit lists, subsample resources, auxiliary
MIME types, or unsupported sample-entry extensions remain on yt-dlp fallback.

TTML timing is accepted only with media timeBase (the TTML default when the
attribute is absent). Cue times are interpreted on the track presentation
timeline, not relative to the containing sample or DASH segment. Each sample's
document is clipped to its MP4 presentation interval [PTS, PTS + duration);
adjacent repeated cues are coalesced. The accepted XML subset is limited to
untimed, zero-origin body/div containers; plain p text with optional span and
br children; and begin/end/dur expressed as millisecond-accurate clock-time or
h, m, s, and ms offset-time. Empty documents are valid. TTML styling, regions, non-empty head metadata, external images,
inherited/container timing, sequential time containers, frame/tick/wallclock
expressions, CDATA, custom entities, and unsupported XML constructs remain on
yt-dlp fallback. An absent or empty head is accepted; non-empty head content is
rejected rather than silently discarded. The native parser does not attempt to
render TTML layout or styles.

Bounds: assembled track 64 MiB; 50,000 samples; 256 KiB per sample; 48 MiB
total sample payload; 16 KiB sample-entry metadata; XML depth 32 and 20,000
elements per document; 50,000 cues; 12 MiB cue text; 16 MiB normalized output.
FFmpeg continues to consume the normalized WebVTT sidecar for sidecar and
embedded subtitle finalization.

Acceptance: tests for sample timeline clipping and duplicate sample snapshots,
timeBase and time-expression validation, namespace/element restrictions,
whitespace/entities, malformed or oversized XML, sample and output bounds,
and unchanged WVTT and FFmpeg finalization paths.

### 4. Shared DRM across HLS and DASH

Keep protocol parsing separate from the shared protection-system, CDM, key
handling, and sample-decryption layers. HLS already has identity AES-128 and
MPEG-TS identity SAMPLE-AES handling; DASH has bounded ClearKey CENC/CBCS
decryption. The shared metadata layer now recognizes DASH Widevine PSSH and
HLS Widevine/PlayReady/FairPlay signaling, but those HLS providers still fall
back until their shared CDM/decryption path is implemented.

Next, add an isolated Widevine/PlayReady CDM worker and license exchange usable
by both DASH and HLS CMAF. Keep FairPlay Streaming separate because its
SAMPLE-AES and skd URI flow uses Apple-specific application key handling rather
than the Widevine or PlayReady CDM protocol. Do not extract browser secrets or
log license challenges, license responses, content keys, or device material.

Acceptance: validated protocol-specific initialization data mapped to a shared
track-scoped KID/key model; exact CENC/CBCS sample auxiliary-data validation;
bounded worker IPC, license responses, and resource parsing; zeroizing parent
key storage; no key leakage into logs or argv; and known-answer fixtures for
each enabled scheme. Unsupported schemes continue through the extractor
fallback.

### 5. Broader DASH index addressing

After flat SIDX has production evidence, evaluate nested/hierarchical SIDX,
representation-level indexes, BaseURL byte ranges, and SegmentBase timing
attributes such as presentation-time offsets. Each extension needs explicit
standards-based semantics and separate tests; unsupported forms remain fallback.

