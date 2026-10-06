@echo off
REM Open Edge fullscreen on an animated page in the console session, so the
REM desktop has continuous motion for DXGI Desktop Duplication to capture.
REM Uses a bouncing-DVD style CSS animation served from a data: URL.
start "" "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe" --new-window --start-fullscreen --kiosk "https://bouncingdvdlogo.com/"
