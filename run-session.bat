@echo off
REM Per-session broadcast launcher. Invoked (via PsExec -s -i <sessionId>) INSIDE a
REM gamer session so DXGI Desktop Duplication captures that session's desktop.
REM
REM Args: <port> <tag> [width] [height] [fps] [bitrate] [publicIp]
REM   port    - unique TCP port for this session's WebRTC viewer/signaling
REM   tag     - log suffix (session id), so logs don't collide
REM   rest    - optional geometry/bitrate/IP (defaults below)
REM
REM Each session gets its own engine process, port, and log. One capture + NVENC
REM per session; they share the GPU (validated: ~+2-4%% ENC each, concurrent OK).
setlocal
cd /d C:\glitch9-stream
set PORT=%~1
set TAG=%~2
set WIDTH=%~3
set HEIGHT=%~4
set FPS=%~5
set BITRATE=%~6
set PUBIP=%~7
if "%PORT%"==""   set PORT=8080
if "%TAG%"==""    set TAG=default
if "%WIDTH%"==""  set WIDTH=1920
if "%HEIGHT%"=="" set HEIGHT=1080
if "%FPS%"==""    set FPS=30
if "%BITRATE%"=="" set BITRATE=3000000
if "%PUBIP%"==""  set PUBIP=103.171.97.176
set G9_PUBLIC_IP=%PUBIP%
target\release\glitch9-stream.exe --bind 0.0.0.0 --port %PORT% --display 0 --width %WIDTH% --height %HEIGHT% --fps %FPS% --bitrate %BITRATE% --audio true > C:\glitch9-stream\session-%TAG%.log 2>&1
endlocal
