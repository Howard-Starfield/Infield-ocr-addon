# Release runbook (maintainer, local)

A release is built by CI, checked and signed by hand. The CI build holds no secret and publishes
nothing. The signing key never leaves the maintainer's machine. Never paste a private key anywhere:
only `.pub` files and `.sig` files travel.

Use three separate folders, all **outside every git repository**:

| Folder | Holds |
|---|---|
| `candidate/` | the downloaded artifact, untouched |
| `u3-dev/` | a copy plus a `.sig` from a throwaway key, for the pre-release test in a debug Infield |
| `release/` | a copy plus the **real** `.sig`, which is what gets published |

Both `.sig` files have the same name; keep them in their folders.

## 0. One-time: keys and the private denylist

1. Generate the active and backup keys with `scripts/sign.ps1 -Generate` (see `keys/README.md`). Keep the
   backup offline. Put the two `.pub` contents in `keys/` and in Infield's `ocr_addon_keys.rs`.
2. Keep a **private denylist** (one regular expression per line: user names, local folder names,
   document names, private paths) in a file outside the repository. It is never committed. It is run
   over the tree, over `git log -p`, and over the packaged files before every push and every signing.

## 1. Candidate

1. Make sure `main` is the commit to release and `crates/ocr-sidecar/Cargo.toml` has the version.
2. Run the `candidate` workflow on `main` with that version. It guards, tests, checks the dependency
   policy (advisories blocking), builds with `--locked`, packages, scans, uploads the artifact
   `ocr-addon-candidate-<version>` (30 days) and attests the checksums.
3. Download it and verify:

```text
gh run download <run-id> --repo Howard-Starfield/Infield-ocr-addon -n ocr-addon-candidate-<version> -D candidate
gh attestation verify candidate/<file> --repo Howard-Starfield/Infield-ocr-addon     # each file
sha256sum -c candidate/SHA256SUMS
```

4. Scan the package with the private denylist (no `.pdb`, no PDB path, no user-profile path, no denylist hit):

```text
pwsh scripts/scan-package.ps1 -Zip candidate/infield-ocr-<version>-x86_64-pc-windows-msvc.zip `
  -Extra candidate/manifest-x86_64-pc-windows-msvc.json -Denylist <private-denylist.txt>
```

## 2. Test the exact candidate bytes (before anything is published)

1. Copy `candidate/` to `u3-dev/` and sign **that copy's manifest with a throwaway key**
   (`sign.ps1 -Generate -NoPassword`, then `sign.ps1 -NoPassword`); delete the throwaway private key
   afterwards.
2. In a debug Infield, set `INFIELD_OCR_ADDON_SOURCE=<absolute u3-dev folder>` and
   `INFIELD_OCR_ADDON_DEV_PUBKEY=<the throwaway .pub text>` on **every** launch (the app re-checks the
   installed manifest at each launch with the keys it trusts), and run the accuracy and soak checks.
3. Debug and release builds share one data directory: uninstall that dev install (with the same
   variables set) before testing a release build.
4. If anything fails: fix, run a new candidate (same version while nothing is published), repeat.

## 3. Tag, sign, test the real signature

1. `git tag ocr-addon-v<version> <the run's head commit>` and push the tag. (Lightweight tag.)
2. Copy `candidate/` to `release/` and sign its manifest with the **real active key**:

```text
pwsh scripts/sign.ps1 -KeyFile <outside-repo>/infield-ocr-addon-active.key -File release/manifest-x86_64-pc-windows-msvc.json
```

3. Before publishing, run Infield's ignored candidate-install test against `release/` with the real
   `.sig`, `OCR_ADDON_CANDIDATE_PUBKEY=<active.pub text>` and the expected version. A published release
   cannot be changed, so a failure found after step 4 means a new patch version.

## 4. Publish (as a draft first)

```text
gh release create ocr-addon-v<version> --repo Howard-Starfield/Infield-ocr-addon --draft --verify-tag `
  --title "OCR add-on <version>" --notes-file <notes.md> `
  release/manifest-x86_64-pc-windows-msvc.json release/manifest-x86_64-pc-windows-msvc.json.sig `
  release/infield-ocr-<version>-x86_64-pc-windows-msvc.zip release/SHA256SUMS
```

1. Re-download the four assets from the draft and compare them byte for byte with `release/`.
2. Only then: `gh release edit ocr-addon-v<version> --repo Howard-Starfield/Infield-ocr-addon --draft=false`.

A draft that fails halfway can be deleted and redone. A published release (with immutable releases on)
keeps its tag and assets for good.

## 5. After publishing

Bump `OCR_ADDON_VERSION` and the embedded public keys in Infield, add the published manifest and `.sig`
as test data there, and run its ignored real-network install once before merging.
