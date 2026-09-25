@echo off
REM Build wsock32.dll bootstrap for LumaForge CDP Proxy
REM Requires Visual Studio Build Tools (cl.exe + MSVC linker).
REM Uses cl from PATH when already in a Developer Command Prompt (CI does
REM this via ilammy/msvc-dev-cmd); otherwise locates vcvars64.bat.

setlocal
cd /d "%~dp0"

where cl >nul 2>nul
if %errorlevel%==0 goto build

set "VCVARS="

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if exist "%VSWHERE%" (
    for /f "usebackq delims=" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VCVARS=%%i\VC\Auxiliary\Build\vcvars64.bat"
)

if not defined VCVARS if exist "%ProgramFiles%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=%ProgramFiles%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if not defined VCVARS if exist "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"

if not defined VCVARS (
    echo ERROR: cl.exe not in PATH and vcvars64.bat not found.
    exit /b 1
)

:build
echo Building wsock32.dll bootstrap...

REM NOTE: keep the vcvars invocation at top level (not inside an if-block):
REM VCVARS paths contain "(x86)" and cmd mis-parses parens inside blocks.
if not defined VCVARS goto build_direct
cmd /c ""%VCVARS%" >nul 2>&1 && cl /nologo /O2 /LD main.c /link /DEF:wsock32_proxy.def /OUT:wsock32.dll /NOLOGO kernel32.lib ws2_32.lib mswsock.lib"
goto check

:build_direct
cl /nologo /O2 /LD main.c /link /DEF:wsock32_proxy.def /OUT:wsock32.dll /NOLOGO kernel32.lib ws2_32.lib mswsock.lib

:check
if %errorlevel% neq 0 (
    echo ERROR: Build failed
    exit /b 1
)

echo OK: wsock32.dll built successfully
dir wsock32.dll
