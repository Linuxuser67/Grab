# Native DASH Common Encryption

Native DASH decryption supports static, single-Period fragmented MP4 presentations
using CENC or CBCS and one default KID per selected video or audio representation.
Grab obtains the required content keys through an explicitly configured Widevine
device and license endpoint. There is no manual content-key file fallback.

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
application logs. Every selected encrypted track must have a matching Widevine
PSSH and a license response that supplies its default KID.

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
