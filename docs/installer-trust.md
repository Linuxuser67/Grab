# Tool installer trust model

Grab downloads yt-dlp, gallery-dl, and quickjs from GitHub releases; non-Flatpak
builds also download ffmpeg. Each download is SHA-256 verified before
installation.

Flatpak builds use the `ffmpeg` and `ffprobe` provided by the GNOME runtime
(`/usr/bin`), or an app-bundled copy in `/app/bin` if one is added later. Grab
does not download FFmpeg under Flatpak, and the user-library copy is ignored so
a stale FFmpeg cannot shadow the runtime. yt-dlp, gallery-dl, and quickjs still
use the verified GitHub download path below.

## Digest source

The SHA-256 digest comes from the **same GitHub API response** as the binary
download URL (the release asset metadata). There is no separate checksum file
or hardcoded pin.

## What this guards against

- Corrupted or truncated downloads (network errors, CDN issues)
- Swapped assets (wrong file served for the asset URL)

## What this does NOT guard against

- A compromised GitHub release itself. If an attacker publishes a malicious
  binary to the upstream release, the digest will match it. TLS to
  `api.github.com` is the root of trust.

## Fail-closed behavior

If the release metadata carries no SHA-256 digest, installation is refused
with an error — the previous binary (if any) is kept. No unverified binary
is ever installed.
