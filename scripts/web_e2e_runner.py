"""在「全新存储」上跑一遍 Web 端到端回归。

`web_e2e.py` 假设 8080 上已经有服务在跑。本脚本负责：
  1. 清空 data/runtime/web_e2e/ 下自己的 AOF（不影响日常演示数据）
  2. 起 mredis-server :6380 + mredis-web :8080
  3. 调用 scripts/web_e2e.py
  4. 无论成败都用 SHUTDOWN 优雅停机

用法（在 campus-market 目录下）：python scripts/web_e2e_runner.py
"""

import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(ROOT)

SERVER = os.path.join("target", "release", "mredis-server.exe")
WEB = os.path.join("target", "release", "mredis-web.exe")
WORK = os.path.join("data", "runtime", "web_e2e")
AOF = os.path.join(WORK, "appendonly.aof")
SEED = os.path.join("data", "seed.json")
REDIS_PORT = 6380
HTTP_PORT = 8080
BASE = "http://127.0.0.1:%d" % HTTP_PORT


def wait_redis():
    deadline = time.time() + 20
    while time.time() < deadline:
        try:
            sock = socket.create_connection(("127.0.0.1", REDIS_PORT), timeout=1)
            sock.close()
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("mredis-server 未在 20 秒内就绪")


def wait_web():
    deadline = time.time() + 20
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(BASE + "/api/health", timeout=2) as resp:
                if resp.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.15)
    raise RuntimeError("mredis-web 未在 20 秒内就绪")


def stop(server, web):
    # 先请服务端优雅退出（同时验证 SHUTDOWN 路径），再硬杀兜底。
    try:
        sock = socket.create_connection(("127.0.0.1", REDIS_PORT), timeout=2)
        sock.sendall(b"*1\r\n$8\r\nSHUTDOWN\r\n")
        sock.close()
    except OSError:
        pass
    for proc in (web, server):
        if proc is None:
            continue
        try:
            proc.wait(timeout=6)
        except subprocess.TimeoutExpired:
            proc.kill()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
    time.sleep(0.3)


def main():
    for path in (AOF, AOF + ".ready"):
        if os.path.exists(path):
            os.remove(path)
    os.makedirs(WORK, exist_ok=True)

    server = web = None
    try:
        server = subprocess.Popen(
            [SERVER, "--port", str(REDIS_PORT), "--seed", SEED,
             "--appendonly", "yes", "--appendfsync", "everysec", "--aof-path", AOF],
            stdout=open(os.path.join(WORK, "server.log"), "ab"),
            stderr=subprocess.STDOUT,
        )
        wait_redis()
        web = subprocess.Popen(
            [WEB, "--http-port", str(HTTP_PORT),
             "--redis-addr", "127.0.0.1:%d" % REDIS_PORT],
            stdout=open(os.path.join(WORK, "web.log"), "ab"),
            stderr=subprocess.STDOUT,
        )
        wait_web()
        result = subprocess.run([sys.executable, os.path.join("scripts", "web_e2e.py")])
        return result.returncode
    finally:
        stop(server, web)


if __name__ == "__main__":
    sys.exit(main())
