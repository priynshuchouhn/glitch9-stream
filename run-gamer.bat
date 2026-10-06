@echo off
REM Run the engine inside the gamer1 RDP session (launched via PsExec -s -i 2)
REM to capture that session's GPU-composited desktop and serve WebRTC.
cd /d C:\glitch9-stream
set G9_PUBLIC_IP=103.171.97.176
REM Lower bitrate + fps for a remote internet viewer: 61.8%% loss at 7.7Mbps/64fps
REM means the path can't sustain it. 3 Mbps @ 30fps is far more forgiving.
target\release\glitch9-stream.exe --bind 0.0.0.0 --port 8080 --display 0 --width 1920 --height 1080 --fps 30 --bitrate 3000000 --audio false > C:\glitch9-stream\gamer_stream.log 2>&1
