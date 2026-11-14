# Test fixture attributions

## HLS SAMPLE-AES MPEG-TS

- `hls-sample-aes/ac3.ts` is copied from [iori-rs/iori](https://github.com/iori-rs/iori/tree/master/crates/ssa/tests/fixtures/ac3), where its accompanying `script.sh` generates the fixture with FFmpeg and mp42hls.
- The source repository is licensed under Apache-2.0.
- The test-only 16-byte key and IV are stored as binary sidecar fixtures so secret scanners do not mistake public test vectors for credentials.

## HLS cue-marker metadata (inert)

- `hls-ad-marker-metadata.m3u8` is based on the AWS Elemental MediaTailor origin-manifest example: https://docs.aws.amazon.com/mediatailor/latest/ug/manifest-hls-example.html
- The fixture preserves the published `EXT-X-CUE-OUT`, `EXT-X-CUE-OUT-CONT`, and `EXT-X-CUE-IN` boundary/timing shape while replacing segment URIs with deterministic test names and SCTE-35 payloads with a non-secret placeholder. Source attribution is recorded here rather than as non-standard preamble text in the playlist.
- Cue markers are inert metadata: they do not remove or skip media segments. The `hls_cue_markers_do_not_remove_media_segments` integration test verifies that every segment, including the marked interval, is fetched and retained in the local playlist.
- `EXT-X-GAP` is a separate unavailable-segment marker; its conservative VOD handling is covered by the HLS gap-planning tests.
