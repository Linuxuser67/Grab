# Native DASH Common Encryption

Native DASH decryption supports static, single-Period fragmented MP4
presentations using CENC or CBCS, provided the MPD declares a ClearKey
protection descriptor and an HTTPS license URL. Fragment-local `sgpd` version 1 / `sbgp` version 0
`seig` sample groups may rotate KIDs between samples; Grab discovers every
selected sample KID and requests the corresponding keys before decryption.
Rotation is limited to KID changes that preserve the initialization segment's
IV size, constant IV, and CBCS crypt/skip pattern, with at most 64 distinct
KIDs per selected track.

Grab sends the selected KIDs to the MPD's ClearKey license URL using the
Encrypted Media Extensions ClearKey request format:

```json
{
  "kids": ["base64url-encoded-kid"],
  "type": "temporary"
}
```

The response must be a JSON Web Key Set containing 128-bit octet keys:

```json
{
  "keys": [{
    "kty": "oct",
    "kid": "base64url-encoded-kid",
    "k": "base64url-encoded-content-key"
  }],
  "type": "temporary"
}
```

The `dashif:Laurl` (or supported legacy ClearKey `Laurl`) value is resolved
against the MPD URL. License endpoints must use HTTPS and pass Grab's existing
public-host validation. Requests are bounded by a 30-second timeout and a
64 KiB response limit. Keys are held only in zeroizing memory; Grab does not
persist them, log them, or fall back to a local key file. The former
`drm-keys.txt` configuration is no longer read.

Native DASH also supports PlayReady for static, single-Period fragmented MP4
CENC/CBCS tracks when the MPD supplies one supported PlayReady initialization
object and an explicit HTTPS DASHIF Laurl. Initialization data may be
signalled as cenc:pssh or mspr:pro; cenc:pssh takes precedence within a
descriptor. Each selected track must resolve to exactly one WRMHEADER.
Configure a user-provisioned .prd device file under Advanced → Media
preferences. Grab stores only the file path in GSettings and reads device
material inside the short-lived PlayReady worker.

The parent process retains network transport, host validation, proxy policy,
and bounded HTTPS license requests, including the PlayReady SOAP
`AcquireLicense` action header. The worker performs device verification,
challenge creation, and response parsing. The PlayReady CDM normalizes
GUID-layout KIDs to ISO Common Encryption byte order before returning them;
Grab preserves that byte order when matching fragment sample KIDs. Retrieved
content-key allocations are moved into zeroizing storage immediately. This
process boundary is not an OS sandbox.

PlayReady licenses requiring custom authorization headers, DASHIF authorization
URL workflows, multiple WRMHEADER records, or non-standard challenge formats
remain on extractor fallback. Encrypted subtitles also remain unsupported.

Widevine remains on fallback: the currently available Rust CDM crates depend
on the unpatched `rsa 0.9.x` advisory, which Grab does not suppress.

The current native media subset requires one encrypted track in each selected
initialization resource, one track fragment per media resource, explicit
sample sizes and offsets, and in-band sample encryption metadata. Indexed
SegmentBase is supported when the initialization byte range and a flat SIDX
are present; unindexed single-file SegmentBase remains on fallback because its
initialization and media fragments are not separately addressable by this
adapter. Track-level/global sample-group references, malformed or incomplete sample
group runs, per-sample encryption-parameter changes, dynamic or multi-Period
presentations, and encrypted subtitles remain unsupported and fail closed.

Different selected audio and video tracks may each use their own default KID
and license URL. The current CENC adapter accepts only in-band `senc`
metadata; auxiliary information must be validated as pointing to that same
`senc` data. Unsupported layouts retain the existing fallback behavior.
Only acquire licenses for media you are authorized to decrypt.
