# Tool installer trust model

Grab downloads yt-dlp, gallery-dl, ffmpeg, and quickjs from GitHub releases.
Each download is SHA-256 verified before installation.

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
