# Test fixture attributions

## HLS SAMPLE-AES MPEG-TS

- `hls-sample-aes/ac3.ts` is copied from [iori-rs/iori](https://github.com/iori-rs/iori/tree/master/crates/ssa/tests/fixtures/ac3), where its accompanying `script.sh` generates the fixture with FFmpeg and mp42hls.
- The source repository is licensed under Apache-2.0.
- The test-only 16-byte key and IV are stored as binary sidecar fixtures so secret scanners do not mistake public test vectors for credentials.
