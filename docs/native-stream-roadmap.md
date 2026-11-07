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

## Native HLS supported profile

The native HLS path covers multivariant master playlists with selectable video
variants and external audio/subtitle renditions; static VOD and EVENT media
playlists; ordinary non-low-latency live capture; MPEG-TS and fragmented-MP4
media with initialization maps; explicit and valid implicit byte ranges;
discontinuities; identity AES-128 keys (including key rotation and
`METHOD=NONE` resets); and identity SAMPLE-AES for MPEG-TS. Master-playlist
`EXT-X-SESSION-KEY` hints are accepted only for AES-128 with an explicit or
implicit identity key format; Grab never fetches or substitutes these hints for
the selected media playlist's authoritative `EXT-X-KEY`.

For complete static VOD/EVENT playlists, paired `EXT-X-CUE-OUT` /
`EXT-X-CUE-IN` boundaries omit every segment in the marked ad break from
native resource planning and from the local FFmpeg playlist. Valid
`EXT-X-CUE-OUT-CONT` markers are checked against elapsed media duration and
must retain one consistent declared break duration. The first segment after
`CUE-IN` remains program content. Ad-only keys and initialization resources
are not fetched, surviving resource names retain their source segment indices,
and implicit byte-range offsets advance across removed source segments.
`METHOD=NONE` resets are preserved when their original boundary is inside an
omitted ad. Live playlists with ad markers, ad intervals containing a
discontinuity, playlists using `EXT-X-START`, ambiguous or incomplete cues,
and all `EXT-X-DATERANGE` playlists remain on yt-dlp fallback until
date-time ranges can be mapped unambiguously to individual media segments.

This is a deliberately bounded profile, not a claim of support for every HLS
extension. Unknown parser tags and unsupported resource-affecting extensions
fall back to yt-dlp. This includes alternate VIDEO rendition groups, LL-HLS
parts, preload hints, delta updates, rendition reports, blocking-reload
query parameters, GAP, DEFINE, SKIP, and CONTENT-STEERING. I-frame-only media
playlists, unsupported session-key methods, non-identity session-key formats,
non-identity media-key formats, SAMPLE-AES over fragmented MP4/CBCS, and other
unsupported encryption schemes also remain on fallback.
AES-128 and supported SAMPLE-AES keys continue to use the existing
bounded key-resource path; no license or CDM acquisition is added.

## Explicitly unplanned

- Low-Latency HLS (LL-HLS): part/preload scheduling, delta playlist updates,
  rendition reports, and blocking reload.
- Low-Latency DASH (LL-DASH): partial-segment availability and low-latency
  chunk transfer behavior.

Do not silently strip or ignore low-latency metadata and then treat the
playlist as a complete segment-based presentation.

## Phases

### 1. Static DASH SegmentBase and hierarchical SIDX — implemented

Support a static single-period MP4 representation with an in-file
`SegmentBase@indexRange`, `Initialization@range`, and bounded SIDX hierarchy.
Fetch each bounded index range, decode SIDX with the existing `mp4box`
dependency, and translate direct and nested references into ordered Grab
byte-range resources. Separate initialization URLs, dynamic SegmentBase, malformed or out-of-file
references, and hierarchies exceeding the explicit depth/node/size limits remain
on yt-dlp fallback.

Acceptance is covered by tests for exact/non-exact index ranges, initialization
and media extents, nested child indexes, reference identity/timing validation,
overlap, overflow, and truncated responses. Index responses require strict
`206`/Content-Range validation and bounded bodies; media bytes flow through the
existing ordered writer and recovery journal. Static non-zero
`SegmentBase@presentationTimeOffset` values are also retained for per-track
timestamp alignment during finalization.

### 2. Static multi-period DASH — implemented

Model each Period as an explicit sub-plan with independent inherited
BaseURL/addressing/timing. Create stable identities that include period identity
and addressing, and preserve period order for track assembly. Handle initialization
changes at Period boundaries explicitly; do not concatenate incompatible
initialization sections into one track. Keep unsupported transitions on yt-dlp
fallback.

Acceptance: ordered multi-period video/audio plans, period-local initialization,
time/byte-range validation, representation selection stable across MPD reorder,
and fixtures covering discontinuities and differing codec/init metadata.

### 3a. Fragmented-MP4 WVTT extraction — implemented bounded profile

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

### 3b. Fragmented-MP4 STPP extraction — implemented bounded profile

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

### 4. Explicit-key and DRM expansion — shelved

This phase is shelved and is not part of the native-stream completion PR. No
native DASH CENC/CBCS decryption expansion, user-supplied CENC keys, license
acquisition, Widevine/PlayReady device-file support, or CDM integration is
included. The existing HLS identity SAMPLE-AES MPEG-TS path remains on `main`;
shelving this phase does not remove that already-supported HLS behavior.

### 5. DASH presentation-time offsets — implemented bounded profile

Static `SegmentTemplate` and `SegmentBase` `presentationTimeOffset` values are
converted using each representation's timescale and Period start, then applied
to that track's FFmpeg input with `-itsoffset`. Video and audio are offset
independently; a non-zero Period start with zero PTO is handled by the same
mapping. Zero-origin tracks omit the FFmpeg option and retain the existing
finalization argument sequence. Dynamic non-zero PTO and non-zero
`SegmentList@presentationTimeOffset` remain on yt-dlp fallback. Existing
single-Period dynamic Period-start handling is unchanged and does not receive
the new static mux offsets.

Multi-Period template sources must produce one identical effective timestamp
offset across all Periods; otherwise the planner rejects the presentation.
WVTT/STPP normalization retains its zero-origin restriction and falls back when
the selected subtitle track cannot be mapped without a separate timestamp
transformation.

Acceptance covers per-representation video/audio offsets, Period-start
mapping, SegmentBase PTO, zero-origin behavior, SegmentList fallback, and
FFmpeg/ffprobe validation that independently offset video and audio start within
one video frame.

### 6. Broader DASH index addressing — evaluated, bounded subset implemented

The native path now supports:
- `RepresentationIndex@sourceURL` with an explicit bounded `range`, either
  referencing the media resource itself or a separate HTTPS index resource.
  An external SIDX uses `first_offset` relative to the media resource; its
  media extents are validated against the media resource's independently
  checked total length. External indexes are limited to a flat SIDX.
- Bounded hierarchical SIDX when the index and media ranges are in the same
  media resource.

The following remain explicit yt-dlp fallbacks:
- External RepresentationIndex hierarchies and external index resources without
  an explicit bounded range.
- Separate initialization URLs, dynamic SegmentBase, and non-zero
  `eptDelta`/`pbDelta`.

`BaseURL@byteRange` remains unsupported. ISO/IEC 23009-1 Annex E defines it
as an alternate byte-range URL template for regular GET requests when HTTP
partial GET is unavailable or not honored. Grab deliberately does not retry a
failed native resource request or add an alternate request path, so this
fallback-only optimization is not implemented. Ordinary DASH byte ranges
continue to use Grab's strict HTTP Range and Content-Range validation.

