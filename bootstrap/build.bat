@echo off
REM Build wsock32.dll bootstrap for LumaForge CDP Proxy
REM Requires Visual Studio Build Tools (cl.exe + MSVC linker)

set "CL_EXE=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\cl.exe"
set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"

if not exist "%CL_EXE%" (
    echo ERROR: cl.exe not found at %CL_EXE%
    exit /b 1
)

echo Building wsock32.dll bootstrap...

cmd /c "`"%VCVARS%`" >nul 2>&1 && cl /nologo /O2 /LD main.c /link /DEF:wsock32_proxy.def /OUT:wsock32.dll /NOLOGO kernel32.lib ws2_32.lib mswsock.lib"
if %errorlevel% neq 0 (
    echo ERROR: Build failed
    exit /b 1
)

echo OK: wsock32.dll built successfully
dir wsock32.dll
