@echo off
REM Build helper for the g9admin SSH session, where cargo lives under the
REM Administrator profile and the MSVC env isn't preloaded.
cd /d C:\glitch9-stream
set "PATH=C:\Users\Administrator\.cargo\bin;%PATH%"
REM rustup/cargo were installed under the Administrator profile; point the g9admin
REM SSH session at that toolchain + registry instead of g9admin's empty ~/.rustup.
set "RUSTUP_HOME=C:\Users\Administrator\.rustup"
set "CARGO_HOME=C:\Users\Administrator\.cargo"
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
REM Prebuilt static libvpx (vcpkg) for the facecam VP8 decoder (env-libvpx-sys).
REM The crate links `static=libvpx`, so we expose a `libvpx.lib` alongside vcpkg's
REM `vpx.lib`. VPX_VERSION selects the crate's pre-generated FFI (1.13.0); the
REM linked lib (1.16.0) is ABI-compatible for the decode functions we call.
set "VPX_LIB_DIR=C:\glitch9-stream\vendor\libvpx\lib"
set "VPX_INCLUDE_DIR=C:\vcpkg\installed\x64-windows-static\include"
set "VPX_VERSION=1.13.0"
set "VPX_STATIC=1"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
if not exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" (
    call "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
)
cargo build --release
