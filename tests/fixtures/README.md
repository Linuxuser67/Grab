# Test fixture attributions

## HLS SAMPLE-AES MPEG-TS

- `hls-sample-aes/ac3.ts` is copied from [iori-rs/iori](https://github.com/iori-rs/iori/tree/master/crates/ssa/tests/fixtures/ac3), where its accompanying `script.sh` generates the fixture with FFmpeg and mp42hls.
- The source repository is licensed under Apache-2.0.
- The test-only 16-byte key and IV are stored as binary sidecar fixtures so secret scanners do not mistake public test vectors for credentials.

## HLS ad-marker metadata

- `hls-ad-marker-metadata.m3u8` is based on the AWS Elemental MediaTailor origin-manifest example for `EXT-OATCLS-SCTE35`, `EXT-X-CUE-OUT`, `EXT-X-CUE-OUT-CONT`, and `EXT-X-CUE-IN`: https://docs.aws.amazon.com/mediatailor/latest/ug/manifest-hls-example.html
- Its `EXT-X-ASSET` and `EXT-X-CUE` markers follow the AWS Elemental MediaLive examples: https://docs.aws.amazon.com/medialive/latest/ug/sample-manifests-hls.html
- Segment URIs are replaced with deterministic fixture names while preserving the published marker syntax and order. The fixture checks that marker removal leaves all 12 listed segment resources unchanged.
