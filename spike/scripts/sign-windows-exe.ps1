<#
.SYNOPSIS
    Signs a Windows executable with the local dev code-signing certificate.

.DESCRIPTION
    Uses the newest signtool.exe in the Windows Kits and the certificate created by
    setup-dev-signing.ps1. Fails if the signature is not valid afterwards.

.PARAMETER Path
    Executable to sign in place.

.PARAMETER Subject
    Certificate subject; must match setup-dev-signing.ps1.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File spike/scripts/sign-windows-exe.ps1 -Path target/release/siskin-windows-spike.exe
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Path,
    [string]$Subject = 'CN=Siskin Dev'
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Path)) {
    throw "Executable not found: $Path"
}

$certificate = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq $Subject -and $_.NotAfter -gt (Get-Date) } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1
if ($null -eq $certificate) {
    throw 'No dev signing certificate. Run setup-dev-signing.ps1 first.'
}

$kitsRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
$kit = Get-ChildItem $kitsRoot -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^\d+\.\d+' } |
    Sort-Object { [version]$_.Name } -Descending |
    Select-Object -First 1
if ($null -eq $kit) {
    throw "Windows SDK not found under $kitsRoot"
}

$signtool = Join-Path $kit.FullName 'x64\signtool.exe'
if (-not (Test-Path $signtool)) {
    throw "signtool.exe not found: $signtool"
}

& $signtool sign /fd SHA256 /sha1 $certificate.Thumbprint /d 'Siskin dev build' $Path
if ($LASTEXITCODE -ne 0) {
    throw "signtool failed with exit code $LASTEXITCODE"
}

$signature = Get-AuthenticodeSignature $Path
if ($signature.Status -ne 'Valid') {
    throw "signature status is $($signature.Status)"
}

# Sandboxed agents and some CI runners create files with a Low integrity label.
# Explorer then shows the "unknown publisher" security warning even though the
# Authenticode signature is valid and the certificate is trusted. Relabel the
# output so a normal user can start it from Explorer without a prompt.
& icacls.exe $Path /setintegritylevel Medium | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "icacls failed to set the integrity level (exit $LASTEXITCODE)"
}

Write-Host ('signed {0} ({1})' -f $Path, $certificate.Subject)
