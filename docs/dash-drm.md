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

Native DASH currently remains ClearKey-only. Grab now has an isolated
PlayReady challenge/license-response worker, but it is not yet connected to
DASH manifest planning, device-path configuration, or MP4 decryption; PlayReady
presentations therefore remain on the existing fallback path. The worker
requires a user-provisioned `.prd` device and supports one PlayReady PSSH with
one WRMHEADER per acquisition. Custom authorization headers and non-standard
license challenge formats are not supported yet.

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
