# Public signing keys

`ocr-addon-active.pub` and `ocr-addon-backup.pub` are the **public** halves of the two keys that sign
the add-on's manifest. Infield embeds the same two public keys and refuses a manifest that neither of
them signed. The private halves never leave the maintainer's machine and are never committed.

## Status: PLACEHOLDERS

Until the maintainer generates the real keys, both files contain the single line `PLACEHOLDER`,
exactly like the placeholder constants in the Infield app (an app with placeholder keys refuses every
manifest). Replace each file's content with the **base64 text** of the matching `.pub` file produced by
`scripts/sign.ps1 -Generate`.

## Format

The Tauri signer's public key file is one base64 line: the encoding of a minisign public key
(`untrusted comment: minisign public key: <KEYID>` followed by the key). The same text goes into
Infield's `ocr_addon_keys.rs`. The key id is inside the decoded text.

## Generate (maintainer, once)

```text
pwsh scripts/sign.ps1 -Generate -KeyFile <folder outside every repository>/infield-ocr-addon-active.key
pwsh scripts/sign.ps1 -Generate -KeyFile <folder outside every repository>/infield-ocr-addon-backup.key
```

Use a password. Keep the backup key offline. Share only the two `.pub` files. `sign.ps1` refuses a key
path inside a git work tree.
