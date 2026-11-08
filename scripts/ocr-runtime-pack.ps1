<#
.SYNOPSIS
  Helpers that package the accurate-mode llama.cpp runtime into the OCR add-on.

.DESCRIPTION
  Dot-sourced by scripts/package.ps1 and exercised by scripts/ocr-runtime-pack.test.ts. The runtime
  comes only from the official ggml-org release zip: its SHA-256 must equal the pin in
  config/sources.json AND the digest GitHub reports for that release asset. Only the files named in
  config/ocr-vlm-allowlist.txt are taken out of the zip, byte for byte, so each file's hash in the
  signed manifest is the upstream file's hash.
#>
Set-StrictMode -Version 2

function Get-OcrFileSha256([string]$Path) {
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

# The pinned release: asset name, URL, SHA-256 and size of the official zip (config/sources.json, key llama_cpp).
function Get-OcrRuntimePin([string]$Build, [string]$SourcesPath) {
    $sources = Get-Content -Raw -LiteralPath $SourcesPath | ConvertFrom-Json
    $pin = $sources.llama_cpp
    if (-not $pin -or $pin.build -ne $Build) { throw "no pin for llama.cpp build $Build in $SourcesPath" }
    if ($pin.sha256 -notmatch '^[0-9a-f]{64}$') { throw "pin for $Build has no valid sha256" }
    return $pin
}

# One file name per line; blank lines and # comments are skipped. Names are plain file names.
function Read-OcrRuntimeAllowlist([string]$Path) {
    $names = New-Object System.Collections.Generic.List[string]
    foreach ($line in Get-Content -LiteralPath $Path) {
        $t = $line.Trim()
        if ($t -eq '' -or $t.StartsWith('#')) { continue }
        if ($t -notmatch '^[A-Za-z0-9._-]+$') { throw "bad allowlist entry: $t" }
        if ($names.Contains($t)) { throw "duplicate allowlist entry: $t" }
        $names.Add($t)
    }
    if ($names.Count -eq 0) { throw "the allowlist is empty: $Path" }
    return , $names.ToArray()
}

# The digest GitHub reports for a release asset ("sha256:<hex>"), as bare hex.
function Get-GithubAssetDigest([string]$Build, [string]$AssetName) {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/ggml-org/llama.cpp/releases/tags/$Build" -Headers @{ 'User-Agent' = 'infield-ocr-addon-build' }
    $asset = $release.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
    if (-not $asset) { throw "release $Build has no asset $AssetName" }
    if ($asset.digest -notmatch '^sha256:([0-9a-fA-F]{64})$') { throw "GitHub reports no sha256 digest for $AssetName" }
    return $Matches[1].ToLowerInvariant()
}

# Refuse a zip whose SHA-256 is not the pinned one, or not the one GitHub reports (when given).
function Assert-OcrRuntimeZip([string]$ZipPath, [string]$PinnedSha256, [string]$GithubSha256) {
    $actual = Get-OcrFileSha256 $ZipPath
    if ($actual -ne $PinnedSha256.ToLowerInvariant()) {
        throw "runtime zip SHA-256 mismatch: the file is $actual, the pin is $PinnedSha256"
    }
    if ($GithubSha256 -and $actual -ne $GithubSha256.ToLowerInvariant()) {
        throw "runtime zip SHA-256 mismatch: the file is $actual, GitHub reports $GithubSha256"
    }
}

# Extract exactly the allowlisted root files from the zip into DestDir; nothing else is written.
function Expand-OcrRuntime([string]$ZipPath, [string[]]$Allowlist, [string]$DestDir) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($ZipPath)
    try {
        $byName = @{}
        foreach ($entry in $zip.Entries) {
            if ($entry.Name -ne '') { $byName[$entry.FullName] = $entry }
        }
        New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
        foreach ($name in $Allowlist) {
            if (-not $byName.ContainsKey($name)) { throw "the runtime zip has no $name" }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($byName[$name], (Join-Path $DestDir $name), $true)
        }
    } finally { $zip.Dispose() }
}

# The manifest kind of a runtime file.
function Get-OcrRuntimeKind([string]$Name) {
    if ($Name.EndsWith('.exe')) { return 'exe' }
    if ($Name.EndsWith('.dll')) { return 'dll' }
    return 'license'
}
