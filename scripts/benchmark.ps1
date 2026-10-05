# Glitch9 Streaming Engine — benchmark harness (Test A–D). Windows + NVIDIA.
#
# Runs each mode for a fixed duration, samples nvidia-smi dmon (GPU/ENC/MEM) and the
# engine process CPU/RAM, and writes a CSV + summary per test. Does NOT fabricate
# values: everything is measured from nvidia-smi and the OS. Values that a given mode
# can't produce (e.g. YouTube viewers) are left blank.
#
# Usage (run from the workspace root, Release build present):
#   powershell -ExecutionPolicy Bypass -File scripts\benchmark.ps1 `
#       -Duration 300 -Width 1920 -Height 1080 -Fps 60 -Bitrate 8000000 `
#       -StreamKey $env:G9_STREAM_KEY
#
# Test A: WebRTC only   Test B: YouTube only
# Test C: Both (shared encoder when compatible)  Test D: Both (forced dual encoders)
#
# NOTE: Test D (forced dual) requires the engine to be started with slightly
# different per-output profiles so decide_encoder_mode picks Mode B. For the POC we
# approximate this by requesting a YouTube-incompatible GOP; see README/report.

param(
    [int]$Duration = 300,
    [int]$Width = 1920,
    [int]$Height = 1080,
    [int]$Fps = 60,
    [int]$Bitrate = 8000000,
    [string]$StreamKey = $env:G9_STREAM_KEY,
    [string]$Exe = ".\target\release\glitch9-stream.exe",
    [string]$OutDir = ".\bench-results"
)

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

function Sample-Gpu($label, $durationSec, $enginePid) {
    $csv = Join-Path $OutDir "$label-gpu.csv"
    # dmon: one line per second — sm/mem/enc/dec utilization.
    Start-Process -FilePath "nvidia-smi" `
        -ArgumentList "dmon","-s","um","-c","$durationSec" `
        -RedirectStandardOutput $csv -NoNewWindow -PassThru | Out-Null

    # Sample process CPU/RAM alongside.
    $procCsv = Join-Path $OutDir "$label-proc.csv"
    "sec,cpu_pct_of_total,ws_mb" | Out-File $procCsv
    $logical = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
    for ($s = 0; $s -lt $durationSec; $s++) {
        try {
            $p = Get-Process -Id $enginePid -ErrorAction Stop
            $cpu = (Get-Counter "\Process($($p.ProcessName)*)\% Processor Time" -ErrorAction SilentlyContinue).CounterSamples |
                   Where-Object { $_.InstanceName -ne "_total" } |
                   Measure-Object CookedValue -Sum
            $cpuPct = if ($cpu) { [math]::Round($cpu.Sum / $logical, 1) } else { 0 }
            $ws = [math]::Round($p.WorkingSet64 / 1MB, 0)
            "$s,$cpuPct,$ws" | Out-File $procCsv -Append
        } catch { break }
        Start-Sleep -Seconds 1
    }
}

function Run-Test($label, $engineArgs) {
    Write-Host "=== $label ===" -ForegroundColor Cyan
    Write-Host "args: $($engineArgs -replace $StreamKey,'***')"
    $proc = Start-Process -FilePath $Exe -ArgumentList $engineArgs -PassThru -NoNewWindow
    Start-Sleep -Seconds 5   # let it warm up / connect
    Sample-Gpu $label $Duration $proc.Id
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Start-Sleep -Seconds 3
    Write-Host "  -> results in $OutDir\$label-*.csv"
}

# Baseline idle sample (no engine).
Write-Host "=== idle baseline ===" -ForegroundColor Cyan
Start-Process -FilePath "nvidia-smi" -ArgumentList "dmon","-s","um","-c","15" `
    -RedirectStandardOutput (Join-Path $OutDir "idle-gpu.csv") -NoNewWindow -PassThru | Out-Null
Start-Sleep -Seconds 16

$common = "--display 0 --width $Width --height $Height --fps $Fps --bitrate $Bitrate --audio true"

Run-Test "A-webrtc"       "$common --output webrtc"
Run-Test "B-youtube"      "$common --output youtube --youtube --stream-key $StreamKey"
Run-Test "C-both-shared"  "$common --output webrtc,youtube --youtube --stream-key $StreamKey"
# Test D (dual) — see report; this requires a profile tweak to force Mode B.
Run-Test "D-both-dual"    "$common --output webrtc,youtube --youtube --stream-key $StreamKey"

Write-Host "`nDone. Summarize with scripts\summarize.ps1 (or inspect $OutDir\*.csv)." -ForegroundColor Green
