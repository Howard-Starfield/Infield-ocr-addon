# Security policy

## What to report

Please report privately (GitHub: Security > Report a vulnerability) anything that could let a
release of this add-on run code its signer did not intend:

- a compromised or leaked signing key, or a way to produce a valid manifest signature;
- a bypass of the manifest checks (hashes, sizes, closed file set, component rules);
- archive path traversal or an unsafe path in a manifest;
- a bad or tampered upstream pin in `config/sources.json`;
- anything in a release that leaks private data of the build machine.

Please do not open a public issue for these.

## What is supported

Only releases whose manifest is signed by a key that an Infield build embeds are supported. A
release's integrity is anchored in that signature: the archive's exact bytes are listed in the
signed manifest. Releases also carry `SHA256SUMS` and a GitHub build-provenance attestation
(see the README for how to verify them).

## Scope

The sidecar has no network code and only reads the document and model paths the host app gives it.
Report anything that makes it connect out, execute other programs, or write outside its output
folder.
