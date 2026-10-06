@echo off
REM Run the engine inside the gamer1 RDP session (launched via PsExec -s -i 2)
REM to capture that session's GPU-composited desktop and serve WebRTC.
cd /d C:\glitch9-stream
set G9_PUBLIC_IP=103.171.97.176
REM 3 Mbps @ 30fps for a remote internet viewer; audio enabled (WASAPI loopback
REM captures the session's playback audio -> Opus -> WebRTC).
target\release\glitch9-stream.exe --bind 0.0.0.0 --port 8080 --display 0 --width 1920 --height 1080 --fps 30 --bitrate 3000000 --audio true > C:\glitch9-stream\gamer_stream.log 2>&1
