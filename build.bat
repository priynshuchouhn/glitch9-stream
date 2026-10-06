@echo off
REM ============================================================================
REM  build.bat - Build glitch9-stream on a Windows + NVIDIA machine.
REM
REM  Sets up the MSVC environment (so the linker + Windows SDK headers are on
REM  PATH, and bindgen can find windows.h), points bindgen at libclang, then
REM  runs `cargo build --release`.
REM
REM  Usage:   build.bat            (release build)
REM           build.bat run        (build, then run a 1080p60 WebRTC test)
REM           build.bat pull       (git pull, then build)
REM           build.bat pull run   (git pull, build, run)
REM
REM  Prereqs (one-time): rustup (MSVC), VS2022 Build Tools (VCTools + Win SDK),
REM  LLVM, CMake, NASM, Git. See docs/RUN.md / the setup notes.
REM ============================================================================
setlocal EnableDelayedExpansion
cd /d "%~dp0"

REM --- libclang for bindgen (nvEncodeAPI.h) ---
if not defined LIBCLANG_PATH set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"

REM --- Locate and run vcvars64.bat (MSVC env) if the linker isn't already present ---
where /q link.exe
if errorlevel 1 (
    set "VCVARS="
    for %%P in (
        "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
        "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
        "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat"
        "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvars64.bat"
        "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
        "C:\Program Files (x86)\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
        "C:\Program Files (x86)\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat"
        "C:\Program Files (x86)\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvars64.bat"
    ) do (
        if exist %%P set "VCVARS=%%P"
    )
    if not defined VCVARS (
        echo [build] ERROR: vcvars64.bat not found. The VS2022 Build Tools are
        echo         installed but likely missing the C++ workload. Fix with:
        echo         Visual Studio Installer -^> Build Tools 2022 -^> Modify -^>
        echo         check "Desktop development with C++" -^> Install.
        echo         Or run setup.bat again as administrator.
        exit /b 1
    )
    echo [build] Initializing MSVC environment...
    call "!VCVARS!" >nul
)

REM --- Optional: git pull ---
echo.%*| findstr /i "pull" >nul
if not errorlevel 1 (
    echo [build] git pull...
    REM Cargo.lock is tracked but cargo may rewrite it locally on Windows, which
    REM makes `git pull` abort with "local changes would be overwritten". It's a
    REM generated file, so discard any local churn before pulling. (Only Cargo.lock
    REM is reset — your source edits are never touched.)
    git checkout -- Cargo.lock 2>nul
    git pull || (
        echo [build] git pull FAILED. Resolve manually, then re-run.
        exit /b 1
    )
    echo [build] now at:
    git log --oneline -1
)

REM --- Build ---
echo [build] cargo build --release ...
cargo build --release
if errorlevel 1 (
    echo [build] BUILD FAILED.
    exit /b 1
)
echo [build] OK -^> target\release\glitch9-stream.exe

REM --- Optional: run a quick WebRTC test ---
echo.%*| findstr /i "run" >nul
if not errorlevel 1 (
    echo [build] running WebRTC test ^(Ctrl-C to stop^)...
    target\release\glitch9-stream.exe --display 0 --output webrtc --width 1920 --height 1080 --fps 60 --bitrate 8000000 --log-level debug
)

endlocal
