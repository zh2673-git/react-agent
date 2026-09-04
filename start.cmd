@echo off
rem ── react-agent 一键启动（双击即用）──────────────────────────────
rem 缺省启动 Web 前端（http://127.0.0.1:8710）；LLM 供应商/密钥/工具/技能
rem 全部可在网页右上角 ⚙设置 里在线配置（保存即热生效并落盘 config.json）。
rem 如需 REPL：set REACT_FRONTEND=repl 后运行本脚本，或直接 cargo run。
setlocal
cd /d "%~dp0"

rem ── 依赖自检（幂等：缺什么装什么，装过即跳过）──
python -c "import grpc, httpx" >nul 2>&1
if errorlevel 1 (
  echo [setup] 安装 python guest 依赖 grpcio httpx ...
  pip install grpcio httpx
)

if exist "..\agent-kernel\bindings\typescript" (
  if not exist "..\agent-kernel\bindings\typescript\node_modules" (
    echo [setup] 安装 TS guest 依赖（agent-kernel npm install）...
    pushd "..\agent-kernel\bindings\typescript"
    call npm install
    popd
  )
)

rem ── 启动：Web 前端 + 3 秒后自动打开浏览器 ──
set REACT_FRONTEND=web
start "" cmd /c "timeout /t 3 /nobreak >nul & start http://127.0.0.1:8710"
echo [start] react-agent 启动中... 浏览器将自动打开 http://127.0.0.1:8710（Ctrl+C 退出）
cargo run -p react-agent-host
endlocal
