<#
.SYNOPSIS
    Creates and trusts a local code-signing certificate for dev builds.

.DESCRIPTION
    Windows shows "Open File - Security Warning" (Unknown Publisher) for an unsigned
    executable launched from Explorer. Each cargo build replaces the exe, so the warning
    returns. This creates a self-signed code-signing certificate in the current user's
    store, exports its public part to %LOCALAPPDATA%\SiskinDev, and trusts it for code
    signing. Run once per user account. The private key stays in Cert:\CurrentUser\My.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File spike/scripts/setup-dev-signing.ps1
#>
[CmdletBinding()]
param(
    [string]$Subject = 'CN=Siskin Dev'
)

$ErrorActionPreference = 'Stop'

$certificate = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq $Subject -and $_.NotAfter -gt (Get-Date) } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1

if ($null -eq $certificate) {
    $certificate = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $Subject `
        -CertStoreLocation Cert:\CurrentUser\My `
        -KeyExportPolicy Exportable `
        -NotAfter (Get-Date).AddYears(3)
    Write-Host ('created {0}' -f $certificate.Thumbprint)
} else {
    Write-Host ('reusing {0}' -f $certificate.Thumbprint)
}

$directory = Join-Path $env:LOCALAPPDATA 'SiskinDev'
New-Item -ItemType Directory -Force -Path $directory | Out-Null
$cerPath = Join-Path $directory 'siskin-dev.cer'
Export-Certificate -Cert $certificate -FilePath $cerPath -Force | Out-Null

foreach ($store in @('TrustedPublisher', 'Root')) {
    $output = & certutil.exe -user -addstore -f $store $cerPath 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw ('certutil {0} failed: {1}' -f $store, ($output -join ' '))
    }
}

Write-Host ('trusted {0}' -f $cerPath)
Write-Host ('thumbprint {0}' -f $certificate.Thumbprint)
