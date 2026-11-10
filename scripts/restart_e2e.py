"""真实可用性验收：Web 写入 → 硬杀进程 → 重启 → 数据仍在。

覆盖三组场景：
  1. 全新存储启动 → 发布商品 → 直接 kill 两个进程 → 重启 → 商品仍在、可搜索
  2. PATCH 修改 → 硬杀 → 重启 → 标题 / 价格 / 状态都还在
  3. DELETE 删除 → 硬杀 → 重启 → 404，且种子商品不会被重新导入

这里用的是 TerminateProcess（等价于 kill -9），不是优雅停机，用来证明
数据已经写穿到操作系统，而不是只停在进程的 BufWriter 里。

用法（在 campus-market 目录下）：python scripts/restart_e2e.py
"""

import json
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
WORK = os.path.join("data", "runtime", "restart_e2e")
AOF = os.path.join(WORK, "appendonly.aof")
SEED = os.path.join("data", "seed.json")
REDIS_PORT = 16391
HTTP_PORT = 18091
BASE = "http://127.0.0.1:%d" % HTTP_PORT

TITLE = "真实持久化测试商品"
TITLE2 = "真实持久化测试商品（已改价）"

fails = []


def check(label, cond, detail=""):
    mark = "OK  " if cond else "FAIL"
    print("  [%s] %s%s" % (mark, label, (" — " + detail) if detail else ""))
    if not cond:
        fails.append(label)


def req_json(path, method="GET", body=None):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(BASE + path, data=data, method=method)
    if data:
        request.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=15) as resp:
            raw = resp.read()
            return resp.status, resp.headers.get("content-type", ""), (
                json.loads(raw) if raw else None
            )
    except urllib.error.HTTPError as err:
        raw = err.read()
        try:
            parsed = json.loads(raw) if raw else None
        except json.JSONDecodeError:
            parsed = None
        return err.code, err.headers.get("content-type", ""), parsed


def try_shutdown_stale():
    """上一轮异常退出可能留下服务端，先请它优雅退出。"""
    try:
        sock = socket.create_connection(("127.0.0.1", REDIS_PORT), timeout=1)
        sock.sendall(b"*1\r\n$8\r\nSHUTDOWN\r\n")
        sock.close()
        time.sleep(0.5)
    except OSError:
        pass


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
            status, _, _ = req_json("/api/health")
            if status == 200:
                return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.15)
    raise RuntimeError("mredis-web 未在 20 秒内就绪")


def start():
    os.makedirs(WORK, exist_ok=True)
    server_log = open(os.path.join(WORK, "server.log"), "ab")
    web_log = open(os.path.join(WORK, "web.log"), "ab")
    server = subprocess.Popen(
        [SERVER, "--port", str(REDIS_PORT), "--seed", SEED,
         "--appendonly", "yes", "--appendfsync", "everysec", "--aof-path", AOF],
        stdout=server_log, stderr=subprocess.STDOUT,
    )
    try:
        wait_redis()
        web = subprocess.Popen(
            [WEB, "--http-port", str(HTTP_PORT),
             "--redis-addr", "127.0.0.1:%d" % REDIS_PORT],
            stdout=web_log, stderr=subprocess.STDOUT,
        )
        wait_web()
    except Exception:
        hard_kill(server)
        raise
    return server, web


def hard_kill(*procs):
    """TerminateProcess：不给进程任何 flush / 析构的机会。"""
    for proc in procs:
        if proc is not None and proc.poll() is None:
            proc.kill()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pass
    time.sleep(0.3)


def main():
    try_shutdown_stale()
    for path in (AOF, AOF + ".ready"):
        if os.path.exists(path):
            os.remove(path)
    os.makedirs(WORK, exist_ok=True)

    server = None
    web = None
    try:
        print("=== 1. 首启 → 发布 → 硬杀 → 重启 ===")
        server, web = start()
        status, _, health = req_json("/api/health")
        check("首启 /api/health 200", status == 200, str(health))

        status, _, created = req_json("/api/products", "POST", {
            "title": TITLE, "desc": "用于验证重启后仍在", "price": 88.88,
            "tags": ["台灯", "宿舍"], "seller_id": 1, "condition": 9,
            "campus": "前湖校区",
        })
        check("发布 201", status == 201, str(created))
        product_id = (created or {}).get("id")
        status, _, product = req_json("/api/products/%s" % product_id)
        check("发布后可读", status == 200 and product.get("title") == TITLE)

        hard_kill(web, server)
        web = server = None

        server, web = start()
        status, _, product = req_json("/api/products/%s" % product_id)
        check("硬杀重启后商品仍在", status == 200 and product.get("title") == TITLE,
              "status=%s" % status)
        status, _, listing = req_json(
            "/api/products?q=%E7%9C%9F%E5%AE%9E%E6%8C%81%E4%B9%85%E5%8C%96"
        )
        check("重启后仍可搜索", any(it["id"] == product_id for it in listing["items"]),
              "total=%d" % listing["total"])

        print("=== 2. PATCH → 硬杀 → 重启 ===")
        status, _, updated = req_json("/api/products/%s" % product_id, "PATCH", {
            "title": TITLE2, "price_cents": 9999, "status": "sold",
        })
        check("PATCH 200 且字段已更新",
              status == 200 and updated["product"]["title"] == TITLE2
              and updated["product"]["price"] == "99.99")

        hard_kill(web, server)
        web = server = None

        server, web = start()
        status, _, product = req_json("/api/products/%s" % product_id)
        check("修改在重启后仍在",
              status == 200 and product.get("title") == TITLE2
              and product.get("price_cents") == 9999
              and product.get("status") == "sold",
              "%s / %s / %s" % (product.get("title"), product.get("price_cents"),
                                product.get("status")))

        print("=== 3. DELETE → 硬杀 → 重启 ===")
        status, _, deleted = req_json("/api/products/%s" % product_id, "DELETE")
        check("DELETE 200", status == 200 and deleted.get("deleted") == product_id,
              str(deleted))

        hard_kill(web, server)
        web = server = None

        server, web = start()
        status, _, gone = req_json("/api/products/%s" % product_id)
        check("删除后重启仍 404", status == 404, "status=%s" % status)
        status, _, seed_product = req_json("/api/products/1")
        check("种子商品 1 仍在（未被删除）", status == 200 and seed_product.get("id") == 1)
        status, _, stats = req_json("/api/stats")
        check("统计里商品数回到 64", (stats or {}).get("rec", {}).get("products") == 64,
              str((stats or {}).get("rec")))
    finally:
        hard_kill(web, server)

    print()
    if fails:
        print("FAILED %d 项: %s" % (len(fails), fails))
        return 1
    print("全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
