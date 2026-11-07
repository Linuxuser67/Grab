# Test fixture attributions

## HLS SAMPLE-AES MPEG-TS

- `hls-sample-aes/ac3.ts` is copied from [iori-rs/iori](https://github.com/iori-rs/iori/tree/master/crates/ssa/tests/fixtures/ac3), where its accompanying `script.sh` generates the fixture with FFmpeg and mp42hls.
- The source repository is licensed under Apache-2.0.
- The test-only 16-byte key and IV are stored as binary sidecar fixtures so secret scanners do not mistake public test vectors for credentials.

## HLS ad-break removal

- `hls-ad-marker-metadata.m3u8` is based on the AWS Elemental MediaTailor origin-manifest example: https://docs.aws.amazon.com/mediatailor/latest/ug/manifest-hls-example.html
- The fixture begins with `#EXTM3U` as required by the parser. It preserves the published `EXT-X-CUE-OUT`, `EXT-X-CUE-OUT-CONT`, and `EXT-X-CUE-IN` boundary/timing shape while replacing segment URIs with deterministic test names and SCTE-35 payloads with a non-secret placeholder. Source attribution is recorded here rather than as non-standard preamble text in the playlist.
- Only the five segments inside the explicit ad break are omitted; all seven program segments retain their original source sequence-derived staging identities.
- The resource-fetch integration test verifies ad segment URLs are absent from the fetch plan, no ad request is served, and the local FFmpeg playlist contains only program segments.
