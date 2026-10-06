# glitch9-stream POC Report

**Date:** 2026-10-06
**VM:** Pugmarks `XYPT-MEDIA-GPU` (`103.171.97.176`)
**GPU:** 2× NVIDIA RTX PRO 4000 Blackwell (24 GB VRAM each), driver 580.88
**OS:** Windows Server, multi-session (5 gamer RDP sessions + admin)

---

## Executive Summary

The glitch9-stream engine is a lightweight, headless Rust streaming engine that
captures the Windows desktop via DXGI Desktop Duplication, converts BGRA→NV12 on
the GPU (zero CPU readback), encodes H.264 via NVENC, and delivers the stream
over WebRTC to a browser viewer.

**STATUS: PROVEN END-TO-END.** Live video of a gamer's GPU-rendered session was
captured, encoded, and streamed over WebRTC across the public internet to a
remote browser (macOS), rendering at 1920×1080, ~20fps, ~2 Mbps, 0% packet loss.
Every stage is verified on real NVIDIA RTX PRO 4000 hardware.

### Critical operational findings (required for deployment)

1. **Capture must run inside the target session, as SYSTEM.** DXGI Desktop
   Duplication only captures the desktop of the session it runs in. On this
   multi-session gaming VM, each gamer has their own RDP session with a
   GPU-composited desktop (own `dwm.exe`). The engine must be launched **in that
   session** — e.g. `PsExec -s -i <sessionId> run-gamer.bat` — exactly like the
   production RhinoStream (confirmed running as `system` in the gamer session).
   Running from SSH (session 0) or the empty console session yields no output
   (`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`) or a black capture.

2. **Encode must be fps-limited.** DXGI presents at the display's native rate
   (60-75fps here). Encoding every frame overshot the CBR target (~6.6 Mbps when
   3 Mbps was set), causing 40-60% packet loss to a remote viewer. Throttling the
   encoder to the configured fps fixed the bitrate and dropped loss to 0%.

3. **Viewer must attach the decoding track correctly.** With separate video/audio
   streams, the viewer's `ontrack` must collect the video track into the rendered
   MediaStream; otherwise frames decode (confirmed 2981 decoded, 0 dropped) but
   the `<video>` element stays 0×0 / black.

---

## Verified on Hardware (RTX PRO 4000 Blackwell)

| Component | Status | Evidence |
|-----------|--------|----------|
| DXGI Desktop Duplication capture | ✅ Verified | Frames acquired at 32-56 fps; `cpu_readbacks=0` |
| GPU BGRA→NV12 (D3D11 Video Processor) | ✅ Verified | `convert=0.0ms` (GPU-side, no CPU copy) |
| NVENC H.264 encode | ✅ Verified | `encode=4-8ms`, High profile (SPS `profile_idc=100, level_idc=42`), `profile-level-id=64002a` |
| SPS/PPS on every IDR | ✅ Verified | `repeatSPSPPS=1`, `SPS:` log confirmed on first keyframe |
| Keyframe on viewer join | ✅ Verified | `peer connected -> force initial IDR` logged on each connect |
| RTCP PLI/FIR → force IDR | ✅ Verified | `RTCP keyframe request (PLI/FIR) -> force IDR` logged |
| WebSocket signaling | ✅ Verified | SDP offer/answer exchanged, non-trickle answer with embedded candidates |
| ICE (host candidates, no STUN) | ✅ Verified | `ICE connection state: connected` with matching `103.171.97.176` candidates |
| DTLS/SRTP (rustls ring provider) | ✅ Verified | `peer connection state: connected`, no panic |
| RTP media flow | ✅ Verified | `bytes_sent` climbing steadily (1-2 MB/5s), `dropped=0` |
| Opus audio encode | ✅ Verified | `audio pipeline running (opus=true)` |
| Browser viewer (remote, over internet) | ✅ Verified | **Live video rendered** on macOS browser: 1920×1080, ~20fps, ~2 Mbps, 0% loss |
| fps-limited encode (bitrate control) | ✅ Verified | encode throttled to configured fps; loss 61.8% → 0% |
| In-session SYSTEM capture | ✅ Verified | Real GPU desktop captured (dump mean 24.6/255 vs 0.0 when empty) |
| Geometry mismatch warning | ✅ Added | Engine warns if `--width/--height` don't match captured display |
| Build-time commit banner | ✅ Added | First log line prints the git commit hash for deploy verification |

### Benchmark (measured on the VM, 2026-10-06)

Test A (WebRTC only), 1920×1080@30, 3 Mbps, p4 low-latency, running in the gamer
session via `PsExec -s -i 2`. **Caveat:** this is a *live multi-tenant* host — 5
gamer sessions, a running game (`007FirstLight.exe`), and the production
RhinoStream all share the GPU, so absolute GPU numbers reflect a loaded card.
What's meaningful is our engine's **incremental** cost.

| Metric | Idle (engine off) | Engine running | Our delta |
|--------|-------------------|----------------|-----------|
| GPU SM % | 60–66% | 58–66% | ~0 (noise; GPU already loaded) |
| **NVENC ENC %** | 8–13% | 10–15% | **~+2–4%** (1080p30 encode) |
| GPU VRAM | 8766 MB | 8818 MB | **~+52 MB** |
| Engine CPU | — | **<1% (0.59 CPU-sec)** | negligible |
| Engine RAM (WS) | — | **61 MB** | — |
| Engine threads | — | 78 (tokio + webrtc) | — |

Engine-measured per-frame latencies (from in-process counters):

- **capture:** ~10–11 ms  **convert:** **0.0 ms** (GPU Video Processor)  **encode:** **6.7–7.2 ms**
- **cpu_readbacks: 0** (entire frame path stays on the GPU — confirmed)
- capture 60 fps / encode 22 fps (fps-limited), **0 dropped**

Takeaway: the engine adds only a few percent of the NVENC block, ~50 MB VRAM,
and <1% CPU for a 1080p30 WebRTC stream — consistent with the RhinoStream
reference (GPU-bound, near-zero CPU). `encoder.stats.sessionCount` reads 0 even
while encoding (known NVENC telemetry quirk, see ANALYSIS.md); `utilization.encoder`
+ in-process counters are the reliable signal.

Not yet benchmarked: Test B (YouTube), C (both shared), D (dual) — all need a
YouTube stream key. The harness (`scripts/benchmark.ps1`) runs all four and must
be launched inside the gamer session (not SSH session 0, which can't capture).

### Performance (1920×1080, preset p4, low_latency tuning)

- **Capture FPS:** 32–56 fps (desktop-dependent; driven by DXGI present rate)
- **Encode latency:** 4–8 ms per frame
- **CPU readbacks:** 0 (entire frame path stays on GPU)
- **End-to-end:** capture → encode → RTP in under 15 ms typical
- **Bitrate:** 8 Mbps CBR target; browser reports ~0.8 Mbps received (low due to static/black source)
- **Packet loss:** 0.0%
- **RTT:** 0 ms (same-machine)

---

## How to Run the POC (verified working)

```bat
REM 1. Build (cargo/toolchain under Administrator profile on this VM):
build-ssh.bat        REM sets RUSTUP_HOME/CARGO_HOME + MSVC env, then cargo build --release

REM 2. Launch INSIDE the target gamer session as SYSTEM (session id from `query session`):
PsExec64.exe -accepteula -s -i <sessionId> -d C:\glitch9-stream\run-gamer.bat
REM run-gamer.bat runs:
REM   set G9_PUBLIC_IP=<vm-ip>
REM   glitch9-stream.exe --bind 0.0.0.0 --port 8080 --display 0 --width 1920 --height 1080 --fps 30 --bitrate 3000000 --audio false

REM 3. Open the viewer from any browser:
http://<vm-ip>:8080/   -> Watch stream
```

Verified result: 1920×1080, ~20fps, ~2 Mbps, 0% loss, live gamer desktop in a
remote browser.

## Black Screen Debugging Journey (resolved)

The browser initially showed black for three distinct, sequentially-diagnosed
reasons — none of them capture/encode correctness bugs:

1. **Wrong session.** DXGI captured an empty console/SSH session. Fixed by
   running as SYSTEM inside the gamer's session (see finding #1 above).

2. **Bitrate overshoot → packet loss.** Encoding at native 64fps instead of the
   configured 30fps pushed ~6.6 Mbps and caused ~40-60% remote packet loss.
   Fixed by fps-limiting the encode loop. Loss → 0%.

3. **Viewer track wiring.** Frames decoded cleanly (2981 decoded, 25 keyframes,
   1920×1080, 0 dropped) but `<video>` was 0×0. Fixed by collecting the inbound
   video track into the rendered MediaStream.

A `--dump-frame` diagnostic (reads back one captured frame to PPM) confirmed the
capture content directly: an empty session dumped mean 0.0/255 (black); the
active gamer session dumped mean 24.6/255 with real desktop content.

### Diagnosis

A `--dump-frame` diagnostic captured one frame to a PPM file and analyzed every
pixel:

```
Pixels: 2,073,600 (1920×1080)
Mean RGB: (0.0, 0.0, 0.0)
Non-black pixels (r+g+b > 10): 0 / 2,073,600 (0.0%)
```

### Why

The VM is a multi-session gaming host (`gamer1`–`gamer5` in RDP sessions). The
NVIDIA GPU has **no attached display** — no physical monitor, no virtual display
driver, no headless EDID. Each RDP session uses Microsoft's software Remote
Display Adapter, not the NVIDIA GPU.

DXGI Desktop Duplication binds to the NVIDIA adapter's output, but since the GPU
isn't scanning out a real framebuffer, the captured surface is all zeros (black).

### Display driver inventory (via `Get-PnpDevice -Class Display`)

| Device | Count | Status | Notes |
|--------|-------|--------|-------|
| Microsoft Remote Display Adapter | 14 | 6 OK, 8 Unknown | One per RDP session (software, not GPU-backed) |
| NVIDIA RTX PRO 4000 Blackwell | 1 | OK | **No attached display / no scanout** |
| Microsoft Basic Display Adapter | 1 | Error | AMD device, disabled |

No third-party virtual display driver (IddSampleDriver, Parsec VDD, usbmmidd)
is installed.

---

## Deployment Requirement: GPU-Backed Display

For glitch9-stream to capture real content, the NVIDIA GPU must have an attached
display surface. Options:

1. **Virtual display driver** (recommended): Install Parsec VDD, IddSampleDriver,
   or usbmmidd to create a virtual monitor on the NVIDIA GPU. The GPU then
   composites a real desktop and Desktop Duplication captures it.

2. **NVIDIA headless EDID**: Force an EDID on the GPU so it presents a display
   with no physical monitor. Requires NVIDIA driver tools and capturing from the
   console session.

3. **Physical/virtual HDMI dummy**: If the hosting provider supports it, attach a
   virtual HDMI dongle so the GPU sees a monitor.

4. **Per-session capture (advanced)**: For streaming individual gamer sessions,
   each session needs its own GPU-backed virtual display. This is how
   RhinoStream and cloud-gaming platforms handle multi-tenant streaming.

---

## Architecture

```
DXGI Desktop Duplication (GPU texture, zero-copy)
       │
       ▼
D3D11 Video Processor: BGRA → NV12 (GPU, 0ms)
       │
       ▼
NVENC H.264 Encoder (GPU, 4-8ms, High profile, CBR)
       │
       ├──► WebRTC (webrtc-rs): SRTP/RTP → Browser
       │      ├── Signaling: WebSocket (SDP offer/answer, non-trickle)
       │      ├── ICE: host candidates, no STUN/TURN
       │      ├── DTLS: rustls ring provider
       │      └── RTCP: PLI/FIR → force IDR
       │
       └──► RTMPS (pending): → YouTube Live / Twitch
```

### Crate Structure

| Crate | Purpose |
|-------|---------|
| `g9-core` | Shared types, H.264 helpers, config, metrics |
| `g9-capture` | DXGI Desktop Duplication (Windows) |
| `g9-convert` | GPU BGRA→NV12 via D3D11 Video Processor |
| `g9-encode` | NVENC H.264 via bindgen + vendored header |
| `g9-audio` | WASAPI capture → Opus encode |
| `g9-webrtc` | WebRTC transport (webrtc-rs 0.11) |
| `g9-rtmp` | RTMPS transport (pending test) |
| `g9-stream` | Binary: CLI, pipeline wiring, metrics |

---

## Pending

| Item | Status | Notes |
|------|--------|-------|
| RTMPS → YouTube Live | Code written, untested | Needs a YouTube stream key |
| Multi-viewer load test | Not started | Single viewer proven |
| Mode B (dual encoder) | Code written, untested | Single shared encoder proven |
| Per-session capture | Not implemented | Required for multi-gamer streaming |
| Virtual display driver | Not installed | Required for this specific VM |
| Visual proof (non-black) | Blocked on display | Engine is correct; needs a GPU display surface |

---

## Build & Run

```bat
# One-time setup
setup.bat

# Build
build.bat

# Run (match --width/--height to the actual display)
set G9_PUBLIC_IP=<vm-ip>
target\release\glitch9-stream.exe --bind 0.0.0.0 --port 8080 --display 0 --width 1440 --height 900

# Diagnostic: dump one captured frame
target\release\glitch9-stream.exe --display 0 --dump-frame capture.ppm

# List displays
target\release\glitch9-stream.exe --list-displays
```

---

## Key Commits

| Hash | Description |
|------|-------------|
| `cdaa56a` | Install rustls ring CryptoProvider (DTLS fix) |
| `047f35b` | Keyframe on demand: PLI/FIR handling + force IDR on connect |
| `ec2b29c` | SDP profile-level-id=64002a (match NVENC High profile) |
| `17ada2e` | repeatSPSPPS=1 so every IDR carries SPS/PPS |
| `a07c449` | build.bat auto-discard Cargo.lock before git pull |
| `2d3ebdb` | Print git commit at startup for deploy verification |
| `d68058a` | Geometry mismatch warning |
| `1b555a0` | --dump-frame diagnostic tool |

---

## Conclusion

The glitch9-stream engine is **proven end-to-end** on real NVIDIA RTX PRO 4000
hardware: a gamer's GPU-rendered session was captured, converted (0 CPU copies),
NVENC-encoded, and streamed over WebRTC across the public internet to a remote
browser, rendering live at 1920×1080 with 0% packet loss.

Deployment requires launching the engine inside each target session as SYSTEM
(matching the production RhinoStream model), fps-limited encoding for stable
bitrate, and the corrected viewer track handling — all now in the codebase.

### Multi-session broadcast (one broadcast per gamer session)

glitch9-stream is the **spectator/broadcast** service running ALONGSIDE
RhinoStream (which serves the player). Validated: RhinoStream and glitch9-stream
capture the same session concurrently (DXGI Desktop Duplication supports multiple
duplication clients) at only ~+2-4% NVENC added.

Because duplication only captures its own session, broadcasting N sessions = N
engine instances, one launched inside each session on its own port. The native
**`glitch9-manager`** binary (crate `g9-manager`) enumerates active gamer sessions
via the WTS API and launches one engine per session **directly in-session** using
the session's user token (`WTSQueryUserToken` + `CreateProcessAsUserW`) — no
PsExec, no PowerShell. Deterministic port = `base_port + session_id`.

Two hard-won deployment details:
- `WTSQueryUserToken` needs SE_TCB privilege (SYSTEM-only), so launching into other
  sessions requires SYSTEM. To let a **plain admin (g9admin) deploy and operate**
  without PsExec, the manager has a one-time `deploy` that registers a SYSTEM
  scheduled task (`glitch9-broadcast`) running `start-system` with the persisted
  config. Then `start` (as any admin) auto-triggers that task; it runs directly
  when already SYSTEM. Verified: g9admin ran `deploy` then `start` and all 5
  sessions went LIVE.
- Gamer accounts are **blocked from running cmd.exe** by group policy
  (0x800704EC), so the manager launches the engine **.exe directly** — injecting
  `G9_PUBLIC_IP` into a rebuilt environment block and redirecting stdout/stderr to
  the per-session log via an inheritable file handle (no shell).

Operator flow (as g9admin, no SYSTEM shell / no PsExec):
```bat
glitch9-manager.exe deploy    REM one-time: register the SYSTEM watcher task
glitch9-manager.exe start     REM launch the watcher (auto-manages per-session)
glitch9-manager.exe status
glitch9-manager.exe stop       REM stop the watcher + all engines
```

**Orchestration-driven workers (watch mode).** The deployed task runs `watch` as
SYSTEM: a reconcile loop that spawns a broadcast worker when a session becomes
active and stops it when the session ends — so no GPU encoder is wasted on an idle
desktop. Two sources (`--source`):

- **orchestration (default, production):** driven by a per-session `broadcast.json`
  that Glitch9's session-api drops on the VM via the vm-agent — `start` on session
  provision, cleared on teardown. This mirrors the existing idle-watchdog /
  `.rhino_apikey` per-session config pattern. The orchestration side is wired in
  `glitch9-game-orchestration`: new vm-agent `place/clear-broadcast-config`
  endpoints + session-api `placeBroadcastConfig`/`clearBroadcastConfig` calls in
  `provisionSession`/`teardownSession`, gated by `BROADCAST_ENABLED`.
- **process (fallback):** detect a game via `WTSEnumerateProcessesW` (any session
  process not on the OS/shell/infra denylist counts as a game).

**No hardcoded IP (multi-VM).** `--public-ip` is optional; unset, the manager
auto-detects the VM's public IP (external echo `api.ipify.org`, then primary NIC).
One build deploys to any VM — verified it auto-detected `103.171.97.176` via ipify.

Verified on hardware (orchestration source): dropping `broadcast.json` for gamer1
only → watcher spawned **exactly one** broadcast (gamer1 LIVE), others stopped even
though gamer2/gamer3 had games running — proving the signal is the orchestration
file, not the process. Removing the file → watcher tore the worker down. Full
session start/end lifecycle driven by session-api.

Verified on hardware: **5 concurrent broadcasts** (gamer1→8082 … gamer5→8086),
5 engine processes, all ports LIVE. Sessions with active content capture at
~43-58fps; idle sessions show 0fps (DXGI delivers no frames when nothing changes)
and start automatically when motion appears.

**5-broadcast benchmark (shared with the live game + RhinoStream):**

| Metric | 1 broadcast | 5 broadcasts |
|--------|-------------|--------------|
| GPU SM % | 58-66% | **95-96%** (near saturation) |
| NVENC ENC % | 10-15% | **31-40%** |
| VRAM | ~8.8 GB | **~14.4 GB** / 24 GB |
| Engine RAM (total) | 61 MB | **~325 MB** (5× ~65 MB) |
| Encode latency | ~7 ms | **~9-12 ms** (GPU contention) |
| Actively-encoding sessions | 1 | 3 (others idle→0fps) |

Takeaway: ~5 broadcasts is viable on this card but near its ceiling — at 95% SM
with 3 active encoders *plus* the game *plus* RhinoStream, encode latency rises
from ~7ms to ~12ms. More concurrent *active* broadcasts would need a second GPU
(the VM has two) or lower per-stream settings (720p / lower fps). Idle sessions
cost almost nothing (no frames captured).

Firewall: broadcast ports need opening — `netsh advfirewall firewall add rule
name="glitch9-broadcast-tcp" dir=in action=allow protocol=TCP localport=8080-8090
profile=any` (UDP for ICE is covered by the per-program `glitch9-stream-udp` rule).

```bat
REM run the manager as SYSTEM (service, or via PsExec -s for testing):
glitch9-manager.exe start    # one engine per active gamer session
glitch9-manager.exe status   # list sessions + LIVE ports
glitch9-manager.exe stop     # stop all (does not need SYSTEM)
```

### Now also verified

- **Audio:** WASAPI loopback capture → Opus → WebRTC, playing smoothly in the
  remote browser. The VM's audio mix is 44.1kHz; the Opus encoder resamples to
  the 48kHz WebRTC requires (a sample-rate mismatch was causing choppy, ~9%-slow
  audio before the resampler was added). Viewer has a mute/unmute control
  (starts muted for autoplay, unmutes on the Watch-stream gesture).
- **Adaptive bitrate:** loss-based controller driven by RTCP Receiver Reports
  (EWMA-smoothed), reconfiguring NVENC at runtime via `nvEncReconfigureEncoder`
  with no session teardown. Observed on hardware stepping down on loss and
  ramping back up when the path cleared, clamped to [max/8, max].

Remaining POC work: RTMPS→YouTube output test (code written, needs a stream
key), multi-viewer load test, and a delay-based congestion signal (REMB /
transport-wide-cc) to complement the current loss-based controller. Audio
resampling uses a linear resampler (adequate for a POC; a polyphase/sinc
resampler would improve fidelity for production).
