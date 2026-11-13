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

When a selected variant uses a separate audio rendition, Grab passes the
video and audio media playlists as separate FFmpeg inputs and uses `-copyts`
for both. Without it, FFmpeg rebases each input independently and can erase
an intentional source timestamp offset. The same option is used when the static
destination-part fast path supplies raw concatenated rendition tracks. Program
date-time metadata remains in the local media playlists; it is not treated as
an instruction to rewrite media timestamps.

HLS ad-insertion metadata is not used to filter content. `EXT-X-CUE-OUT`,
`EXT-X-CUE-OUT-CONT`, `EXT-X-CUE-IN`, OATCLS-SCTE35 and asset markers are
inert while planning native media resources: every listed media segment remains
in the fetch plan and the local FFmpeg playlist. The regenerated local playlist
does not promise to preserve the original opaque cue metadata. DASH SCTE-35
event metadata is likewise not used to reject or remove media segments.

This is a deliberately bounded profile, not a claim of support for every HLS
extension. Unknown parser tags and unsupported resource-affecting extensions
fall back to yt-dlp. This includes VIDEO groups with unmatched variant
references or more than one URI-less member, unsupported encryption schemes,
and extensions with unmet REQ-prefixed attributes. Complete static VOD/EVENT
playlists may omit well-formed EXT-X-GAP segments from the local resource plan;
live/LL-HLS gaps remain on fallback.

Mainstream Apple codec metadata is retained on selected variants: H.264
(avc1), HEVC (hvc1), AAC-LC (mp4a.40.2), E-AC-3 (ec-3) and AC-3
(ac-3) identifiers remain available for stream selection and profile
verification. VIDEO-RANGE accepts the Apple SDR, HLG and PQ values; unknown
values fall back instead of being silently discarded. Native finalization uses
FFmpeg stream copy and does not transcode unsupported codecs. FFmpeg/ffprobe
integration fixtures cover AVC/AAC SDR, HEVC/E-AC-3 PQ and AVC/AC-3 fallback
when the corresponding encoders are present on the test host. This matrix does
not claim Dolby Vision, AV1, or spatial-video verification.

For ordinary HLS, `EXT-X-START` is ignored during recording: all listed
segments are retained and the playback hint is omitted from the local playlist.
`EXT-X-CONTENT-STEERING` is accepted as benign metadata: Grab does not fetch
the steering manifest, does not select a Pathway, and downloads the variants
the master advertises. Pathway-driven host or URI rewriting is not applied.
Media-playlist NAME/VALUE and QUERYPARAM variable substitution is supported
for ordinary HLS and LL-HLS. Master-playlist
NAME/VALUE and QUERYPARAM variables are expanded in variant and rendition
URIs. Child media playlists may import explicitly named parent variables with
EXT-X-DEFINE:IMPORT; the selected variable scope is carried through video,
audio, subtitle, and LL-HLS reload planning. Undefined imports, duplicate
definitions, and unsupported IMPORT attributes remain on yt-dlp fallback.
I-frame-only media playlists, non-identity
session-key formats, non-identity media-key formats, SAMPLE-AES over
fragmented MP4/CBCS, and unsupported encryption remain on fallback. AES-128
and supported SAMPLE-AES keys in ordinary HLS continue to use the existing
bounded key-resource path; no license or CDM acquisition is added.

## Native Low-Latency HLS supported profile

LL-HLS media playlists are parsed with broadcast-hls; Grab retains ownership of
network requests, per-origin guards, part scheduling, staging, recovery, and
FFmpeg finalization. Confirmed EXT-X-PART resources are recorded as stable
(media sequence, part index) units. A complete parent segment is recognized
as closure of already captured parts only when the part indexes begin at zero,
remain contiguous, and their durations and initialization map match the parent.
An initialization map remains active across segments and snapshots.

Blocking reload uses _HLS_msn and, only when an open part index exists,
_HLS_part. Delta updates use _HLS_skip=YES only when CAN-SKIP-UNTIL is
advertised; SKIPPED-SEGMENTS advances the first visible media sequence, and
the omitted prefix must be covered by the committed capture before new media is
accepted. Sliding windows retain stable part identities. A single failed
blocking request is not retried in-place; the normal next-poll schedule resumes
capture. Preload hints remain speculative and are not fetched until the part is
confirmed in a later playlist. LL-HLS `EXT-X-PROGRAM-DATE-TIME` anchors are
retained on the first captured unit of each closed or open segment; malformed
or unanchored date-time tags fall back rather than being dropped. Native LL-HLS
enforces Apple's authoring-profile minimum `PART-HOLD-BACK >= 3 * PART-TARGET`.

This LL-HLS profile remains deliberately bounded. The first captured part must
be independently decodable (or covered by EXT-X-INDEPENDENT-SEGMENTS).
CMAF part capture without a known EXT-X-MAP, unsupported encryption
for partial/delta LL-HLS (complete segment playlists without parts can use the
ordinary AES-128 path), incomplete continuity, GAP, unknown required
attributes, and ad/date-range markers remain on yt-dlp fallback.
EXT-X-START is treated as a playback-position hint: LL-HLS recording retains
the full advertised live window rather than trimming media units to that hint.
Mapless parts are identified from playlist structure and passed to FFmpeg
as part of the completed assembled transport stream; Grab does not inspect
transport packets per resource. Native LL-HLS does not strip ad markers or
schedule ad-marked media. DRM, license acquisition, and CDM support remain
unplanned.

LL-HLS reconciliation keeps an incremental unit-position and per-media-sequence
part-run index. Normal polls verify only resources in the current playlist
snapshot; a full staged-resource integrity pass runs on restoration, every
60 seconds during capture, and on an ENDLIST snapshot. The part-aware recording
limit is 250,000 units, with the existing 32 MiB local-playlist serialization
limit still applying. A delta update that no longer overlaps the committed tail
is rejected and the already captured prefix is preserved; expired source media
is never silently skipped.

## Native Low-Latency DASH supported profile

The LL-DASH path reuses dash-mpd-core for typed MPD parsing and Grab's existing
dynamic SegmentTemplate planner, guarded HTTP client, streamed resource writer,
staging, live journal, recovery, and FFmpeg finalization. No separate DASH
downloader or new parser dependency is introduced.

The supported profile is a single-period dynamic MPD with HTTPS SegmentTemplate
addressing, an explicit MP4 audio/video MIME type, and an initialization
resource. SegmentTemplate availability attributes use normal inheritance:
a value on a lower-level SegmentTemplate overrides the inherited value.
Availability offsets must be finite and non-negative; their expanded
resource window is bounded by the existing DASH segment-count and recording
byte limits. availabilityTimeComplete=false requires a positive offset, an
initialization resource, and an explicit MP4 audio/video MIME type. The effective offset is
evaluated against every refreshed MPD without making representation identity
depend on the current offset or segment-duration window. Existing
policy-sensitive live keys are accepted as migration aliases, and restored
capture state is rewritten using the policy-independent key.

Grab streams the HTTP response body as it arrives into its owned staging
.part file. For LL-DASH, initialization is fetched first, then available media
segments are requested oldest-first with one active request per rendition.
This drains the retained backlog before waiting for progressively delivered
tail segments, avoiding time-shift-window starvation. The live journal commits
a segment only after its full response has completed and retains presentation
order regardless of fetch order. A stopped or interrupted in-progress
response is not treated as committed media; a later resume starts that segment
request again. Native transfers still have no automatic retries.

Finite BaseURL availabilityTimeOffset values are summed across MPD,
Period, AdaptationSet, and Representation levels for dynamic SegmentTemplate
and the bounded dynamic SegmentList profile. Negative/infinite offsets, static
offsets, BaseURL availabilityTimeComplete=false, and unsupported BaseURL
metadata remain on yt-dlp fallback. Static LL-DASH, SegmentBase, and static
SegmentList extensions beyond the existing explicit-resource profile also
remain on fallback. Static SegmentList presentationTimeOffset is applied to the
track timestamp mapping; dynamic SegmentList PTO remains on fallback. Dynamic
SegmentList is limited to a single MP4 Period with a separate initialization
resource, a zero-origin explicit SegmentTimeline containing one entry per
SegmentURL, default startNumber, contiguous equal-duration segments, and a
bounded live window. Sliding lists with non-default startNumber, implicit
timeline times, repeats, gaps, variable durations, missing initialization, or
non-MP4 tracks remain on fallback. Dynamic multi-Period MPDs, resource windows
that exceed the planner limits, DRM, and unsupported addressing also remain on
yt-dlp fallback. Unknown or conflicting presentation semantics are not silently
stripped.

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
finalization argument sequence. Static `SegmentList@presentationTimeOffset`
uses the same per-track mapping. Dynamic non-zero PTO remains on yt-dlp
fallback. Existing single-Period dynamic Period-start handling is unchanged
and does not receive the new static mux offsets.

Multi-Period template sources must produce one identical effective timestamp
offset across all Periods; otherwise the planner rejects the presentation.
WVTT/STPP normalization retains its zero-origin restriction and falls back when
the selected subtitle track cannot be mapped without a separate timestamp
transformation.

Acceptance covers per-representation video/audio offsets, Period-start
mapping, SegmentBase and static SegmentList PTO, zero-origin behavior, and
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

### 7. Dynamic DASH SegmentList — implemented bounded profile

Dynamic SegmentList is supported only for a single MP4 Period with a separate
initialization resource, a zero-origin explicit SegmentTimeline containing one
entry per SegmentURL, default startNumber, contiguous equal-duration segments,
and a bounded live window. Segment sequence numbers are derived from the
zero-origin timeline, allowing the existing live journal to detect missing or
changed resources across MPD refreshes.

Sliding lists with non-default startNumber, implicit timeline times, repeats,
gaps, variable durations, duration-mode addressing, non-zero
presentationTimeOffset, SegmentList-level availabilityTimeComplete,
eptDelta/pbDelta, missing initialization, non-MP4 tracks, and other unsupported
SegmentList addressing remain on yt-dlp fallback. Finite SegmentList-level
availabilityTimeOffset values use normal inheritance, with a lower-level value
overriding its parent. Finite BaseURL-level availabilityTimeOffset values are
summed across MPD, Period, AdaptationSet, and Representation levels; the two
effective offsets are added for the bounded dynamic profile. No automatic
native transfer retries are introduced.

Acceptance covers explicit timeline mapping, time-shift window selection,
stable sequence identity, initialization identity, and conservative rejection
of ambiguous or unsupported timeline forms.

