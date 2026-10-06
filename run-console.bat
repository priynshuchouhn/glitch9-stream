@echo off
REM Launch the streaming engine in the interactive console session (via a
REM scheduled task with /it). Logs to stream.log. Used for POC testing on a
REM headless GPU VM where the GPU display lives in the console session.
cd /d C:\glitch9-stream
set G9_PUBLIC_IP=103.171.97.176
target\release\glitch9-stream.exe --bind 0.0.0.0 --port 8080 --display 0 --width 1024 --height 768 --audio false > C:\glitch9-stream\stream.log 2>&1
