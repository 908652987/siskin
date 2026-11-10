<#
.SYNOPSIS
    Builds the Windows UI spike and signs the executable.

.DESCRIPTION
    Windows warns about unsigned executables launched from Explorer. Cargo replaces
    the exe on every build, so this wrapper signs it after the build. Run
    setup-dev-signing.ps1 once per user account first.

.PARAMETER Profile
    debug or release. Default: release.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File spike/scripts/build-windows-ui.ps1
#>
[CmdletBinding()]
param(
    [ValidateSet('debug', 'release')][string]$Profile = 'release'
)

$ErrorActionPreference = 'Stop'

$signingCertificate = Get-ChildItem Cert:\CurrentUser\My -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq 'CN=Siskin Dev' -and $_.NotAfter -gt (Get-Date) } |
    Select-Object -First 1
if ($null -eq $signingCertificate) {
    Write-Host 'First build: preparing the local signing certificate for this machine...'
    & (Join-Path $PSScriptRoot 'setup-dev-signing.ps1')
}

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if ($null -eq $cargo) {
    $candidate = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (-not (Test-Path $candidate)) {
        throw 'cargo not found; install Rust first.'
    }
    $cargoPath = $candidate
} else {
    $cargoPath = $cargo.Source
}

$arguments = @('build', '-p', 'siskin-windows-spike')
if ($Profile -eq 'release') {
    $arguments += '--release'
}

Push-Location $repoRoot
try {
    & $cargoPath @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    $exe = Join-Path $repoRoot ('target\{0}\siskin-windows-spike.exe' -f $Profile)
    & (Join-Path $PSScriptRoot 'sign-windows-exe.ps1') -Path $exe
} finally {
    Pop-Location
}
