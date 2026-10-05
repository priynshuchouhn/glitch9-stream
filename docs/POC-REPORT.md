# Glitch9 Streaming Engine POC — Report

> Status: implementation complete; **runtime benchmarks pending the Windows+NVIDIA
> host**. Code authored and verified on macOS to the extent the toolchain allows
> (unit tests + native compile + Windows cross-check). No component is faked or
> replaced with a software encoder. Rows that require a GPU to measure are marked
> **[PENDING-HW]**.

## Final Architecture

```
                         Windows Game / Display
                                  │
                                  ▼
                     DXGI Desktop Duplication            (g9-capture)
                     IDXGIOutputDuplication → ID3D11Texture2D (BGRA, VRAM)
                                  │  one capture only
                                  ▼
                     D3D11 Video Processor                (g9-convert)
                     BGRA → NV12, GPU-resident, 0 CPU readback
                                  │
                                  ▼
                     NVENC H.264 (nvEncodeAPI64.dll)      (g9-encode)
                     registered D3D11 NV12 input (zero-copy)
                                  │
                  Mode A: 1 encoder   │   Mode B: 2 encoders (shared NV12 input)
                                  │
                        Arc<EncodedFrame> fan-out
                   ┌──────────────┴───────────────┐
                   ▼                               ▼
          WebRtcTransport (g9-webrtc)      RtmpTransport (g9-rtmp)
          H.264 RTP/SRTP via webrtc-rs     FLV mux → RTMP chunk → RTMPS(TLS)
          WS signaling + browser viewer    reconnect w/ bounded backoff
                   │                               │
                   ▼                               ▼
             Chrome/Edge <video>              YouTube Live

  Audio: WASAPI loopback (g9-audio) → one PCM stream
             ├── Opus  → WebRTC
             └── AAC   → FLV → RTMPS
```

Single unified `PtsClock` drives both video and audio timestamps (RTP 90 kHz, FLV ms).

## Files Created

| Crate / file | Role |
|---|---|
| `crates/g9-core` | Shared types: `EncodedFrame`, `ParameterSets`, `VideoCodec`; `h264` NAL/SPS-PPS/AVCC helpers; `PtsClock`+`DriftTracker`; `EncoderProfile` (+`compatible_with` for Mode A/B); `MediaTransport` trait, `AudioPacket`, `TransportStats`; `config` (`Outputs`, `VideoConfig`, `RtmpConfig`+`Secret`, `AudioConfig`); `metrics`. |
| `crates/g9-capture` | `windows_impl.rs` D3D11 device + adapter/display enum + DXGI Desktop Duplication; non-Windows stub. |
| `crates/g9-convert` | `windows_impl.rs` D3D11 Video Processor BGRA→NV12 (GPU-resident); stub. |
| `crates/g9-encode` | `nvenc_ffi.rs` hand FFI to `nvEncodeAPI`; `nvenc.rs` encoder (register/map D3D11 NV12, encode, lock bitstream); `h264` re-export; stub. |
| `crates/g9-audio` | `asc.rs` AAC AudioSpecificConfig; `wasapi.rs` loopback capture; `opus_enc.rs` Opus; `aac.rs` MF AAC; stub. |
| `crates/g9-webrtc` | `packetizer.rs` RFC 6184 (single-NAL/FU-A/SPS-PPS); `signaling.rs` WS signaling + embedded viewer HTTP; `transport.rs` webrtc-rs peer connection + tracks. |
| `crates/g9-rtmp` | `flv.rs` FLV mux; `amf0.rs`; `chunk.rs` RTMP chunk writer; `handshake.rs`; `client.rs` connect/publish (TCP/TLS); `transport.rs` publisher + reconnect. |
| `crates/g9-stream` | `cli.rs` (clap) + `pipeline.rs` (capture→convert→encode→fan-out, audio thread, metrics, shutdown). Binary `glitch9-stream`. |
| `web/index.html` | Minimal browser viewer (view-only, stats overlay). |
| `scripts/benchmark.ps1`, `scripts/summarize.ps1` | Test A–D harness + nvidia-smi dmon capture + summary table. |
| `ANALYSIS.md`, `docs/BUILD.md`, `docs/RUN.md`, this report | Docs. |

## DXGI Capture

`IDXGIOutputDuplication::AcquireNextFrame` returns the desktop as an
`ID3D11Texture2D` in VRAM. Handling: `DXGI_ERROR_WAIT_TIMEOUT` → no-op/retry;
`DXGI_ERROR_ACCESS_LOST` (mode change / display reconnect) → `CaptureReinit` so the
pipeline rebuilds duplication without crashing; `E_ACCESSDENIED` (secure desktop /
fullscreen-exclusive transition) → surfaced as a retryable error.

## D3D11 Processing

BGRA→NV12 uses the `ID3D11VideoProcessor` fixed-function converter. The NV12 output
texture is allocated **once** and reused; it is created with `CPUAccessFlags = 0`.

**CPU video readbacks / frame = 0.** The pixels never leave the GPU: capture →
`VideoProcessorBlt` → NVENC-registered texture. We never call `Map()` on the frame.
The `cpu_readbacks` metric counter exists specifically to catch regressions and is
expected to read 0 at all times.

## NVENC

| Setting | WebRTC profile | YouTube profile |
|---|---|---|
| Codec | H.264 | H.264 |
| Preset | P4 | P5 |
| Tuning | low_latency | high_quality |
| Rate control | CBR | CBR |
| Bitrate | 8 Mbps (4–12 configurable) | 8 Mbps |
| GOP (keyframe interval) | 4 s (on-demand IDR on viewer join) | 2 s (YouTube requirement) |
| B-frames | 0 | 0 |
| Lookahead | off | off |
| Input | registered D3D11 NV12 (zero-copy) | same |

NVENC is reached by loading `nvEncodeAPI64.dll` (driver-provided) and calling
`NvEncodeAPICreateInstance`. There is **no x264/x265 fallback**; if NVENC init fails
the engine reports the error. SPS/PPS are emitted on keyframes
(`NV_ENC_PIC_FLAG_OUTPUT_SPSPPS`) and cached for RTP out-of-band config and the FLV
AVCDecoderConfigurationRecord.

## WebRTC

- Codec negotiation: H.264 (payload type 102, `packetization-mode=1`,
  `profile-level-id=42e01f`) + Opus (111), registered in the webrtc-rs `MediaEngine`.
- RTP packetization: our tested packetizer implements single-NAL and FU-A with
  SPS/PPS prepended on keyframes; webrtc-rs performs the on-wire RTP+SRTP.
- ICE: STUN (`stun.l.google.com:19302`), trickle both ways. No TURN (POC).
- Signaling: local WebSocket on `127.0.0.1:8080` (bindable to LAN). Media never
  flows through signaling.
- Late viewer: a new viewer triggers `force_idr` on the encoder so an IDR + SPS/PPS
  arrive promptly.
- Browser: plain `<video autoplay playsinline>` in Chrome/Edge; view-only.

## YouTube

- H.264 (AVCC) + AAC muxed into FLV tags, sent as RTMP video/audio messages.
- RTMP: simple handshake → `connect` → `releaseStream`/`FCPublish` → `createStream`
  → `publish`, chunked with a 4096-byte chunk size.
- RTMPS: TLS via rustls over the same path.
- Reconnect: bounded exponential backoff (1→2→4→8→16 s). After reconnect the AVC/AAC
  sequence headers are re-sent and delta frames wait for the next keyframe.
- Stream key: used only as the publish name; never logged (redaction unit-tested).

## Audio

- WASAPI loopback capture of the render endpoint (what the game outputs), captured
  **once**.
- Opus (libopus, 20 ms frames) → WebRTC.
- AAC-LC (Media Foundation) → FLV/RTMPS; AudioSpecificConfig sent as the FLV AAC
  sequence header (unit-tested).
- A/V sync: one `PtsClock`; video and audio timestamps derive from it. `DriftTracker`
  reports audio-vs-video drift.

## Simultaneous Streaming — one NVENC or two?

The pipeline chooses at startup via `EncoderProfile::compatible_with`:

- **Mode A (one NVENC)** when the WebRTC and YouTube requests share
  resolution/fps/GOP/B-frames and bitrate band. One encode, fanned out to both
  transports. Lowest cost.
- **Mode B (two NVENC)** when the profiles differ (e.g. WebRTC low-latency 4 s GOP
  vs YouTube 2 s GOP). Two encoders, but they consume the **same** captured+converted
  NV12 texture — capture and color-convert still happen once. The display is never
  captured twice.

With the current defaults (WebRTC 4 s GOP, YouTube 2 s GOP) the two profiles are
**not** compatible, so both-outputs runs in Mode B. If you set both to the same GOP,
it collapses to Mode A. The active mode is logged at startup.

## Performance Comparison

**[PENDING-HW]** — produced by `scripts/benchmark.ps1` + `summarize.ps1` on the
Windows+NVIDIA host. Table shape (values filled in from measured `nvidia-smi dmon` +
process counters; no fabricated numbers):

| Metric | Idle | WebRTC | YouTube | WebRTC + YouTube (shared) | WebRTC + YouTube (dual) |
|---|---:|---:|---:|---:|---:|
| CPU % | | | | | |
| RAM MB | | | | | |
| GPU SM % | | | | | |
| NVENC % | | | | | |
| GPU MEM % | | | | | |
| Capture FPS | | | | | |
| Encode FPS | | | | | |
| VM outbound Mbps | | | | | |
| Dropped frames | | | | | |

Expectation to validate (from the RhinoStream investigation on the same GPU class):
per-stream CPU should be ~0 (hardware encode), with NVENC the shared bottleneck and
game rendering dominating GPU SM. Network is **not** shared across destinations —
each of WebRTC and YouTube needs its own ~bitrate of upstream (≈2× bitrate for both).

## Known Issues

1. **NVENC input registration per frame.** `encode()` registers/unregisters the NV12
   texture each call. It's correct but a known optimization is register-once/cache
   (the converter reuses one texture). Low effort, do before perf runs.
2. **MF AAC MFT loop is PENDING-HW.** `g9-audio/aac.rs` builds the ASC (tested) and
   wires the MFT, but the `ProcessInput/ProcessOutput` drain is validated only on the
   target. If MF proves awkward, FDK-AAC (bundled by fmedia on the gaming VMs) is a
   drop-in alternative.
3. **Opus/TLS native builds** need autotools/CMake on the build host (fine on
   Windows; the reason the Mac can't fully link).
4. **Test D (forced dual)** currently uses the same CLI as Test C; to force Mode B
   the harness should request mismatched GOPs. Documented in `benchmark.ps1`.
5. **No HTTP range / multi-file serving** in the viewer server — it serves the single
   embedded `index.html`. Sufficient for the POC.

## Next Steps (evaluate, do NOT implement now)

1. SFU for 1-upload→N-viewers fan-out.
2. TURN for restrictive-NAT viewers.
3. Glitch9 NestJS signaling/control integration (replace local WS; vm-agent-style
   control surface).
4. User authentication on signaling.
5. Per-gamer Windows session workers (one engine per session, like RhinoStream).
6. Adaptive bitrate (react to WebRTC RTCP loss / RTMP backpressure).
7. Twitch/Facebook/SRT/recording outputs (the `MediaTransport` trait already allows this).
8. Production Windows service packaging.
9. Shared D3D11 texture with an existing gameplay encoder (capture once, encode for
   play + broadcast) — the biggest efficiency win, per the RhinoStream findings.
10. Production deployment.
```
