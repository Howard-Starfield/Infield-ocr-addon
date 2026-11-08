<#
.SYNOPSIS
  Generate the add-on signing key pair, or sign a manifest. For local use by the maintainer only.

.DESCRIPTION
  Runs exactly `bunx @tauri-apps/cli@2.10.1 signer ...`. The version is pinned on purpose: outside a
  JavaScript project the CLI's short name resolves to a different npm package, and a newer
  CLI may change the signature format. The six TAURI signing environment variables are cleared first, so
  a key or password meant for another purpose (for example the app updater's) is never picked up.

  The private key stays outside every repository. Only the .pub file and the .sig file are ever shared.

  Modes:
    -Generate  create a key pair: -KeyFile is the private key to write, <KeyFile>.pub the public key.
               The key is encrypted; the CLI prompts for its password (or use -NoPassword for a throwaway key).
    (default)  sign -File with the private key in -KeyFile. Writes <File>.sig next to it.

  The key password goes through the TAURI_SIGNING_PRIVATE_KEY_PASSWORD variable of the child process, or a
  prompt; it is never put on a command line (a command line is visible to other processes).

.PARAMETER KeyFile
  The private key file (to read, or with -Generate to write). Not inside a repository.

.PARAMETER File
  The manifest to sign (manifest-<triple>.json).

.PARAMETER Generate
  Create a key pair instead of signing.

.PARAMETER NoPassword
  The key has no password (a throwaway key): do not prompt.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$KeyFile,
    [string]$File,
    [switch]$Generate,
    [switch]$NoPassword
)

$ErrorActionPreference = 'Stop'
$Cli = '@tauri-apps/cli@2.10.1'

# Whatever the caller's session holds, none of these reaches the signer; the password is read first.
$sessionPassword = $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD
foreach ($name in 'TAURI_SIGNING_PRIVATE_KEY', 'TAURI_SIGNING_PRIVATE_KEY_PATH', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD',
                  'TAURI_PRIVATE_KEY', 'TAURI_PRIVATE_KEY_PATH', 'TAURI_PRIVATE_KEY_PASSWORD') {
    Remove-Item -Path "Env:\$name" -ErrorAction SilentlyContinue
}

# A key inside a git work tree would be one `git add` from a leak.
function Assert-OutsideRepo([string]$Path) {
    $dir = Split-Path -Parent ([IO.Path]::GetFullPath($Path))
    while ($dir) {
        if (Test-Path -LiteralPath (Join-Path $dir '.git')) { throw "refusing to use a key inside a git work tree: $Path" }
        $parent = Split-Path -Parent $dir
        if ($parent -eq $dir) { break }
        $dir = $parent
    }
}

# An empty -p is passed as '""' by Windows PowerShell 5.1 (it drops an empty native argument).
$empty = if ($PSVersionTable.PSVersion.Major -lt 7) { '""' } else { '' }

try {
    if ($Generate) {
        Assert-OutsideRepo $KeyFile
        if ((Test-Path -LiteralPath $KeyFile) -or (Test-Path -LiteralPath "$KeyFile.pub")) { throw "refusing to overwrite an existing key: $KeyFile" }
        if ($NoPassword) {
            & bunx $Cli signer generate --ci -w $KeyFile -p $empty
        } else {
            & bunx $Cli signer generate -w $KeyFile
        }
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath "$KeyFile.pub")) { throw 'signer generate failed' }
        Write-Host "Public key (share this): $KeyFile.pub"
        Write-Host 'Private key: keep it outside every repository, and keep a backup offline.'
        return
    }

    if (-not $File) { throw '-File (the manifest to sign) is required' }
    Assert-OutsideRepo $KeyFile
    if (-not (Test-Path -LiteralPath $KeyFile)) { throw "key file not found: $KeyFile" }
    if (-not (Test-Path -LiteralPath $File)) { throw "file not found: $File" }

    $pw = $sessionPassword
    if ($NoPassword) { $pw = '' }
    if ($null -eq $pw) {
        $sec = Read-Host -AsSecureString 'Add-on key password (Enter for none)'
        $pw = [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec))
    }
    $SigPath = "$File.sig"
    if (Test-Path -LiteralPath $SigPath) { Remove-Item -LiteralPath $SigPath }
    if ($pw) {
        $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $pw
        & bunx $Cli signer sign -f $KeyFile $File
    } else {
        & bunx $Cli signer sign -f $KeyFile -p $empty $File
    }
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $SigPath)) { throw 'signer sign failed' }
    Write-Host "Signed: $SigPath"
} finally {
    Remove-Item -Path 'Env:\TAURI_SIGNING_PRIVATE_KEY_PASSWORD' -ErrorAction SilentlyContinue
}
