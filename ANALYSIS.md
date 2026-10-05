# Glitch9 Streaming Engine — Milestone 1: Repository Analysis

> Authored on macOS (Apple Silicon). The engine targets **Windows x86_64 + NVIDIA**.
> It cannot be compiled or run on this Mac. Build/test happens on a Windows + NVIDIA
> machine (e.g. an RTX PRO 4000 VM). Every component is real — nothing is faked or
> stubbed with a software substitute. Where a thing cannot be verified without the
> target hardware, it is marked **[PENDING-HW]**.

---

## Existing Architecture

The repository `glitch9-main` is the Glitch9 cloud-gaming product. Relevant parts:

| Area | Tech | Role |
|---|---|---|
| `glitch9-be` | NestJS (TypeScript) | Main backend API |
| `glitch9-fe` | Next.js (TypeScript) | Player-facing web app |
| `glitch9-admin-fe` | React + Vite | Admin console |
| `glitch9-game-orchestration/session-api` | Node/TypeScript | Session scheduler, GPU-budget enforcement, talks to vm-agent |
| `glitch9-game-orchestration/vm-agent` | **Go** (Windows binary) | Per-VM HTTP control/metrics on port 9090 (nvidia-smi, slots, game control) |
| `glitch9-game-orchestration/game-guard`, `idle-watchdog`, `game-launcher-*`, `key-blocker`, `session-keeper-onprem` | **Go** | Per-VM helper daemons |
| Streaming engine on the VMs | **RhinoStream** (Rust, 3rd-party) + Sunshine (older setups) | Actual capture/encode/transport — investigated separately |

Key facts established from the codebase and the RhinoStream investigation:

- The production streaming engine (**RhinoStream**) is a closed, third-party Rust binary. We do **not** modify, hook, inject, or depend on it. This project is fully independent.
- The orchestration tooling is **Go and TypeScript**. There is **no existing Rust code** in the repo.
- There is **no reusable media/capture/encode code**: every `dxgi`/`d3d11`/`nvenc`/`webrtc`/`rtmp`/`wasapi` hit in the repo is either documentation about Sunshine/RhinoStream, or Go/TS code that *observes* those processes (e.g. `vmAgent.ts` reading UDP endpoints, `scheduler.ts` enforcing a GPU budget).
- **Runtime prerequisite discovered in the runbooks:** on Windows Server RDSH / disconnected-RDP sessions, DXGI only exposes the "Microsoft Remote Display Adapter" (software) unless `UseWddmDriver=1` (+ `bEnumerateHWBeforeSW=1`) is set under `HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services`. Without it, per-session DXGI Desktop Duplication will not see a GPU-backed output and NVENC can't be fed from the desktop. This engine requires a real GPU-backed output (physical console, a virtual display, or a WDDM-enabled RDP session).

## Reusable Components

Honest answer: **almost nothing at the code level** — this is a greenfield Rust engine. What we reuse are *conventions and integration seams*, not code:

- **Naming / layout convention:** `glitch9-*` crate/dir naming, YAML + env-var config (matches `rhino_stream_service.yml` and the deploy scripts).
- **Control-surface pattern:** `vm-agent` exposes a bearer-token HTTP API on a private port for `session-api` to drive. Our engine is a CLI today, but we keep a clean control boundary so a future `vm-agent`-style HTTP/WS control layer can start/stop outputs without touching the media path. (Future work, not this milestone.)
- **GPU-budget awareness:** `session-api`'s per-server GPU weight model is why "how expensive is the extra YouTube output" matters — our metrics/benchmarks feed that conversation.
- **Reference architecture:** the RhinoStream findings (DXGI Desktop Duplication → D3D11 → GPU convert → NVENC H.264 → WebRTC; WASAPI → Opus; ~0 CPU encode) validate the target pipeline. We replicate the *approach*, not the code.

## Missing Components (everything in this POC is new)

1. Rust workspace targeting `x86_64-pc-windows-msvc`.
2. D3D11 device + adapter/display enumeration (prefer NVIDIA).
3. DXGI Desktop Duplication capture with robust `AcquireNextFrame` error handling.
4. GPU BGRA→NV12 color conversion (D3D11 VideoProcessor or compute shader), GPU-resident.
5. NVENC H.264 encoder with **D3D11 input resource registration** (zero-copy, no CPU readback).
6. Encoder-profile abstraction (WebRTC low-latency vs YouTube broadcast).
7. Encoded-frame fan-out (`Arc<EncodedFrame>`) with bounded, drop-stale queues (backpressure).
8. `MediaTransport` trait + two implementations:
   - **WebRTC**: H.264 RTP packetization (single-NAL / FU-A, SPS/PPS, IDR), DTLS-SRTP, local signaling, browser viewer.
   - **RTMP/RTMPS**: handshake, publish, FLV mux (AVC/AAC sequence headers + timestamps), TLS, reconnect.
9. WASAPI audio capture (loopback), encoded once to **Opus** (WebRTC) and **AAC** (YouTube).
10. Unified PTS/timing model + A/V drift reporting.
11. Metrics snapshot every N seconds (capture/encode FPS, latencies, CPU/RAM, per-transport stats, drops).
12. CLI: `--list-displays`, `--output webrtc,youtube`, `--rtmp-url`, `--stream-key` (never logged), geometry/bitrate/audio flags.
13. Benchmark harness (Test A–D) + `nvidia-smi dmon` capture.

## Proposed Architecture

Cargo workspace, one responsibility per crate so capture/convert/encode are not duplicated and transports are swappable:

```
glitch9-stream/                 (Cargo workspace)
├─ crates/
│  ├─ g9-core        # shared types: EncodedFrame, VideoFormat, PtsClock, config, errors, traits (MediaTransport, FrameSink)
│  ├─ g9-capture     # D3D11 device/adapter/display enum + DXGI Desktop Duplication  [Windows]
│  ├─ g9-convert     # GPU BGRA->NV12 (D3D11 VideoProcessor / compute shader)        [Windows]
│  ├─ g9-encode      # NVENC H.264 (D3D11 input registration), encoder profiles      [Windows+NVIDIA]
│  ├─ g9-audio       # WASAPI capture -> PCM, Opus + AAC encoders                     [Windows]
│  ├─ g9-webrtc      # WebRTC transport: RTP H.264 packetization, SRTP, signaling
│  ├─ g9-rtmp        # RTMP/RTMPS transport: handshake, FLV mux, publish, reconnect
│  └─ g9-stream      # binary: CLI, pipeline wiring, fan-out, metrics, lifecycle
├─ web/              # minimal browser viewer (static HTML/JS) + local signaling served by g9-stream
├─ scripts/          # benchmark harness (Test A-D), nvidia-smi dmon capture
├─ docs/             # run guide, build guide, POC report
├─ ANALYSIS.md       # this file
└─ Cargo.toml        # workspace manifest
```

Data flow (single capture, shared GPU frame, fan-out of encoded frames):

```
DXGI Desktop Duplication ─► ID3D11Texture2D (BGRA, GPU)
        │  (one capture only)
        ▼
GPU BGRA ─► NV12 (D3D11, GPU-resident, 0 CPU readback)
        │
        ▼
NVENC H.264  ── Mode A: one encoder when WebRTC+YouTube settings are compatible
   │           Mode B: two NVENC sessions (shared NV12 input) when profiles differ
   ▼
Arc<EncodedFrame>  ──► bounded queue ──► WebRtcTransport ──► browser
        └───────────► bounded queue ──► RtmpTransport  ──► YouTube (RTMPS)

WASAPI (one capture) ─► PCM ─┬─► Opus ─► WebRTC
                             └─► AAC  ─► FLV ─► RTMPS
```

**Shared vs dual encoder decision (Mode A/B):** choose Mode A (single NVENC) only when the WebRTC and YouTube requested configs are compatible (same resolution/fps/bitrate band and both tolerate the chosen GOP/B-frame settings). Otherwise Mode B spins up a second NVENC session **that reuses the same captured + converted NV12 texture** — capture and color-convert happen once regardless. We never do two DXGI captures.

## Implementation Plan

Follows the spec's milestone order. Compile + test after each on the Windows+NVIDIA box.

1. **M1** Analysis (this doc).
2. **M2** Workspace builds (all crates, trait skeletons, CLI parses).
3. **M3** D3D11 + DXGI capture; `--list-displays`; robust `AcquireNextFrame`.
4. **M4** GPU BGRA→NV12; assert 0 full-frame CPU readbacks.
5. **M5** NVENC H.264 from registered D3D11 texture; verify 1080p60, low CPU, 0 readback. **Gate: do not proceed until green.**
6. **M6** WebRTC transport + signaling + browser viewer.
7. **M7** RTMP/RTMPS + FLV mux; YouTube receives video.
8. **M8** WASAPI capture.
9. **M9** Opus → WebRTC.
10. **M10** AAC → YouTube.
11. **M11** Simultaneous WebRTC + YouTube (Mode A and Mode B).
12. **M12** Metrics.
13. **M13** Reconnect / error isolation / backpressure.
14. **M14** Benchmarks (Test A–D) + `nvidia-smi dmon`.
15. **M15** Documentation + POC report.

### Dependency choices (all real, no software-encoder shortcuts)

- **Windows APIs:** `windows` crate (official Microsoft bindings) for D3D11/DXGI/WASAPI/Media Foundation (AAC).
- **NVENC:** direct FFI to `nvEncodeAPI` via the NVIDIA Video Codec SDK headers (thin bindgen or hand-written FFI). NVENC takes a registered `ID3D11Texture2D` as input → zero-copy. No FFmpeg, no x264/x265 in the video path.
- **WebRTC:** `webrtc` crate (webrtc-rs) for PeerConnection/DTLS-SRTP/ICE + manual H.264 RTP payloader (`webrtc::rtp`/`rtp` crate) so we control SPS/PPS/FU-A.
- **RTMP:** a Rust RTMP client + hand-written FLV muxer (AVC/AAC sequence headers). TLS via `rustls`/`tokio-rustls` for RTMPS.
- **Opus:** `audiopus`/`opus` crate (libopus binding).
- **AAC:** Media Foundation AAC encoder via `windows` crate (no extra native dep on Windows), with FDK-AAC as an alternative if MF proves awkward. Documented either way.
- **Async/runtime:** `tokio`. **CLI:** `clap`. **Logging:** `tracing` (+ stream-key redaction). **Config:** `serde` + YAML/env.

> Note on NVENC-on-WDDM telemetry: as found in the investigation, `nvidia-smi pmon` and per-process VRAM are unavailable under WDDM, and `encoder.stats.sessionCount` may read 0 while `utilization.encoder` is non-zero. Benchmarks will rely on `nvidia-smi dmon -s u` (direct ENC block) + our own in-process latency/FPS counters, and will say so.
