@echo off
REM ============================================================================
REM  setup.bat - One-time install of build prerequisites for glitch9-stream on a
REM  Windows + NVIDIA VM, via winget:
REM    Rust rustup MSVC toolchain; VS2022 Build Tools VCTools + Win11 SDK;
REM    LLVM libclang for bindgen; CMake; NASM; Git.
REM  Does NOT touch the NVIDIA driver.
REM  Run in an ELEVATED prompt. Afterwards open a NEW shell, then run build.bat.
REM ============================================================================
setlocal

REM --- Require admin ---
net session >nul 2>&1
if errorlevel 1 goto no_admin

REM --- winget must be available ---
where /q winget
if errorlevel 1 goto no_winget

set "WG=winget install -e --accept-source-agreements --accept-package-agreements"

echo [setup] Installing Rust rustup MSVC...
%WG% --id Rustlang.Rustup

echo [setup] Installing/modifying VS2022 Build Tools VCTools + Win11 SDK...
REM NOTE: the VS Installer 'modify' verb does NOT support --wait; it runs the
REM install in the background. We poll for vcvars64.bat afterwards.
set "VSWORKLOADS=--add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --quiet --norestart"
set "VSINSTALLER=C:\Program Files (x86)\Microsoft Visual Studio\Installer\setup.exe"
set "VSBT=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
if exist "%VSBT%" goto vs_modify
%WG% --id Microsoft.VisualStudio.2022.BuildTools --override "%VSWORKLOADS%"
goto vs_done
:vs_modify
echo [setup] Build Tools present; adding C++ workload via VS installer modify...
"%VSINSTALLER%" modify --installPath "%VSBT%" %VSWORKLOADS%
:vs_done

REM --- Wait for the C++ workload to finish (install runs in background) ---
set "VCVARS=%VSBT%\VC\Auxiliary\Build\vcvars64.bat"
echo [setup] Waiting for C++ workload (vcvars64.bat) to appear...
set /a _tries=0
:wait_vcvars
if exist "%VCVARS%" goto vcvars_ok
set /a _tries+=1
if %_tries% GEQ 60 goto vcvars_timeout
timeout /t 20 >nul
goto wait_vcvars
:vcvars_ok
echo [setup] C++ workload ready.
goto post_vs
:vcvars_timeout
echo [setup] WARNING: vcvars64.bat not found yet. The install may still be running.
echo         Re-check later, or use the Visual Studio Installer GUI to add
echo         "Desktop development with C++".
:post_vs

echo [setup] Installing LLVM libclang...
%WG% --id LLVM.LLVM

echo [setup] Installing CMake...
%WG% --id Kitware.CMake

echo [setup] Installing NASM...
%WG% --id NASM.NASM

echo [setup] Installing Git...
%WG% --id Git.Git

echo [setup] Setting LIBCLANG_PATH...
setx /M LIBCLANG_PATH "C:\Program Files\LLVM\bin" >nul

where /q rustup
if not errorlevel 1 rustup default stable-x86_64-pc-windows-msvc 2>nul

echo:
echo [setup] Done. Open a NEW terminal so PATH changes apply, then run:  build.bat
goto end

:no_admin
echo [setup] ERROR: run this script as administrator.
goto end

:no_winget
echo [setup] ERROR: winget not found. Install App Installer from the Microsoft Store.
goto end

:end
endlocal
