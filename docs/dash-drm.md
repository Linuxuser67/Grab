# Native DASH Common Encryption

Native DASH decryption supports static, single-Period fragmented MP4 presentations
using CENC or CBCS and one default KID per selected video or audio representation.
Grab can use either an explicitly supplied KID-to-key file or an explicitly
configured Widevine license device and endpoint.

## Explicit content keys

Create:

```text
${XDG_CONFIG_HOME:-~/.config}/grab/drm-keys.txt
```

Use one KID-to-key mapping per line. KIDs may be a UUID with hyphens or 32
hexadecimal characters; content keys must be 32 hexadecimal characters:

```text
# KID:KEY
00112233-4455-6677-8899-aabbccddeeff:0123456789abcdef0123456789abcdef
```

Restrict the file to the current user:

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/grab"
chmod 600 "${XDG_CONFIG_HOME:-$HOME/.config}/grab/drm-keys.txt"
```

## Widevine license acquisition

Grab does not include or generate a Widevine device. Provide a device file
that you are authorized to use (a `.wvd` file), then create:

```text
${XDG_CONFIG_HOME:-~/.config}/grab/drm-license.json
```

Example:

```json
{
  "version": 1,
  "widevine": {
    "device": "/absolute/path/to/device.wvd",
    "license_url": "https://license.example.test/widevine",
    "headers": {
      "Authorization": "Bearer replace-with-your-service-token"
    },
    "content_type": "application/octet-stream",
    "service_certificate": "none"
  }
}
```

The only supported service-certificate values are `none`, `google-common`,
and `google-staging`. Omit `headers` when the license endpoint requires none.
The endpoint must be HTTPS and must resolve to a public address. License
requests do not follow redirects. Header names reserved for HTTP routing and
framing are rejected. Responses and device/configuration files are size-bounded.

Restrict both files to the current user:

```sh
chmod 600 "${XDG_CONFIG_HOME:-$HOME/.config}/grab/drm-license.json"
chmod 600 /absolute/path/to/device.wvd
```

Grab rejects symlinks, files owned by another user, and files with group or
other permissions. KIDs and content keys are not written to queue state or
application logs. Explicit KID:KEY entries take precedence; license acquisition
is used only for selected KIDs that are not already configured.

## Current native subset

The current native subset requires one encrypted track in each selected
initialization resource, one track fragment per media resource, explicit
sample sizes and offsets, and in-band sample encryption metadata. Indexed
SegmentBase is supported when the initialization byte range and a flat SIDX
are present; unindexed single-file SegmentBase remains on fallback because its
initialization and media fragments are not separately addressable by this
adapter. In-track key rotation and sample-group KID overrides, dynamic/multi-Period
presentations, encrypted subtitles, PlayReady license acquisition, and HLS
SAMPLE-AES remain outside this increment. Different selected tracks may each
use their own default KID and key. Unsupported layouts retain the existing
fallback behavior; Grab does not publish ciphertext as a successful decrypted
download.

Only configure CDM devices, authorization headers, and keys for media you are
authorized to decrypt.
