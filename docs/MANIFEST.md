# Add-on manifest, schema 1

A release has one manifest per target, `manifest-<target>.json`, and its signature
`manifest-<target>.json.sig` (the Tauri signer's base64-wrapped minisign format; legacy,
non-prehashed signatures are refused). The signature covers the manifest file's exact bytes.
Infield downloads the manifest and signature first, verifies the signature with the public keys it
embeds, then the archive.

```json
{
  "schema": 1,
  "addon": "ocr",
  "version": "0.2.0",
  "protocol": 2,
  "min_app_version": "0.8.1",
  "target": "x86_64-pc-windows-msvc",
  "archive": { "name": "infield-ocr-0.2.0-x86_64-pc-windows-msvc.zip", "sha256": "<64 hex>", "size": 123 },
  "files": [
    { "path": "infield-ocr.exe", "sha256": "<64 hex>", "size": 123, "kind": "exe" },
    { "path": "DirectML.dll", "sha256": "<64 hex>", "size": 123, "kind": "dll" },
    { "path": "LICENSES/THIRD-PARTY.txt", "sha256": "<64 hex>", "size": 123, "kind": "license" },
    { "path": "ocr-vlm/llama-server.exe", "sha256": "<64 hex>", "size": 123, "kind": "exe", "component": "ocr-vlm" }
  ]
}
```

| Field | Meaning |
|---|---|
| `schema` | Always 1. |
| `addon` | Always `ocr`. |
| `version` | The add-on's semantic version; equals the sidecar crate's version and the git tag `ocr-addon-v<version>`. |
| `protocol` | The wire-protocol version the sidecar speaks (`PROTOCOL_VERSION` in `crates/ocr-protocol`). An app speaking another version refuses the add-on. |
| `min_app_version` | The oldest Infield build allowed to run this add-on. It is checked at install and at every launch. |
| `target` | A Rust target triple; the app refuses a manifest for another target. |
| `archive` | The zip's file name (a plain name), SHA-256 and size. |
| `files` | Every file in the archive: relative path (no `..`, no absolute path), SHA-256, size and `kind` (`exe`, `dll`, `license` or `data`). Paths are unique ignoring case. At most 512 entries. |

**Components.** A `files` entry without `component` belongs to the sidecar. The files under `ocr-vlm/`,
and only those, carry `"component": "ocr-vlm"`: the accurate-mode runtime (the official llama.cpp
server and its libraries, byte-identical to upstream). The app checks that the folder and the tag agree.

**Closed set.** After extraction the app refuses any file that the manifest does not list, and any file
whose hash or size differs.

**Rollback floor.** The app remembers the highest add-on version it has installed from a signed
release and refuses a lower one.

The manifest is produced by `scripts/package.ps1` (LF line endings, UTF-8 without a byte-order mark,
the exact bytes that are signed) and signed locally with `scripts/sign.ps1`.
