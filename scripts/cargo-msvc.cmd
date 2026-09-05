@echo off
rem Runs cargo inside the x64 environment of a Visual Studio install that actually has the
rem C++ x64 build tools. rustc otherwise picks the *newest* VS it can find, which on some
rem machines is a partial install without msvcrt.lib (LNK1104).
rem
rem Usage:  scripts\cargo-msvc.cmd test -p vox-core
setlocal

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
    echo cargo-msvc: vswhere.exe not found; is Visual Studio / Build Tools installed? 1>&2
    exit /b 1
)

rem Written to a file rather than read via `for /f`: the "(x86)" in the path breaks
rem cmd's for-set parsing.
set "VSDIR_FILE=%TEMP%\vox-cargo-msvc-vsdir.txt"
"%VSWHERE%" -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -latest -property installationPath > "%VSDIR_FILE%"
set "VSDIR="
set /p VSDIR=<"%VSDIR_FILE%"
del "%VSDIR_FILE%" >nul 2>&1
if not defined VSDIR (
    echo cargo-msvc: no Visual Studio install with the C++ x64 build tools component. 1>&2
    exit /b 1
)
if defined VOX_VERBOSE echo cargo-msvc: using %VSDIR% 1>&2

rem vcvars64 prints a harmless "'vswhere.exe' is not recognized" to stderr on machines
rem where vswhere isn't on PATH; it still sets the environment correctly.
call "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>nul
if errorlevel 1 exit /b %errorlevel%

set "PATH=%USERPROFILE%\.cargo\bin;%ProgramFiles%\CMake\bin;%PATH%"

rem whisper-rs-sys runs bindgen, which needs libclang. Respect an existing LIBCLANG_PATH,
rem otherwise use the default LLVM install location.
if not defined LIBCLANG_PATH if exist "%ProgramFiles%\LLVM\bin\libclang.dll" set "LIBCLANG_PATH=%ProgramFiles%\LLVM\bin"

cargo %*
