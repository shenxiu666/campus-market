# 停止由 start.ps1 启动的 mini-redis 服务端与 Web 网关
# 服务端优先走 SHUTDOWN 命令（刷盘后主动退出），失败再兜底 Kill。

[CmdletBinding()]
param()

$ErrorActionPreference = 'SilentlyContinue'
$root = Split-Path -Parent $PSScriptRoot
$pidFile = Join-Path $root 'data\runtime\pids.json'
$cliExe = Join-Path $root 'target\release\mredis-cli.exe'

$stopped = 0
if (Test-Path $pidFile) {
    $info = Get-Content -Raw -Encoding utf8 $pidFile | ConvertFrom-Json

    # 1) 先请求服务端优雅停机：SHUTDOWN 会 fsync 后再退出
    if ($info.redis -and (Test-Path $cliExe)) {
        $port = ($info.redis -split ':')[-1]
        & $cliExe -p $port SHUTDOWN *> $null
        for ($i = 0; $i -lt 50; $i++) {
            Start-Sleep -Milliseconds 100
            if (-not (Get-Process -Id $info.server -ErrorAction SilentlyContinue)) { break }
        }
    }

    # 2) Web 网关无持久化状态，直接结束即可
    $web = Get-Process -Id $info.web -ErrorAction SilentlyContinue
    if ($web) { Write-Host "stop  $($web.ProcessName) pid=$($info.web)"; $web.Kill(); $stopped++ }

    Remove-Item $pidFile -Force
}

# 3) 兜底：按进程名清理（含 SHUTDOWN 失败或 pid 文件丢失的情况）
Get-Process -Name 'mredis-server', 'mredis-web' -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "kill  $($_.ProcessName) pid=$($_.Id)"; $_.Kill(); $stopped++
}

if ($stopped -eq 0) { Write-Host '没有正在运行的 mredis 进程' }
else { Write-Host "已停止 $stopped 个进程" -ForegroundColor Green }
