# Summarize benchmark CSVs into the POC report's performance table.
# Averages the dmon samples (sm/mem/enc) and process CPU/RAM per test.
# Reports MEASURED values only; blanks where a column doesn't apply.

param([string]$OutDir = ".\bench-results")

function Avg-Dmon($file) {
    if (-not (Test-Path $file)) { return $null }
    $sm = @(); $mem = @(); $enc = @()
    Get-Content $file | ForEach-Object {
        $line = $_.Trim()
        if ($line -match '^\d') {
            $cols = -split $line
            # dmon -s um columns: gpu sm mem enc dec ...
            if ($cols.Count -ge 5) {
                $sm  += [double]$cols[1]
                $mem += [double]$cols[2]
                $enc += [double]$cols[3]
            }
        }
    }
    if ($sm.Count -eq 0) { return $null }
    [pscustomobject]@{
        SmPct  = [math]::Round(($sm  | Measure-Object -Average).Average, 1)
        MemPct = [math]::Round(($mem | Measure-Object -Average).Average, 1)
        EncPct = [math]::Round(($enc | Measure-Object -Average).Average, 1)
    }
}

function Avg-Proc($file) {
    if (-not (Test-Path $file)) { return $null }
    $cpu = @(); $ws = @()
    Import-Csv $file | ForEach-Object { $cpu += [double]$_.cpu_pct_of_total; $ws += [double]$_.ws_mb }
    if ($cpu.Count -eq 0) { return $null }
    [pscustomobject]@{
        CpuPct = [math]::Round(($cpu | Measure-Object -Average).Average, 1)
        WsMb   = [math]::Round(($ws  | Measure-Object -Average).Average, 0)
    }
}

$tests = @(
    @{ Label = "idle";          Name = "Idle" },
    @{ Label = "A-webrtc";      Name = "WebRTC" },
    @{ Label = "B-youtube";     Name = "YouTube" },
    @{ Label = "C-both-shared"; Name = "WebRTC+YouTube (shared)" },
    @{ Label = "D-both-dual";   Name = "WebRTC+YouTube (dual)" }
)

"| Metric | " + (($tests | ForEach-Object { $_.Name }) -join " | ") + " |"
"|---|" + (($tests | ForEach-Object { "---:" }) -join "|") + "|"

$gpu  = @{}; $proc = @{}
foreach ($t in $tests) {
    $gpu[$t.Label]  = Avg-Dmon (Join-Path $OutDir "$($t.Label)-gpu.csv")
    $proc[$t.Label] = Avg-Proc (Join-Path $OutDir "$($t.Label)-proc.csv")
}

function Row($metric, $sel) {
    "| $metric | " + (($tests | ForEach-Object { & $sel $_.Label }) -join " | ") + " |"
}

Row "CPU %"    { param($l) if ($proc[$l]) { $proc[$l].CpuPct } else { "" } }
Row "RAM MB"   { param($l) if ($proc[$l]) { $proc[$l].WsMb }   else { "" } }
Row "GPU SM %" { param($l) if ($gpu[$l])  { $gpu[$l].SmPct }   else { "" } }
Row "NVENC %"  { param($l) if ($gpu[$l])  { $gpu[$l].EncPct }  else { "" } }
Row "GPU MEM %"{ param($l) if ($gpu[$l])  { $gpu[$l].MemPct }  else { "" } }

Write-Host "`nPaste the table above into docs\POC-REPORT.md (Performance Comparison)."
