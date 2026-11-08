<#
.SYNOPSIS
  Scan a packaged add-on (the zip, plus the manifest) for build-machine leaks.

.DESCRIPTION
  Extracts the zip to a temporary folder and checks every file's bytes (ASCII and UTF-16) for:
    - a .pdb file in the archive;
    - a Windows user-profile path (<drive>:\Users\...) other than the CI runner's own `runneradmin`
      (every file);
    - a CodeView record (RSDS) whose PDB path has a directory part (this repository's own files only: a bare
      file name, or a path inside a third-party binary, is reported as a note, not a failure);
    - any match of the optional private denylist (-Denylist, one regular expression per line, # comments).
      A line starting with `all:` applies to every file; a line starting with `tree:` is for the source-tree
      scan only and is skipped here. Other lines apply only to this repository's own files (-OwnFiles and
      -Extra): third-party binaries (DirectML, the llama.cpp runtime) legitimately contain words and paths
      of their own.
  Exit code 1 and the findings on failure. It prints counts and file names only, never document text.

  CI runs it without a denylist. The maintainer runs it with the private, local-only denylist before signing
  (docs/RELEASE.md).

.PARAMETER Zip
  The packaged archive (infield-ocr-<version>-<triple>.zip).

.PARAMETER Denylist
  A local file of private regular expressions, kept out of this repository.

.PARAMETER Extra
  More files to scan as they are (for example the manifest). They count as own files.

.PARAMETER OwnFiles
  Archive entries built by this repository (default: infield-ocr.exe).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Zip,
    [string]$Denylist,
    [string[]]$Extra = @(),
    [string[]]$OwnFiles = @('infield-ocr.exe')
)

$ErrorActionPreference = 'Stop'
$latin1 = [Text.Encoding]::GetEncoding(28591)
$findings = New-Object System.Collections.Generic.List[string]
$notes = New-Object System.Collections.Generic.List[string]

$denyAll = @()
$denyOwn = @()
if ($Denylist) {
    $lines = Get-Content -LiteralPath $Denylist | ForEach-Object { $_.Trim() } | Where-Object { $_ -and -not $_.StartsWith('#') }
    foreach ($line in $lines) {
        if ($line.StartsWith('tree:')) { continue }
        if ($line.StartsWith('all:')) { $denyAll += $line.Substring(4) } else { $denyOwn += $line }
    }
}

# A literal-character pattern as UTF-16LE (each character followed by a NUL); a regex with groups or
# classes is matched as ASCII only.
function ConvertTo-Utf16Pattern([string]$Pattern) {
    if ($Pattern -match '[\\\[\]\(\)\|\*\+\?\{\}\^\$]') { return $null }
    return (($Pattern.ToCharArray() | ForEach-Object { [regex]::Escape([string]$_) + '\x00' }) -join '')
}

function Test-Bytes([string]$Name, [byte[]]$Bytes, [bool]$Own) {
    $text = $latin1.GetString($Bytes)
    $patterns = @($denyAll)
    if ($Own) { $patterns += @($denyOwn) }

    # 1. user-profile paths (ASCII and UTF-16)
    $profile = [regex]'(?i)[A-Z]:\\Users\\([^\\\x00\r\n"'']{1,64})'
    foreach ($m in $profile.Matches($text)) {
        $user = $m.Groups[1].Value
        if ($user -ine 'runneradmin') { $findings.Add("${Name}: a user-profile path (user '$user')") }
    }
    $profile16 = [regex]'(?i)[A-Z]\x00:\x00\\\x00U\x00s\x00e\x00r\x00s\x00\\\x00((?:[^\\\x00]\x00){1,64})'
    foreach ($m in $profile16.Matches($text)) {
        $user = ($m.Groups[1].Value -replace '\x00', '')
        if ($user -ine 'runneradmin') { $findings.Add("${Name}: a user-profile path in UTF-16 (user '$user')") }
    }

    # 2. CodeView records: RSDS, 16-byte GUID, 4-byte age, then the NUL-terminated PDB path
    foreach ($m in [regex]::Matches($text, 'RSDS[\x00-\xff]{20}([\x20-\x7e]{1,260})\x00')) {
        $path = $m.Groups[1].Value
        if ($path -match '^[A-Za-z]:|[\\/]') {
            if ($Own) { $findings.Add("${Name}: a debug record names a PDB path with a directory ($path)") }
            else { $notes.Add("${Name}: third-party debug record, PDB path as shipped upstream ($path)") }
        } else {
            $notes.Add("${Name}: a debug record names the PDB file '$path' (no directory)")
        }
    }

    # 3. the private denylist
    $n = 0
    foreach ($pattern in $patterns) {
        $n++
        if ([regex]::IsMatch($text, $pattern, 'IgnoreCase')) { $findings.Add("${Name}: matches the private denylist (pattern '$pattern')") }
        $wide = ConvertTo-Utf16Pattern $pattern
        if ($wide -and [regex]::IsMatch($text, $wide, 'IgnoreCase')) { $findings.Add("${Name}: matches the private denylist in UTF-16 (pattern '$pattern')") }
    }
}

$count = 0
$work = Join-Path ([IO.Path]::GetTempPath()) ('infield-ocr-scan-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($Zip)
    try {
        foreach ($entry in $archive.Entries) {
            if ($entry.Name -eq '') { continue }
            $count++
            if ($entry.FullName -match '(?i)\.pdb$') { $findings.Add("$($entry.FullName): a .pdb file is in the archive") }
            $dest = Join-Path $work ([guid]::NewGuid().ToString('N'))
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $dest)
            Test-Bytes $entry.FullName ([IO.File]::ReadAllBytes($dest)) ($OwnFiles -contains $entry.FullName)
            Remove-Item -LiteralPath $dest -Force
        }
    } finally { $archive.Dispose() }
    foreach ($path in $Extra) { Test-Bytes ([IO.Path]::GetFileName($path)) ([IO.File]::ReadAllBytes($path)) $true }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

foreach ($n in ($notes | Sort-Object -Unique)) { Write-Host "note: $n" }
if ($findings.Count -gt 0) {
    Write-Host 'scan-package: FAILED' -ForegroundColor Red
    $findings | Sort-Object -Unique | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "scan-package: ok ($count archive files + $($Extra.Count) extra; denylist: $($denyAll.Count) patterns for every file + $($denyOwn.Count) for own files)"
