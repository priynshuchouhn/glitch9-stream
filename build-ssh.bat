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
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
if not exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" (
    call "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
)
cargo build --release
