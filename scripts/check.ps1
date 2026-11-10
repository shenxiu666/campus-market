# 全量验收：fmt → clippy → test → build
# 用法： .\scripts\check.ps1
#        .\scripts\check.ps1 -Fix     # 自动 cargo fmt 后再检查

[CmdletBinding()]
param([switch]$Fix)

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$failed = @()

function Invoke-Check($name, $block) {
    Write-Host ''
    Write-Host "==> $name" -ForegroundColor Cyan
    & $block
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  FAILED: $name" -ForegroundColor Red
        $script:failed += $name
    }
    else {
        Write-Host "  passed" -ForegroundColor Green
    }
}

if ($Fix) {
    Write-Host '==> cargo fmt --all' -ForegroundColor Cyan
    cargo fmt --all
}

# release build 会覆盖 target\release\*.exe，正在运行的进程会锁住文件导致
# 「拒绝访问 (os error 5)」。先停掉残留进程，让验收脚本可以随时跑。
$stale = Get-Process -Name 'mredis-server', 'mredis-web' -ErrorAction SilentlyContinue
if ($stale) {
    Write-Host ''
    Write-Host '==> 停止残留的 mredis 进程（否则 release 构建会因文件占用失败）' -ForegroundColor Cyan
    $stale | ForEach-Object {
        Write-Host "  kill $($_.ProcessName) pid=$($_.Id)"
        $_.Kill()
    }
    Start-Sleep -Milliseconds 500
}

Invoke-Check 'cargo fmt --all --check' { cargo fmt --all --check }
Invoke-Check 'cargo clippy --workspace --all-targets -D warnings' {
    cargo clippy --workspace --all-targets -- -D warnings
}
Invoke-Check 'cargo test --workspace' { cargo test --workspace }
Invoke-Check 'cargo build --release --workspace' { cargo build --release --workspace }
Invoke-Check 'python scripts/restart_e2e.py（硬杀重启持久化）' { python scripts/restart_e2e.py }

Write-Host ''
if ($failed.Count -eq 0) {
    Write-Host '全部通过 ✔' -ForegroundColor Green
    exit 0
}
else {
    Write-Host "失败项：$($failed -join ', ')" -ForegroundColor Red
    exit 1
}
