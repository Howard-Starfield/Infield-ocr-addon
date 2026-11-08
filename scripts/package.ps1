<#
.SYNOPSIS
  Build and package the Infield OCR add-on (the infield-ocr sidecar and its runtime). No signing.

.DESCRIPTION
  1. release build of crates/ocr-sidecar (the toolchain in rust-toolchain.toml, --locked);
  2. check DirectML.dll against its pin in config/sources.json, collect infield-ocr.exe, DirectML.dll
     and the sidecar's LICENSES/*;
  3. the accurate-mode runtime: the official ggml-org llama.cpp Windows Vulkan zip, whose SHA-256 must
     equal the pin in config/sources.json and the digest GitHub reports for that asset; only the files in
     config/ocr-vlm-allowlist.txt are taken from it (plus third_party/ocr-vlm-licenses/*), into the
     archive's ocr-vlm/ folder, tagged "component": "ocr-vlm" in the manifest;
  4. a deterministic zip (sorted entries, '/' names, every timestamp 1980-01-01) and
     manifest-<triple>.json, whose exact bytes are what gets signed later (scripts/sign.ps1);
  5. SHA256SUMS over the manifest and the zip.

  Output folder: manifest-<triple>.json, infield-ocr-<version>-<triple>.zip and SHA256SUMS. It has no
  signature: signing is a separate, local step (docs/RELEASE.md).

.PARAMETER OutDir
  Output folder. Default: <repo>/dist/<version> (gitignored).

.PARAMETER TargetDir
  Cargo target dir. Default: $env:CARGO_TARGET_DIR, else <repo>/target.

.PARAMETER SkipBuild
  Reuse the existing release build in TargetDir.

.PARAMETER MinAppVersion
  min_app_version written to the manifest. Default: release.min_app_version in config/sources.json.

.PARAMETER OcrRuntimeZip
  A local copy of the official llama.cpp zip instead of downloading it (still checked against the pin).

.PARAMETER SkipGithubDigest
  Offline only, and only with -OcrRuntimeZip: skip the lookup of GitHub's digest for the runtime zip.
  The pin is still checked. A downloaded runtime is always checked against GitHub's digest.
#>
[CmdletBinding()]
param(
    [string]$OutDir,
    [string]$TargetDir,
    [switch]$SkipBuild,
    [string]$MinAppVersion,
    [string]$OcrRuntimeZip,
    [switch]$SkipGithubDigest
)

$ErrorActionPreference = 'Stop'
$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$Crate = Join-Path $Repo 'crates/ocr-sidecar'
$Sources = Get-Content -Raw -LiteralPath (Join-Path $Repo 'config/sources.json') | ConvertFrom-Json
$Triple = $Sources.release.target

# Names in a fixed (ordinal) order, so the manifest and the zip never depend on the machine's culture.
function Sort-Ordinal([string[]]$Names) {
    $copy = [string[]]@($Names)
    [Array]::Sort($copy, [StringComparer]::Ordinal)
    return ,$copy
}

function Read-Match([string]$File, [string]$Pattern) {
    $m = [regex]::Match((Get-Content -Raw -LiteralPath $File), $Pattern, 'Multiline')
    if (-not $m.Success) { throw "no match for $Pattern in $File" }
    return $m.Groups[1].Value
}

$Version = Read-Match (Join-Path $Crate 'Cargo.toml') '^version\s*=\s*"([^"]+)"'
$Protocol = [int](Read-Match (Join-Path $Repo 'crates/ocr-protocol/src/lib.rs') 'PROTOCOL_VERSION:\s*u8\s*=\s*(\d+)')
if (-not $MinAppVersion) { $MinAppVersion = $Sources.release.min_app_version }
# The GitHub digest is only skippable for a local zip (offline work); a download is always checked.
if ($SkipGithubDigest -and -not $OcrRuntimeZip) { throw '-SkipGithubDigest is only for a local -OcrRuntimeZip; a downloaded runtime is always checked against GitHub''s digest' }
if (-not $TargetDir) { $TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Repo 'target' } }
if (-not $OutDir) { $OutDir = Join-Path $Repo "dist/$Version" }

. (Join-Path $PSScriptRoot 'ocr-runtime-pack.ps1')
$RuntimeBuild = $Sources.llama_cpp.build
$pin = Get-OcrRuntimePin $RuntimeBuild (Join-Path $Repo 'config/sources.json')

# Environment changes made below are undone at the end: nothing leaks into the caller's session.
$SavedEnv = @{}
foreach ($n in 'CARGO_TARGET_DIR', 'CARGO_INCREMENTAL') {
    $SavedEnv[$n] = [Environment]::GetEnvironmentVariable($n, 'Process')
}
try {

# 1. build
if (-not $SkipBuild) {
    $env:CARGO_TARGET_DIR = $TargetDir
    $env:CARGO_INCREMENTAL = '0'
    & cargo build --release --ignore-rust-version --locked --manifest-path (Join-Path $Crate 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
}

# 2. collect
$Release = Join-Path $TargetDir 'release'
$Exe = Join-Path $Release 'infield-ocr.exe'
$Dml = Join-Path $Release 'DirectML.dll'
foreach ($f in @($Exe, $Dml)) { if (-not (Test-Path -LiteralPath $f)) { throw "missing build output: $f" } }
# DirectML.dll is Microsoft's, copied next to the exe by the ONNX Runtime build: it must be the pinned one.
$dmlPin = $Sources.directml
# (The build may leave a link to a cached copy, so the size is read through a stream, which follows links.)
$dmlStream = [IO.File]::OpenRead($Dml)
try { $dmlSize = $dmlStream.Length } finally { $dmlStream.Dispose() }
if ($dmlSize -ne $dmlPin.size -or (Get-OcrFileSha256 $Dml) -ne $dmlPin.sha256) {
    throw "DirectML.dll is not the pinned $($dmlPin.nuget_id) $($dmlPin.version) file (config/sources.json)"
}

$Stage = Join-Path ([IO.Path]::GetTempPath()) ("infield-ocr-stage-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path (Join-Path $Stage 'LICENSES') | Out-Null
try {
    Copy-Item -LiteralPath $Exe -Destination $Stage
    # A real file in the stage, whatever $Dml is (a link or a copy).
    [IO.File]::Copy($Dml, (Join-Path $Stage 'DirectML.dll'), $true)
    Copy-Item -Path (Join-Path $Crate 'LICENSES/*') -Destination (Join-Path $Stage 'LICENSES')

    function Describe([string]$Rel, [string]$Kind, [string]$Component) {
        $p = Join-Path $Stage $Rel
        $entry = [ordered]@{
            path   = $Rel
            sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $p).Hash.ToLowerInvariant()
            size   = (Get-Item -LiteralPath $p).Length
            kind   = $Kind
        }
        # The sidecar's files carry no component key; only the runtime is tagged.
        if ($Component) { $entry['component'] = $Component }
        $entry
    }
    $files = @((Describe 'infield-ocr.exe' 'exe'), (Describe 'DirectML.dll' 'dll'))
    foreach ($name in (Sort-Ordinal (Get-ChildItem -LiteralPath (Join-Path $Stage 'LICENSES') -File | ForEach-Object { $_.Name }))) {
        $files += , (Describe ("LICENSES/" + $name) 'license')
    }

    # 3. the accurate-mode runtime, as the tagged component ocr-vlm/ (byte-identical to upstream)
    $runtimeZip = $OcrRuntimeZip
    $downloaded = $false
    if (-not $runtimeZip) {
        $runtimeZip = Join-Path ([IO.Path]::GetTempPath()) ("infield-ocr-runtime-" + [guid]::NewGuid().ToString('N') + '.zip')
        $downloaded = $true
        Invoke-WebRequest -UseBasicParsing -Uri $pin.url -OutFile $runtimeZip
    }
    try {
        $githubDigest = ''
        if (-not $SkipGithubDigest) { $githubDigest = Get-GithubAssetDigest $RuntimeBuild $pin.asset }
        Assert-OcrRuntimeZip $runtimeZip $pin.sha256 $githubDigest
        $vlmDir = Join-Path $Stage 'ocr-vlm'
        Expand-OcrRuntime $runtimeZip (Read-OcrRuntimeAllowlist (Join-Path $Repo $pin.allowlist)) $vlmDir
        Copy-Item -Path (Join-Path $Repo 'third_party/ocr-vlm-licenses/*') -Destination $vlmDir
    } finally {
        if ($downloaded) { Remove-Item -LiteralPath $runtimeZip -Force -ErrorAction SilentlyContinue }
    }
    # The server first, then the rest in name order (ordinal, not culture, so the order never varies).
    $vlmFiles = @(Get-ChildItem -LiteralPath $vlmDir -File | ForEach-Object { $_.Name })
    $vlmSorted = @($vlmFiles | Where-Object { $_ -eq 'llama-server.exe' }) + (Sort-Ordinal @($vlmFiles | Where-Object { $_ -ne 'llama-server.exe' }))
    foreach ($name in $vlmSorted) { $files += , (Describe ("ocr-vlm/" + $name) (Get-OcrRuntimeKind $name) 'ocr-vlm') }

    # 4. zip: entry names use '/', the order is the manifest's, and every entry's timestamp is fixed,
    # so the same inputs always give the same bytes.
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $ArchiveName = "infield-ocr-$Version-$Triple.zip"
    $Archive = Join-Path $OutDir $ArchiveName
    if (Test-Path -LiteralPath $Archive) { Remove-Item -LiteralPath $Archive }
    Add-Type -AssemblyName System.IO.Compression
    $fixedTime = [DateTimeOffset]::new(1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
    $out = [IO.File]::Create($Archive)
    try {
        $zip = New-Object IO.Compression.ZipArchive($out, [IO.Compression.ZipArchiveMode]::Create, $false)
        try {
            foreach ($f in $files) {
                $entry = $zip.CreateEntry($f.path, [IO.Compression.CompressionLevel]::Optimal)
                $entry.LastWriteTime = $fixedTime
                $dst = $entry.Open()
                try {
                    $src = [IO.File]::OpenRead((Join-Path $Stage $f.path))
                    try { $src.CopyTo($dst) } finally { $src.Dispose() }
                } finally { $dst.Dispose() }
            }
        } finally { $zip.Dispose() }
    } finally { $out.Dispose() }

    # manifest (exact bytes are what gets signed)
    $manifest = [ordered]@{
        schema          = 1
        addon           = 'ocr'
        version         = $Version
        protocol        = $Protocol
        min_app_version = $MinAppVersion
        target          = $Triple
        archive         = [ordered]@{
            name   = $ArchiveName
            sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash.ToLowerInvariant()
            size   = (Get-Item -LiteralPath $Archive).Length
        }
        files           = $files
    }
    $ManifestPath = Join-Path $OutDir "manifest-$Triple.json"
    $json = ($manifest | ConvertTo-Json -Depth 6) -replace "`r`n", "`n"
    [IO.File]::WriteAllText($ManifestPath, $json + "`n", (New-Object Text.UTF8Encoding($false)))

    # 5. SHA256SUMS (LF, sorted by name)
    $sums = foreach ($p in @($ManifestPath, $Archive) | Sort-Object { [IO.Path]::GetFileName($_) }) {
        "{0}  {1}" -f (Get-OcrFileSha256 $p), [IO.Path]::GetFileName($p)
    }
    [IO.File]::WriteAllText((Join-Path $OutDir 'SHA256SUMS'), (($sums -join "`n") + "`n"), (New-Object Text.UTF8Encoding($false)))
} finally {
    Remove-Item -LiteralPath $Stage -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host "Packaged OCR add-on $Version in $OutDir"
Get-ChildItem -LiteralPath $OutDir | ForEach-Object { Write-Host ("  {0}  {1:N0} bytes" -f $_.Name, $_.Length) }
Write-Host 'Next: scan the files, then sign the manifest with scripts/sign.ps1 (docs/RELEASE.md).'

} finally {
    foreach ($n in $SavedEnv.Keys) {
        [Environment]::SetEnvironmentVariable($n, $SavedEnv[$n], 'Process')
    }
}
