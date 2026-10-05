# glitch9-stream

A lightweight, headless game-streaming engine for Glitch9. Captures a Windows
display on the GPU, encodes with NVIDIA NVENC H.264, and routes the result to two
destinations:

1. **WebRTC → browser** (view-only live stream)
2. **RTMPS → YouTube Live** (broadcast)

The design goal is "capture once on the GPU, encode efficiently with NVENC, route to
the required destination" — not an OBS clone. Capture and color conversion happen
once; encoded H.264 is fanned out to every enabled transport.

```
Game → DXGI Desktop Duplication → D3D11 texture → GPU BGRA→NV12 → NVENC H.264
                                                                      │
                                                      ┌───────────────┴───────────────┐
                                                      ▼                               ▼
                                                 WebRTC (browser)              RTMPS (YouTube)
```

- **Target:** Windows x86_64 + NVIDIA GPU. Builds/links there only.
- **No software-encoder fallback.** If NVENC is unavailable, the engine errors out.
- **View-only WebRTC:** no keyboard/mouse/controller, no data channel.

## Layout

| Crate | Responsibility |
|---|---|
| `g9-core` | shared types, H.264 bitstream helpers, PTS clock, encoder profiles, `MediaTransport` trait, config, metrics |
| `g9-capture` | D3D11 + DXGI Desktop Duplication |
| `g9-convert` | GPU BGRA→NV12 (D3D11 Video Processor) |
| `g9-encode` | NVENC H.264 (zero-copy D3D11 input) |
| `g9-audio` | WASAPI capture, Opus, AAC |
| `g9-webrtc` | H.264 RTP + webrtc-rs + local signaling + viewer |
| `g9-rtmp` | FLV mux + RTMP/RTMPS publish |
| `g9-stream` | CLI + pipeline wiring (binary `glitch9-stream`) |

## Docs

- [`ANALYSIS.md`](ANALYSIS.md) — repository analysis + proposed architecture (Milestone 1)
- [`docs/BUILD.md`](docs/BUILD.md) — prerequisites + build + verification status
- [`docs/RUN.md`](docs/RUN.md) — CLI, examples, display listing, metrics
- [`docs/POC-REPORT.md`](docs/POC-REPORT.md) — final architecture, decisions, perf table, known issues, next steps

## Quick start (on the Windows+NVIDIA host)

```bat
cargo build --release
target\release\glitch9-stream.exe --list-displays
target\release\glitch9-stream.exe --display 0 --output webrtc
rem browser: http://127.0.0.1:8080/
```

See [`docs/RUN.md`](docs/RUN.md) for YouTube and simultaneous modes.
