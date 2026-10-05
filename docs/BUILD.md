# Build Guide — Glitch9 Streaming Engine

> This engine targets **Windows x86_64 + NVIDIA**. It is authored and partially
> verified on macOS (see "Verification status"), but it links and runs only on a
> Windows machine with an NVIDIA GPU.

## Prerequisites (Windows build host)

| Requirement | Why |
|---|---|
| Windows 10/11 or Server 2022/2025 | DXGI Desktop Duplication, WASAPI, Media Foundation |
| NVIDIA GPU + recent driver | NVENC via `nvEncodeAPI64.dll` (ships with the driver) |
| Rust (stable) + `x86_64-pc-windows-msvc` toolchain | `rustup toolchain install stable-msvc` |
| Visual Studio Build Tools (MSVC + Windows 10/11 SDK) | linker + system libs |
| CMake + a C compiler | builds `aws-lc`/`ring` (TLS for RTMPS) and `libopus` |
| Git | fetch crates |

> NVENC note: you do **not** need the NVIDIA Video Codec SDK installed to build —
> the engine loads `nvEncodeAPI64.dll` at runtime from the driver and binds it via
> hand-written FFI. You only need the driver present at run time.

## Build

```bat
rem from the workspace root
cargo build --release
```

The binary lands at `target\release\glitch9-stream.exe`.

Run the unit tests (bitstream/mux/protocol logic — host-independent):

```bat
cargo test --workspace
```

## Verification status (as authored on macOS)

The code is split so platform-independent logic is fully tested on any host, and the
Windows/NVIDIA code is cross-checked where the toolchain allows:

| Component | How verified on macOS | Needs Windows+GPU to run |
|---|---|---|
| H.264 NAL/SPS-PPS/AVCC, RTP packetizer, FLV mux, AMF0, RTMP chunk, RTMP URL, AAC ASC, backoff | **Unit tested** (`cargo test`, 20 tests) | no |
| `g9-webrtc` (webrtc-rs peer connection, tracks, signaling) | **Compiles natively** on macOS | runs anywhere, but fed by NVENC on target |
| `g9-rtmp` (incl. rustls TLS) | **Compiles natively** on macOS | — |
| `g9-capture` (D3D11 + DXGI Desktop Duplication) | **`cargo check --target x86_64-pc-windows-msvc` passes** | yes |
| `g9-convert` (D3D11 Video Processor BGRA→NV12) | **Windows cross-check passes** | yes |
| `g9-encode` (NVENC FFI + encoder) | **Windows cross-check passes** | yes (NVIDIA GPU) |
| `g9-audio` (WASAPI capture, MF AAC) | authored; the MF AAC `ProcessInput/Output` loop is **PENDING-HW** | yes |

Items marked **PENDING-HW** compile but their runtime behavior must be validated on
the Windows+NVIDIA target. There are **no** fake encoders or software substitutes —
if NVENC is unavailable, the engine returns an error rather than falling back.
