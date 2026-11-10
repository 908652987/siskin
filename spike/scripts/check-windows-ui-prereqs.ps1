<#
.SYNOPSIS
    Checks the Windows toolchain needed by the Slint desktop UI (docs/ARCHITECTURE.md §15).

.DESCRIPTION
    Read-only. Reports the Rust toolchain, its MSVC target, Visual Studio C++ tools,
    the .NET SDK and the WebView2 runtime. Exit code 0 means the Slint build can run.

.PARAMETER Json
    Print the report as JSON instead of a table.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File spike/scripts/check-windows-ui-prereqs.ps1
#>
[CmdletBinding()]
param(
    [switch]$Json
)

$ErrorActionPreference = 'Stop'

function Resolve-CommandPath {
    param([string]$Name)

    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }

    # rustup installs per user; a shell started before the PATH change cannot see it.
    $candidate = Join-Path $env:USERPROFILE ('.cargo\bin\' + $Name + '.exe')
    if (Test-Path $candidate) {
        return $candidate
    }
    return $null
}

$cargoPath = Resolve-CommandPath 'cargo'
$rustcPath = Resolve-CommandPath 'rustc'
$rustupPath = Resolve-CommandPath 'rustup'

$cargoVersion = $null
if ($cargoPath) {
    $cargoVersion = (& $cargoPath --version 2>$null | Out-String).Trim()
}

$msvcTarget = $false
if ($rustupPath) {
    $targets = @(& $rustupPath target list --installed 2>$null)
    $msvcTarget = @($targets | Where-Object { $_ -eq 'x86_64-pc-windows-msvc' }).Count -gt 0
}

$vswhere = $null
$visualStudio = $null
if ($env:ProgramFiles) {
    $vswhereCandidate = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhereCandidate) {
        $vswhere = $vswhereCandidate
        $visualStudio = (& $vswhereCandidate -latest -products '*' `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -property installationPath 2>$null | Out-String).Trim()
        if ([string]::IsNullOrWhiteSpace($visualStudio)) {
            $visualStudio = $null
        }
    }
}

$dotnetPath = Resolve-CommandPath 'dotnet'
$dotnetSdks = @()
if ($dotnetPath) {
    $dotnetSdks = @(& $dotnetPath --list-sdks 2>$null)
}

$webViewVersions = @()
$webViewRoots = @()
if ($env:ProgramFiles) {
    $webViewRoots += Join-Path $env:ProgramFiles 'Microsoft\EdgeWebView\Application'
}
if (${env:ProgramFiles(x86)}) {
    $webViewRoots += Join-Path ${env:ProgramFiles(x86)} 'Microsoft\EdgeWebView\Application'
}
foreach ($root in $webViewRoots) {
    if (Test-Path $root) {
        $webViewVersions += @(Get-ChildItem -Path $root -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^\d+\.' } |
            Select-Object -ExpandProperty Name)
    }
}

$report = [pscustomobject]@{
    os            = [System.Environment]::OSVersion.VersionString
    architecture  = $env:PROCESSOR_ARCHITECTURE
    cargo         = $cargoPath
    cargoVersion  = $cargoVersion
    rustc         = $rustcPath
    rustup        = $rustupPath
    msvcTarget    = $msvcTarget
    visualStudio  = $visualStudio
    dotnetSdk     = $dotnetSdks
    webView2      = $webViewVersions
    ready         = [bool]($cargoPath -and $msvcTarget -and $visualStudio)
}

if ($Json) {
    $report | ConvertTo-Json -Depth 4
} else {
    Write-Host 'Siskin Windows UI prerequisites'
    Write-Host ('  OS              : {0} ({1})' -f $report.os, $report.architecture)
    Write-Host ('  cargo           : {0}' -f $(if ($cargoPath) { $cargoVersion } else { 'MISSING' }))
    Write-Host ('  rustc           : {0}' -f $(if ($rustcPath) { $rustcPath } else { 'MISSING' }))
    Write-Host ('  MSVC target     : {0}' -f $(if ($msvcTarget) { 'installed' } else { 'MISSING' }))
    Write-Host ('  Visual Studio   : {0}' -f $(if ($visualStudio) { $visualStudio } else { 'MISSING (C++ tools)' }))
    Write-Host ('  .NET SDK        : {0}' -f $(if ($dotnetSdks.Count -gt 0) { $dotnetSdks -join ', ' } else { 'none (not needed)' }))
    Write-Host ('  WebView2        : {0}' -f $(if ($webViewVersions.Count -gt 0) { $webViewVersions -join ', ' } else { 'MISSING' }))
    Write-Host ''
    if ($report.ready) {
        Write-Host 'Ready: build apps/desktop and run Spike S3 (docs/SPIKE-REPORT.md §10).'
    } else {
        Write-Host 'Not ready. Install Rust (winget install Rustlang.Rustup) and the Visual Studio'
        Write-Host 'Build Tools C++ workload, then run this check again.'
    }
}

if (-not $report.ready) {
    exit 1
}
exit 0
