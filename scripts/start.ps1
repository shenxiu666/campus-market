# 一键启动：编译 → 起 mini-redis 服务端（导入种子）→ 起 Web 网关 → 打开浏览器
#
# 用法：
#   .\scripts\start.ps1                 # 默认 RESP 6380 / HTTP 8080，开启 AOF
#   .\scripts\start.ps1 -NoAof          # 关闭 AOF（每次从种子重建）
#   .\scripts\start.ps1 -RedisPort 6381 -HttpPort 8081
#   .\scripts\start.ps1 -NoBrowser      # 不自动开浏览器

[CmdletBinding()]
param(
    [int]$RedisPort = 6380,
    [int]$HttpPort = 8080,
    [string]$HostAddr = '127.0.0.1',
    [switch]$NoAof,
    [switch]$NoBrowser,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg) { Write-Host "  ok  $msg" -ForegroundColor Green }

# ---------- 0. 环境检查 ----------
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw '未找到 cargo，请先安装 Rust 工具链（https://rustup.rs）'
}

# ---------- 1. 编译 ----------
if (-not $SkipBuild) {
    Write-Step '编译（release）'
    cargo build --release --workspace
    if ($LASTEXITCODE -ne 0) { throw '编译失败' }
    Write-Ok '编译完成'
}

$serverExe = Join-Path $root 'target\release\mredis-server.exe'
$webExe = Join-Path $root 'target\release\mredis-web.exe'
foreach ($exe in @($serverExe, $webExe)) {
    if (-not (Test-Path $exe)) { throw "缺少可执行文件：$exe" }
}

# ---------- 2. 清理旧进程 ----------
Write-Step '清理残留进程'
Get-Process -Name 'mredis-server', 'mredis-web' -ErrorAction SilentlyContinue |
    ForEach-Object { Write-Host "  kill $($_.ProcessName) pid=$($_.Id)"; $_.Kill() }
Start-Sleep -Milliseconds 400

# ---------- 3. 准备运行时目录 ----------
$runtimeDir = Join-Path $root 'data\runtime'
New-Item -ItemType Directory -Force -Path $runtimeDir | Out-Null
$logDir = Join-Path $root 'logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

# ---------- 4. 起 mini-redis 服务端 ----------
$seedPath = Join-Path $root 'data\seed.json'
if (-not (Test-Path $seedPath)) { throw "缺少种子数据：$seedPath" }

$serverArgs = @('--host', $HostAddr, '--port', "$RedisPort", '--seed', $seedPath)
if (-not $NoAof) {
    $serverArgs += @('--appendonly', 'yes', '--appendfsync', 'everysec',
        '--aof-path', (Join-Path $runtimeDir 'appendonly.aof'))
}
else {
    $serverArgs += @('--appendonly', 'no')
}

Write-Step "启动 mini-redis 服务端 :$RedisPort"
$serverLog = Join-Path $logDir 'server.log'
# 用 -WindowStyle Hidden 而不是 -NoNewWindow：后者会让子进程继承当前控制台的
# stdout/stderr 句柄，一旦本脚本的调用方是管道/CI（没有交互式控制台），
# 脚本跑完后调用方仍会一直等到服务退出。隐藏窗口能真正脱离父控制台。
$serverProc = Start-Process -FilePath $serverExe -ArgumentList $serverArgs `
    -RedirectStandardOutput $serverLog -RedirectStandardError (Join-Path $logDir 'server.err.log') `
    -PassThru -WindowStyle Hidden
Write-Ok "pid=$($serverProc.Id)  日志：logs\server.log"

# 等服务端就绪：轮询端口
$ready = $false
for ($i = 0; $i -lt 100; $i++) {
    Start-Sleep -Milliseconds 100
    try {
        $c = New-Object System.Net.Sockets.TcpClient
        $c.Connect($HostAddr, $RedisPort)
        $c.Close()
        $ready = $true
        break
    }
    catch { }
}
if (-not $ready) { throw "服务端 5 秒内未就绪，请查看 logs\server.err.log" }
Write-Ok "RESP 已就绪 $HostAddr`:$RedisPort"

# ---------- 5. 起 Web 网关 ----------
Write-Step "启动 Web 网关 :$HttpPort"
$webLog = Join-Path $logDir 'web.log'
$webProc = Start-Process -FilePath $webExe `
    -ArgumentList @('--http-port', "$HttpPort", '--redis-addr', "$HostAddr`:$RedisPort") `
    -RedirectStandardOutput $webLog -RedirectStandardError (Join-Path $logDir 'web.err.log') `
    -PassThru -WindowStyle Hidden
Write-Ok "pid=$($webProc.Id)  日志：logs\web.log"

$webReady = $false
for ($i = 0; $i -lt 100; $i++) {
    Start-Sleep -Milliseconds 100
    try {
        $r = Invoke-WebRequest -Uri "http://$HostAddr`:$HttpPort/api/health" -TimeoutSec 2 -UseBasicParsing
        if ($r.StatusCode -eq 200) { $webReady = $true; break }
    }
    catch { }
}
if (-not $webReady) { throw "Web 5 秒内未就绪，请查看 logs\web.err.log" }
Write-Ok 'Web 已就绪'

# ---------- 6. 写 PID 文件，供 stop.ps1 使用 ----------
@{
    server = $serverProc.Id
    web    = $webProc.Id
    redis  = "$HostAddr`:$RedisPort"
    http   = "http://$HostAddr`:$HttpPort"
} | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $runtimeDir 'pids.json')

$url = "http://$HostAddr`:$HttpPort/"
Write-Host ''
Write-Host '┌────────────────────────────────────────────┐' -ForegroundColor DarkCyan
Write-Host '│  mini-redis 校园二手交易平台已启动          │' -ForegroundColor DarkCyan
Write-Host '├────────────────────────────────────────────┤' -ForegroundColor DarkCyan
Write-Host "│  Web UI : $url" -ForegroundColor DarkCyan
Write-Host "│  RESP   : $HostAddr`:$RedisPort" -ForegroundColor DarkCyan
Write-Host "│  CLI    : target\release\mredis-cli.exe -p $RedisPort" -ForegroundColor DarkCyan
Write-Host '│  停止   : .\scripts\stop.ps1                │' -ForegroundColor DarkCyan
Write-Host '└────────────────────────────────────────────┘' -ForegroundColor DarkCyan

if (-not $NoBrowser) { Start-Process $url }
