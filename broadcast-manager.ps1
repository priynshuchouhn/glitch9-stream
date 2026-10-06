<#
.SYNOPSIS
  Multi-session broadcast manager for glitch9-stream.

.DESCRIPTION
  Glitch9-stream is the SPECTATOR/broadcast service that runs ALONGSIDE RhinoStream
  (which serves the player). DXGI Desktop Duplication only captures the desktop of
  the session the process runs in, so broadcasting N gamer sessions means N engine
  instances — one launched INSIDE each session (as SYSTEM, via PsExec -i <sessionId>)
  on its own TCP port.

  This script enumerates active gamer RDP sessions, maps each to a deterministic
  port, and starts/stops/report one engine per session.

  Port mapping: base port + session_id. Default base 8080, so session 2 -> 8082,
  session 3 -> 8083, etc. Deterministic so a viewer URL for a session is stable.

.PARAMETER Action
  start | stop | status   (default: status)

.PARAMETER UserPattern
  Regex of session usernames to manage. Default "^gamer\d+$" (gamer1..gamerN).

.PARAMETER BasePort
  Base TCP port; a session's port = BasePort + sessionId. Default 8080.

.PARAMETER PublicIp
  Public IP advertised in ICE candidates (G9_PUBLIC_IP). Default 103.171.97.176.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File broadcast-manager.ps1 -Action start
  powershell -ExecutionPolicy Bypass -File broadcast-manager.ps1 -Action status
  powershell -ExecutionPolicy Bypass -File broadcast-manager.ps1 -Action stop
#>
param(
    [ValidateSet("start", "stop", "status")]
    [string]$Action = "status",
    [string]$UserPattern = "^gamer\d+$",
    [int]$BasePort = 8080,
    [string]$PublicIp = "103.171.97.176",
    [int]$Width = 1920,
    [int]$Height = 1080,
    [int]$Fps = 30,
    [int]$Bitrate = 3000000,
    [string]$Root = "C:\glitch9-stream"
)

$ErrorActionPreference = "Stop"
$exe = Join-Path $Root "target\release\glitch9-stream.exe"
$psexec = Join-Path $Root "PsExec64.exe"
$runner = Join-Path $Root "run-session.bat"

# Parse `query session` into objects: name, user, id, state.
function Get-GamerSessions {
    $lines = (query session) 2>$null
    $sessions = @()
    foreach ($line in $lines | Select-Object -Skip 1) {
        # Columns are fixed-width; split on whitespace runs. The leading ">" marks
        # the current session — strip it.
        $t = ($line -replace '^\s*>', ' ').Trim() -split '\s+'
        if ($t.Count -lt 3) { continue }
        # Layout: SESSIONNAME USERNAME ID STATE ...  (USERNAME blank for listeners)
        # Find the numeric ID and the username before it.
        for ($i = 0; $i -lt $t.Count; $i++) {
            if ($t[$i] -match '^\d+$') {
                $id = [int]$t[$i]
                $user = if ($i -ge 1) { $t[$i - 1] } else { "" }
                $state = if ($i + 1 -lt $t.Count) { $t[$i + 1] } else { "" }
                if ($user -match $UserPattern -and $state -eq "Active") {
                    $sessions += [pscustomobject]@{ User = $user; Id = $id; Port = $BasePort + $id }
                }
                break
            }
        }
    }
    $sessions
}

function Start-All {
    $sessions = Get-GamerSessions
    if (-not $sessions) { Write-Host "No active gamer sessions found." -ForegroundColor Yellow; return }
    foreach ($s in $sessions) {
        Write-Host "Starting broadcast: $($s.User) (session $($s.Id)) -> port $($s.Port)" -ForegroundColor Cyan
        # Launch the engine as SYSTEM inside the session. run-session.bat takes
        # <port> <tag> [w h fps bitrate ip]; tag = session id for per-session logs.
        # PsExec writes progress to stderr; don't let that abort the loop. Launch via
        # cmd /c with stderr redirected so PowerShell's Stop preference is unaffected.
        $argline = "-accepteula -nobanner -s -i $($s.Id) -d `"$runner`" $($s.Port) $($s.Id) $Width $Height $Fps $Bitrate $PublicIp"
        Start-Process -FilePath $psexec -ArgumentList $argline -NoNewWindow -Wait `
            -RedirectStandardError (Join-Path $Root "psexec-$($s.Id).err") | Out-Null
        Start-Sleep -Milliseconds 500
    }
    Start-Sleep -Seconds 3
    Show-Status
}

function Stop-All {
    Write-Host "Stopping all broadcast engines..." -ForegroundColor Cyan
    taskkill /im glitch9-stream.exe /f 2>$null | Out-Null
    Write-Host "Stopped."
}

function Show-Status {
    $sessions = Get-GamerSessions
    Write-Host "`nActive gamer sessions and broadcast ports:" -ForegroundColor Green
    $procs = Get-Process glitch9-stream -ErrorAction SilentlyContinue
    foreach ($s in $sessions) {
        $listening = (netstat -ano | Select-String ":$($s.Port)\s" | Select-String "LISTENING") -ne $null
        $live = if ($listening) { "LIVE  http://$PublicIp`:$($s.Port)/" } else { "stopped" }
        Write-Host ("  {0,-8} session {1,-3} port {2}  {3}" -f $s.User, $s.Id, $s.Port, $live)
    }
    Write-Host "`nEngine processes: $(@($procs).Count)"
}

switch ($Action) {
    "start"  { Start-All }
    "stop"   { Stop-All }
    "status" { Show-Status }
}
