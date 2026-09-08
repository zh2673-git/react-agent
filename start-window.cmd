@echo off
rem ── react-agent 多窗口启动器（W17：VSCode 式多实例，每实例独立工作区/会话/配置）──
rem 用法：start-window.cmd <工作区路径>            实例名自动取文件夹名（推荐）
rem       start-window.cmd <实例名> <工作区路径>   显式指定实例名
rem       start-window.cmd <实例名>               工作区缺省 = .instances\<名>\workspace（自带空工作区）
rem 实例名：1-32 字符，不含 / \ : * ? " < > |（允许中文）。目标项目目录零污染。
setlocal enabledelayedexpansion
cd /d "%~dp0"

if "%~1"=="" (
  echo 用法：start-window.cmd ^<工作区路径^> ｜ 或 ^<实例名^> ^<工作区路径^> ｜ 或 ^<实例名^>
  echo 示例：start-window.cmd D:\projects\my-app
  exit /b 1
)

rem ── 参数分派：首参含 \ 或 : → 是工作区路径（实例名自动派生）；否则首参=实例名 ──
set "INST="
echo %~1 | findstr /c:"\\" /c:":" >nul 2>&1
if not errorlevel 1 (
  set "WS=%~1"
) else (
  set "INST=%~1"
  set "WS=%~2"
)
if not defined INST (
  for %%i in ("%WS%") do set "INST=%%~nxi"
  if not defined INST (
    echo [error] 无法从工作区路径派生实例名，请显式指定：start-window.cmd ^<实例名^> ^<工作区路径^>
    exit /b 1
  )
)
if "%WS%"=="" set "WS=%~dp0.instances\%INST%\workspace"
if not exist "%WS%" mkdir "%WS%"

rem ── host 二进制（多实例并发跑 cargo run 会抢 target 构建锁，直接用已构建 exe）──
set "EXE=%~dp0target\debug\react-agent-host.exe"
if not exist "%EXE%" (
  echo [error] 未找到 %EXE%
  echo         请先运行 start.cmd（或 cargo build -p react-agent-host）完成一次构建。
  exit /b 1
)

rem ── 端口探测：8711 起找第一个空闲端口 ──
set /a PORT=8711
:probe
powershell -NoProfile -Command "try{$l=[System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback,%PORT%);$l.Start();$l.Stop();exit 0}catch{exit 1}" >nul 2>&1
if errorlevel 1 (
  set /a PORT+=1
  if %PORT% GEQ 8910 ( echo [error] 8711-8910 无空闲端口 & exit /b 1 )
  goto probe
)

set "DATA=%~dp0.instances\%INST%"
if not exist "%DATA%\memory" mkdir "%DATA%\memory"
if not exist "%DATA%\stream" mkdir "%DATA%\stream"
if not exist "%DATA%\config.json" (
  if exist "%~dp0config.json" copy /y "%~dp0config.json" "%DATA%\config.json" >nul
)

set REACT_FRONTEND=web
set REACT_INSTANCE_NAME=%INST%
set WORKSPACE_ROOT=%WS%
set WEB_ADDR=127.0.0.1:%PORT%
set MEMORY_DATA_DIR=%DATA%\memory
set AGENT_STREAM_DIR=%DATA%\stream
set CONFIG_FILE=%DATA%\config.json

start "" cmd /c "timeout /t 4 /nobreak >nul & start http://127.0.0.1:%PORT%"
echo [window:%INST%] 工作区=%WS%
echo [window:%INST%] 启动中... 浏览器将自动打开 http://127.0.0.1:%PORT%（关掉本窗口即停实例）
"%EXE%"
endlocal
