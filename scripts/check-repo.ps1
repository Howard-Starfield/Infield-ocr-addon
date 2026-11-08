<#
.SYNOPSIS
  The repository guard: source only, no keys, no private paths, no risky workflow triggers.

.DESCRIPTION
  Run by CI (the `guard` job) and by hand before every push. Checks every file git tracks or would add:
    - no binary or build output (by extension, and any file with a NUL byte);
    - no key material: only keys/README.md and keys/*.pub may live in keys/; no *.key / *.pem / *.pfx /
      *.p12 files; no text that starts like a Tauri (minisign) encrypted private key;
    - no private-looking paths: a drive letter path, \Users\ or /Users/ paths, .claude, docs/plans,
      docs/superpowers (LICENSES/ and the two guard scripts are exempt from the content checks);
    - no `pull_request_target` or `workflow_run` trigger in .github/workflows (they run with secrets
      or write tokens on untrusted input).
  Exit code 1 and a list of violations on failure.

  Private strings (user names, local folder names, document names) are NOT listed here: this repository is
  public. A maintainer-local denylist is run over the tree and the git history before every push
  (docs/RELEASE.md).
#>
[CmdletBinding()]
param([string]$Root)

$ErrorActionPreference = 'Stop'
if (-not $Root) { $Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path }
Set-Location -LiteralPath $Root

$violations = New-Object System.Collections.Generic.List[string]
function Fail([string]$Message) { $violations.Add($Message) }

# Tracked files, plus untracked ones that are not ignored (so it also works before the first commit).
$files = @(& git ls-files -co --exclude-standard) | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) }
if ($LASTEXITCODE -ne 0) { throw 'git ls-files failed' }

$binaryExtensions = '.exe', '.dll', '.zip', '.7z', '.gz', '.tgz', '.tar', '.onnx', '.gguf', '.pdb', '.lib', '.obj', '.rlib',
                    '.so', '.dylib', '.bin', '.msi', '.nupkg', '.crate', '.pdf', '.png', '.jpg', '.jpeg', '.ico'
$keyExtensions = '.key', '.pem', '.pfx', '.p12', '.jks'

# Built from pieces so this file does not contain the strings it looks for.
$privateKeyMarkers = @(
    ('dW50cnVzdGVk' + 'IGNvbW1lbnQ6IHJzaWduIGVuY3J5cHRlZCBzZWNyZXQga2V5'),
    ('rsign encrypted' + ' secret key')
)
$pathPatterns = [ordered]@{
    'a drive-letter path'   = '(?<![A-Za-z0-9])[A-Za-z]:[\\/]'
    'a \Users\ path'        = '\\Users\\'
    'a /Users/ path'        = '(?<![A-Za-z0-9.])/Users/'
    'a .claude folder'      = '\.claude'
    'a docs/plans path'     = 'docs/plans'
    'a docs/superpowers path' = 'docs/superpowers'
}

foreach ($f in $files) {
    $norm = $f -replace '\\', '/'
    $ext = [IO.Path]::GetExtension($norm).ToLowerInvariant()
    if ($binaryExtensions -contains $ext) { Fail "binary or build output tracked: $norm" ; continue }
    if ($keyExtensions -contains $ext) { Fail "key file tracked: $norm" ; continue }
    if ($norm -like 'keys/*' -and $norm -ne 'keys/README.md' -and -not $norm.EndsWith('.pub')) { Fail "only keys/README.md and keys/*.pub may live in keys/: $norm" }
    if ([IO.Path]::GetFileName($norm) -match '^id_(rsa|ed25519|ecdsa)') { Fail "key file tracked: $norm" ; continue }

    $bytes = [IO.File]::ReadAllBytes((Join-Path $Root $f))
    if ($bytes -contains 0) { Fail "binary content (a NUL byte): $norm" ; continue }
    $text = [Text.Encoding]::UTF8.GetString($bytes)

    # Key material is refused everywhere, the guard's own markers included (they are built from pieces).
    foreach ($m in $privateKeyMarkers) {
        if ($text.Contains($m)) { Fail "looks like a private key: $norm" }
    }
    if ($norm -like 'LICENSES/*' -or $norm -like '*/LICENSES/*' -or $norm -eq 'scripts/check-repo.ps1' -or $norm -eq 'scripts/scan-package.ps1') { continue }
    foreach ($name in $pathPatterns.Keys) {
        if ([regex]::IsMatch($text, $pathPatterns[$name])) { Fail "$name in $norm" }
    }
    if ($norm -like '.github/workflows/*') {
        foreach ($trigger in 'pull_request_target', 'workflow_run') {
            if ([regex]::IsMatch($text, "(?m)^\s*$trigger\s*:|\b$trigger\b")) { Fail "workflow trigger $trigger is not allowed: $norm" }
        }
    }
}

if ($violations.Count -gt 0) {
    Write-Host 'check-repo: FAILED' -ForegroundColor Red
    $violations | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "check-repo: ok ($($files.Count) files)"
