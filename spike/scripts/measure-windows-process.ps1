<#
.SYNOPSIS
    Samples a process's private working set, private commit, working set and CPU for the Windows UI Spike.

.DESCRIPTION
    Reads a process by name, plus (by default) every descendant, which is how the
    WebView2 child processes are counted. Prints min/avg/max and optionally writes
    one CSV row per sample. Read-only; it does not attach to or alter the process.

.PARAMETER ProcessName
    Process name without the .exe suffix, for example siskin-desktop.

.PARAMETER Seconds
    Number of samples. One sample per IntervalMs.

.PARAMETER IntervalMs
    Delay between samples in milliseconds.

.PARAMETER RootOnly
    Measure only the named process, without its child processes (WebView2 children are
    included by default).

.PARAMETER CsvPath
    Optional path for the per-sample CSV.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File spike/scripts/measure-windows-process.ps1 -ProcessName siskin-desktop -Seconds 60 -CsvPath spike/data/windows-ui-idle.csv
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ProcessName,

    [int]$Seconds = 60,

    [int]$IntervalMs = 1000,

    [switch]$RootOnly,

    [string]$CsvPath
)

$includeDescendants = -not $RootOnly

$ErrorActionPreference = 'Stop'

function Get-ProcessIds {
    param(
        [int[]]$RootIds,
        [bool]$WithDescendants
    )

    if (-not $WithDescendants) {
        return @($RootIds)
    }

    $all = @(Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId -ErrorAction SilentlyContinue)
    $children = @{}
    foreach ($entry in $all) {
        $parent = [int]$entry.ParentProcessId
        if (-not $children.ContainsKey($parent)) {
            $children[$parent] = New-Object 'System.Collections.Generic.List[int]'
        }
        $children[$parent].Add([int]$entry.ProcessId)
    }

    $seen = New-Object 'System.Collections.Generic.HashSet[int]'
    $queue = New-Object 'System.Collections.Generic.Queue[int]'
    foreach ($root in $RootIds) {
        $queue.Enqueue($root)
    }

    while ($queue.Count -gt 0) {
        $current = $queue.Dequeue()
        if (-not $seen.Add($current)) {
            continue
        }
        if ($children.ContainsKey($current)) {
            foreach ($child in $children[$current]) {
                $queue.Enqueue($child)
            }
        }
    }

    return @($seen)
}

function Get-ProcessTotals {
    param([int[]]$Ids)

    $private = [int64]0
    $working = [int64]0
    $cpu = [double]0
    $count = 0

    foreach ($processId in $Ids) {
        $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
        if ($null -eq $process) {
            continue
        }
        if ($process.HasExited) {
            continue
        }
        $private += $process.PrivateMemorySize64
        $working += $process.WorkingSet64
        if ($null -ne $process.CPU) {
            $cpu += [double]$process.CPU
        }
        $count++
    }

    return [pscustomobject]@{
        Private = $private
        Working = $working
        Cpu     = $cpu
        Count   = $count
    }
}

function Get-PrivateWorkingSet {
    param([int[]]$Ids)

    $wanted = New-Object 'System.Collections.Generic.HashSet[int]'
    foreach ($processId in $Ids) {
        $null = $wanted.Add($processId)
    }

    $total = [int64]0
    $rows = @(Get-CimInstance Win32_PerfRawData_PerfProc_Process `
        -Property IDProcess, WorkingSetPrivate -ErrorAction SilentlyContinue)
    foreach ($row in $rows) {
        if ($wanted.Contains([int]$row.IDProcess)) {
            $total += [int64]$row.WorkingSetPrivate
        }
    }
    return $total
}

function Get-Stats {
    param($Values)

    $items = @($Values)
    if ($items.Count -eq 0) {
        return $null
    }
    $measured = $items | Measure-Object -Minimum -Maximum -Average
    return [pscustomobject]@{
        Min = [math]::Round($measured.Minimum, 2)
        Avg = [math]::Round($measured.Average, 2)
        Max = [math]::Round($measured.Maximum, 2)
    }
}

$roots = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
    Where-Object { -not $_.HasExited })
if ($roots.Count -eq 0) {
    throw "No process named '$ProcessName' is running."
}

$rootIds = @($roots | ForEach-Object { $_.Id })
$sampleCount = [math]::Max(1, $Seconds)
$samples = New-Object 'System.Collections.Generic.List[object]'
$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
$previous = $null
$previousMs = 0.0

for ($index = 0; $index -lt $sampleCount; $index++) {
    $ids = Get-ProcessIds -RootIds $rootIds -WithDescendants $includeDescendants
    $totals = Get-ProcessTotals -Ids $ids
    $privateWorkingSet = Get-PrivateWorkingSet -Ids $ids
    $nowMs = $stopwatch.Elapsed.TotalMilliseconds

    $cpuPercent = $null
    if ($null -ne $previous -and $nowMs -gt $previousMs) {
        $deltaCpu = $totals.Cpu - $previous.Cpu
        if ($deltaCpu -lt 0) {
            $deltaCpu = 0
        }
        $elapsedSeconds = ($nowMs - $previousMs) / 1000.0
        $cpuPercent = [math]::Round(($deltaCpu / $elapsedSeconds) /
            [Environment]::ProcessorCount * 100.0, 2)
    }

    $samples.Add([pscustomobject]@{
        elapsedSeconds     = [math]::Round($nowMs / 1000.0, 2)
        privateWorkingSetMB = [math]::Round($privateWorkingSet / 1MB, 2)
        privateCommitMB    = [math]::Round($totals.Private / 1MB, 2)
        workingSetMB       = [math]::Round($totals.Working / 1MB, 2)
        cpuPercent         = $cpuPercent
        processes          = $totals.Count
    })

    $previous = $totals
    $previousMs = $nowMs

    if ($index -lt ($sampleCount - 1)) {
        Start-Sleep -Milliseconds $IntervalMs
    }
}

$privateWsStats = Get-Stats -Values ($samples | ForEach-Object { $_.privateWorkingSetMB })
$privateStats = Get-Stats -Values ($samples | ForEach-Object { $_.privateCommitMB })
$workingStats = Get-Stats -Values ($samples | ForEach-Object { $_.workingSetMB })
$cpuStats = Get-Stats -Values ($samples | Where-Object { $null -ne $_.cpuPercent } | ForEach-Object { $_.cpuPercent })

Write-Host ('Process : {0} (roots {1})' -f $ProcessName, ($rootIds -join ', '))
Write-Host ('Samples : {0}, {1} ms apart, descendants: {2}' -f $samples.Count, $IntervalMs, $includeDescendants)
if ($null -ne $privateWsStats) {
    Write-Host ('Private WS (Task Manager Memory): min {0} / avg {1} / max {2} MB' -f $privateWsStats.Min, $privateWsStats.Avg, $privateWsStats.Max)
}
if ($null -ne $privateStats) {
    Write-Host ('Private commit: min {0} / avg {1} / max {2} MB' -f $privateStats.Min, $privateStats.Avg, $privateStats.Max)
}
if ($null -ne $workingStats) {
    Write-Host ('Working : min {0} / avg {1} / max {2} MB' -f $workingStats.Min, $workingStats.Avg, $workingStats.Max)
}
if ($null -ne $cpuStats) {
    Write-Host ('CPU     : min {0} / avg {1} / max {2} %' -f $cpuStats.Min, $cpuStats.Avg, $cpuStats.Max)
} else {
    Write-Host 'CPU     : not enough samples'
}

if ($CsvPath) {
    $directory = Split-Path -Parent $CsvPath
    if ($directory -and -not (Test-Path $directory)) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    $samples | Export-Csv -Path $CsvPath -NoTypeInformation -Encoding UTF8
    Write-Host ('CSV     : {0}' -f $CsvPath)
}
