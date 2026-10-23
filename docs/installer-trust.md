# Tool installer trust model

Grab downloads yt-dlp and Deno from GitHub releases and the stable gallery-dl
Linux executable from the official Codeberg release. All downloaded binaries
are SHA-256 verified before installation. Deno is downloaded as the official
Linux ZIP archive; Grab extracts only the expected executable after checking
the archive digest and enforcing compressed and uncompressed size limits.
Gallery-dl's digest is read from the release's `SHA256SUMS` attachment.

Grab does not download or update FFmpeg. Flatpak resolves `ffmpeg` and
`ffprobe` from `/app/bin` or the GNOME runtime (`/usr/bin`). Other builds
resolve an existing toolchain from system `PATH`. Legacy FFmpeg copies in
Grab's private tool directory are ignored.

## Digest source

For yt-dlp and Deno, the SHA-256 digest comes from the **same GitHub API
response** as the binary download URL (the release asset metadata). For
gallery-dl, Grab fetches the stable release metadata from Codeberg, accepts
only the expected `gallery-dl.bin` and `SHA256SUMS` assets for that exact
tag, then reads the binary digest from the checksum file. No binary hash is
hardcoded; the installer refuses the download if the checksum is absent,
malformed, or ambiguous.

## What this guards against

- Corrupted or truncated downloads (network errors, CDN issues)
- Swapped assets (wrong file served for the asset URL)

## What this does NOT guard against

- A compromised upstream release itself. If an attacker publishes a malicious
  binary and matching digest/checksum, verification will not detect it. The
  relevant GitHub release API and Codeberg release/checksum endpoints are the
  roots of trust.

## Fail-closed behavior

If the release metadata carries no SHA-256 digest, installation is refused
with an error — the previous binary (if any) is kept. No unverified binary
is ever installed.
