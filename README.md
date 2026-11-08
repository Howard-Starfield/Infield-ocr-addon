# Infield OCR add-on

The OCR sidecar and its accurate-mode runtime for Infield,
packaged as an optional, signed add-on.

- `crates/ocr-sidecar`: `infield-ocr`, a sidecar that renders PDFs, checks the text layer, and runs
  layout analysis, OCR and table recognition over a framed stdio protocol. It has no network code.
  Models are **not** included: the host app downloads them at the user's request.
- `crates/ocr-protocol`: the wire protocol shared with the app (a pinned git dependency of Infield).
- A release archive adds Microsoft's `DirectML.dll` and, for accurate reading, the unmodified official
  llama.cpp Vulkan server in `ocr-vlm/`, all listed in a signed manifest (`docs/MANIFEST.md`).

## What a release proves

- Every input is pinned: crate versions by `Cargo.lock` (`--locked`), the llama.cpp zip and
  `DirectML.dll` by SHA-256 in `config/sources.json`.
- The archive is deterministic for given inputs (sorted entries, fixed timestamps).
- The manifest is signed with a minisign key (the Tauri signer format). Infield embeds the public
  keys (`keys/*.pub`) and refuses anything else.
- The release assets carry `SHA256SUMS` and a GitHub build-provenance attestation.

## Verify a release

```text
gh attestation verify <file> --repo Howard-Starfield/Infield-ocr-addon
sha256sum -c SHA256SUMS
```

## Layout

```text
crates/ocr-protocol      wire protocol (library)
crates/ocr-sidecar       the sidecar executable and its tests
config/                  upstream pins (sources.json) and the runtime file allowlist
scripts/                 package.ps1, sign.ps1, check-repo.ps1, scan-package.ps1, runtime helpers
third_party/             licences shipped with the runtime
keys/                    public signing keys (see keys/README.md)
docs/                    MANIFEST.md (the manifest format), RELEASE.md (the release runbook)
.github/workflows/       ci.yml (guard, test, deny) and candidate.yml (build + provenance)
```

## Local checks

```text
pwsh scripts/check-repo.ps1
cargo test --locked --manifest-path crates/ocr-protocol/Cargo.toml
cargo test --locked --ignore-rust-version --manifest-path crates/ocr-sidecar/Cargo.toml
bun test scripts/ocr-runtime-pack.test.ts
cargo deny --manifest-path crates/ocr-sidecar/Cargo.toml check bans licenses sources
```

The sidecar builds with the toolchain in
`rust-toolchain.toml` and `--ignore-rust-version` (its dependency `oar-ocr` declares a newer minimum than the code needs).

## Licence

MIT for the code in this repository (`LICENSE`). Release archives contain third-party binaries under
their own terms: see `NOTICE`.
