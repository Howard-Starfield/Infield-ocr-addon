# Contributing

This repository holds the source and the release tooling of the Infield OCR add-on. It is small on
purpose.

- **No binaries, keys or release output in git.** Models, executables, archives and `*.sig` files
  never belong here. Only `keys/*.pub` (public keys) are tracked.
- **A pin change** (`config/sources.json`, `config/ocr-vlm-allowlist.txt`) names the upstream tag,
  where the checksum comes from, and the licence impact in the pull request.
- **A protocol change** (`crates/ocr-protocol`) is a pull request here first. The Infield app then
  bumps the pinned git revision of `infield-ocr-protocol`. The protocol is versioned
  (`PROTOCOL_VERSION`); additive changes keep the version, anything else raises it.
- **Before opening a pull request**, run:
  - `scripts/check-repo.ps1` (the source-only and privacy guard);
  - `cargo test --locked` in `crates/ocr-protocol`, and `cargo test --locked --ignore-rust-version`
    in `crates/ocr-sidecar` (the model-dependent tests are `#[ignore]`);
  - `bun test scripts/ocr-runtime-pack.test.ts`;
  - `cargo deny --manifest-path crates/ocr-sidecar/Cargo.toml check bans licenses sources`.
- Releases are built by the `candidate` workflow and published by the maintainer by hand
  (`docs/RELEASE.md`).
