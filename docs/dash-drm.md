# Native DASH Common Encryption

Native DASH decryption supports static, single-Period fragmented MP4
presentations using CENC or CBCS, provided the MPD declares a ClearKey
protection descriptor and an HTTPS license URL. Fragment-local `sgpd`/`sbgp`
`seig` sample groups may rotate KIDs between samples; Grab discovers every
selected sample KID and requests the corresponding keys before decryption.
Rotation is limited to KID changes that preserve the initialization segment's
IV size, constant IV, and CBCS crypt/skip pattern.

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

Widevine and PlayReady license exchanges are not supported by this increment.
They require system-specific CDM challenges and device provisioning; such
presentations remain on the existing fallback path. License servers that
require custom authorization headers or challenge formats beyond the ClearKey
JSON exchange are also not supported yet.

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
