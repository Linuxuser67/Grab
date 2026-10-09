# Tool install trust model

Grab can install `yt-dlp`, `ffmpeg`/`ffprobe` and `qjs` (quickjs-ng) into the
user library directory. What that does and does not protect against:

| Tool | Source | Version | Integrity check |
|------|--------|---------|-----------------|
| yt-dlp | `yt-dlp/yt-dlp` GitHub release | latest (must float: extractors break weekly) | SHA-256 from the release API, fail closed when absent |
| ffmpeg, ffprobe | `boul2gom/ffmpeg-builds` GitHub release (third-party static builds) | latest | SHA-256 from the release API, fail closed when absent |
| qjs | `quickjs-ng/quickjs` GitHub release | latest | SHA-256 from the release API, fail closed when absent |

- The digest and the asset come from the same GitHub release. The check catches
  corruption and a swapped asset; it does **not** protect against a compromised
  release or maintainer account, because both would change together.
- The ffmpeg builds come from a single third-party maintainer, unpinned.
- Downloads are size-capped while streaming, written with `create_new`, refuse
  symlinked destinations, and are executable only after the digest matches.
  Archives must contain exactly one `ffmpeg` and at most one `ffprobe`.
- Tool installs and update probes use clients that cannot apply the proxy set in
  Grab (the `yt-dlp` crate's fetchers have no proxy option; only env proxies
  apply), so they are refused while a proxy is configured. Install the tools
  manually in that case.

Pinning (a version + SHA-256 committed to the repo, as CI does for `sentrux`,
`gitleaks` and `cargo-audit`) would close the compromised-release gap for
ffmpeg and qjs at the cost of a manual bump per release. That is a maintainer
decision and is not implemented.
