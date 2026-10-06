@echo off
REM ============================================================================
REM  clean.bat - Tear down glitch9-stream build artifacts / toolchain on a VM.
REM
REM  Levels:
REM    clean.bat            Level 1: stop the engine + remove build output
REM                         (target\) and the CI-copied folder. Keeps toolchain.
REM    clean.bat toolchain  Level 1 + remove the Rust toolchain + cargo cache.
REM    clean.bat all        Level 2 + uninstall VS Build Tools / LLVM / CMake /
REM                         NASM (via winget). Does NOT touch the NVIDIA driver,
REM                         C:\glitch9-prod (RhinoStream) or C:\glitch9.
REM
REM  Each destructive step asks for confirmation.
REM ============================================================================
setlocal EnableDelayedExpansion
cd /d "%~dp0"

echo [clean] Stopping glitch9-stream if running...
taskkill /IM glitch9-stream.exe /F >nul 2>&1

REM --- Level 1: build artifacts ---
call :confirm "Remove build output (target\ in this repo)?"
if "!YES!"=="1" (
    if exist "target" rmdir /s /q "target" && echo [clean] removed target\
)

call :confirm "Remove the CI-copied folder C:\glitch9-stream-windows-x64 (if present)?"
if "!YES!"=="1" (
    if exist "C:\glitch9-stream-windows-x64" rmdir /s /q "C:\glitch9-stream-windows-x64" && echo [clean] removed C:\glitch9-stream-windows-x64
    if exist "C:\glitch9-stream-windows-x64 (1)" rmdir /s /q "C:\glitch9-stream-windows-x64 (1)" && echo [clean] removed "...(1)"
)

echo.%*| findstr /i "toolchain all" >nul
if errorlevel 1 goto :done

REM --- Level 2: Rust toolchain + cargo cache ---
call :confirm "Uninstall the Rust toolchain (rustup) + ~/.cargo + ~/.rustup?"
if "!YES!"=="1" (
    rustup self uninstall -y 2>nul
    if exist "%USERPROFILE%\.rustup" rmdir /s /q "%USERPROFILE%\.rustup"
    if exist "%USERPROFILE%\.cargo"  rmdir /s /q "%USERPROFILE%\.cargo"
    echo [clean] Rust toolchain removed.
)

echo.%*| findstr /i "all" >nul
if errorlevel 1 goto :done

REM --- Level 3: build tools (winget) ---
call :confirm "Uninstall VS Build Tools, LLVM, CMake, NASM via winget? (keeps Git + NVIDIA driver)"
if "!YES!"=="1" (
    winget uninstall --id Microsoft.VisualStudio.2022.BuildTools --silent 2>nul
    winget uninstall --id LLVM.LLVM --silent 2>nul
    winget uninstall --id Kitware.CMake --silent 2>nul
    winget uninstall --id NASM.NASM --silent 2>nul
    setx LIBCLANG_PATH "" >nul
    echo [clean] build tools removed.
)

:done
echo [clean] Done. (NVIDIA driver, C:\glitch9-prod and C:\glitch9 were left untouched.)
endlocal
exit /b 0

:confirm
set "YES=0"
set /p "ans=%~1 [y/N] "
if /i "!ans!"=="y" set "YES=1"
exit /b 0
