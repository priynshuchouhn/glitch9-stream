@echo off
REM ============================================================================
REM  setup.bat - One-time install of build prerequisites for glitch9-stream on a
REM  Windows + NVIDIA VM. Installs via winget:
REM    - Rust (rustup, MSVC toolchain)
REM    - Visual Studio 2022 Build Tools (VCTools + Windows 11 SDK)  [linker + windows.h]
REM    - LLVM (libclang, for bindgen -> nvEncodeAPI.h)
REM    - CMake + NASM (for aws-lc / libopus native builds)
REM    - Git
REM
REM  Does NOT install or touch the NVIDIA driver (already present on the gaming VM
REM  and required by the rest of the stack).
REM
REM  Run in an ELEVATED prompt (Run as administrator). After it finishes, open a
REM  NEW shell (so PATH updates apply) and use build.bat.
REM ============================================================================
setlocal EnableDelayedExpansion

REM --- Require admin (winget machine-scope installs need it) ---
net session >nul 2>&1
if errorlevel 1 (
    echo [setup] ERROR: please run this script "as administrator".
    exit /b 1
)

REM --- winget must be available ---
where /q winget
if errorlevel 1 (
    echo [setup] ERROR: winget not found. Install "App Installer" from the Microsoft
    echo         Store, or install the tools manually (see docs/BUILD.md).
    exit /b 1
)

set "WG=winget install -e --accept-source-agreements --accept-package-agreements"

echo [setup] Installing Rust (rustup, MSVC)...
%WG% --id Rustlang.Rustup

echo [setup] Installing Visual Studio 2022 Build Tools (VCTools + Windows 11 SDK)...
%WG% --id Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.22621"

echo [setup] Installing LLVM (libclang)...
%WG% --id LLVM.LLVM

echo [setup] Installing CMake...
%WG% --id Kitware.CMake

echo [setup] Installing NASM...
%WG% --id NASM.NASM

echo [setup] Installing Git...
%WG% --id Git.Git

REM --- Persist LIBCLANG_PATH for bindgen (machine scope) ---
echo [setup] Setting LIBCLANG_PATH...
setx /M LIBCLANG_PATH "C:\Program Files\LLVM\bin" >nul

REM --- Ensure the MSVC default Rust toolchain is selected ---
where /q rustup
if not errorlevel 1 (
    rustup default stable-x86_64-pc-windows-msvc 2>nul
)

echo.
echo [setup] Done.
echo [setup] IMPORTANT: open a NEW terminal so PATH changes take effect, then run:
echo             build.bat
echo [setup] (build.bat loads the MSVC environment for you automatically.)
endlocal
