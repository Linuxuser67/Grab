# Native DASH Common Encryption

Native DASH decryption is currently limited to static, single-Period fragmented
MP4 presentations using CENC or CBCS and one default KID per selected video
or audio representation. Grab does not acquire license responses, extract
browser secrets, or contact DRM license servers. Content keys must be supplied
by the user.

Create this file:

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

Grab rejects symlinks, files owned by another user, and files with group or
other permissions. The file is size-bounded. Key contents are not written to
queue state or application logs.

The current native subset requires one encrypted track in each selected
initialization resource, one track fragment per media resource, explicit
sample sizes and offsets, and in-band sample encryption metadata. Indexed
SegmentBase is supported when the initialization byte range and a flat SIDX
are present; unindexed single-file SegmentBase remains on fallback because its
initialization and media fragments are not separately addressable by this
adapter. In-track key rotation and sample-group KID overrides, dynamic/multi-Period
presentations, and encrypted subtitles are not supported by this increment.
Different selected tracks may each use their own default KID and key. The
current CENC adapter accepts only in-band senc metadata; auxiliary information
must be validated as pointing to that same senc data. Unsupported layouts retain the existing
fallback behavior; a missing configured key is reported as an error rather
than publishing ciphertext as a successful media download.

Only configure keys for media you are authorized to decrypt.
