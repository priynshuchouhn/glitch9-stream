# Run Guide — Glitch9 Streaming Engine

All commands run on the Windows + NVIDIA host after `cargo build --release`.

## 0. GPU-backed display requirement (important)

The engine captures a **GPU-backed** DXGI output. On a normal desktop with a monitor
(or a virtual display) this is automatic. On **Windows Server RDSH / disconnected RDP
sessions**, Windows gives the session a software "Microsoft Remote Display Adapter"
unless WDDM is enabled, and DXGI will not see a GPU output. Enable it once:

```bat
reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services" /v UseWddmDriver /t REG_DWORD /d 1 /f
reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services" /v bEnumerateHWBeforeSW /t REG_DWORD /d 1 /f
```

(These are the same keys the gaming VMs already use. Reboot / restart the session to apply.)

## 1. List displays

```bat
glitch9-stream.exe --list-displays
```

Shows adapters (NVIDIA preferred) and display indices to pass to `--display`.

## 2. WebRTC → browser (view-only)

```bat
glitch9-stream.exe --display 0 --output webrtc --width 1920 --height 1080 --fps 60 --bitrate 8000000
```

Open the viewer in Chrome/Edge: `http://127.0.0.1:8080/` and click **Watch stream**.
For LAN testing, bind to the LAN IP: `--bind 0.0.0.0` then open `http://<host-ip>:8080/`.

## 3. YouTube → RTMPS

Put the stream key in an env var (never on the command line / logs):

```bat
set G9_STREAM_KEY=xxxx-xxxx-xxxx-xxxx
glitch9-stream.exe --display 0 --output youtube --youtube --width 1920 --height 1080 --fps 60 --bitrate 8000000
```

Or point at any RTMP(S) ingest (Twitch/Facebook/custom):

```bat
glitch9-stream.exe --display 0 --output youtube --rtmp-url "rtmps://host/app" --width 1920 --height 1080 --fps 60 --bitrate 8000000
```

> The stream key is read from `G9_STREAM_KEY` (preferred), else `--stream-key`, else
> `--youtube-stream-key`. It is treated as a secret: never printed, logged, or sent to
> the browser.

## 4. Both at once

```bat
set G9_STREAM_KEY=xxxx-xxxx-xxxx-xxxx
glitch9-stream.exe --display 0 --output webrtc,youtube --youtube --fps 60 --bitrate 8000000
```

The engine captures and converts the frame **once**. If the WebRTC and YouTube
encoder profiles are compatible it runs **one** NVENC session (Mode A); otherwise it
runs **two** NVENC sessions that share the same captured NV12 texture (Mode B). It
never captures the display twice. The chosen mode is logged at startup.

## Options

```
--list-displays
--output webrtc | youtube | webrtc,youtube
--display N
--width / --height / --fps
--bitrate <bits/sec>          (4_000_000 – 12_000_000 recommended)
--audio true|false
--rtmp-url <url>              (reusable: YouTube/Twitch/custom)
--youtube                     (default rtmp-url to YouTube ingest)
--port 8080                   (signaling + viewer)
--bind 127.0.0.1              (0.0.0.0 for LAN)
--stats-interval 5
--log-level info|debug|trace
```

## Metrics

Every `--stats-interval` seconds the engine logs capture/encode FPS, per-stage
latency (capture/convert/encode), `cpu_readbacks` (should always be 0 — the video
path is GPU-resident), dropped-frame counters, and per-transport stats
(state/viewers/bitrate/RTT/loss/jitter/bytes/reconnects).
