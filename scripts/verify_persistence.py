"""独立复现 AOF+seed 持久化场景（Lead 侧验收脚本）。

四轮：首启 → AOF 重启 → BGREWRITEAOF → 重写后重启。
每轮打印 DBSIZE / REC.STATS / REC.EXPLAIN 2 1 的 parts.hot_norm / REC.FOR_YOU 1 2。

用法（在 campus-market 目录下）：python scripts/verify_persistence.py
"""

import json
import os
import socket
import subprocess
import sys
import time

# 脚本在 scripts/ 下，但 exe 和数据路径都相对于项目根目录
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(ROOT)

EXE = os.path.join("target", "release", "mredis-server.exe")
AOF = os.path.join("data", "runtime", "verify", "a.aof")
PORT = 7801


def encode(args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        b = a.encode() if isinstance(a, str) else a
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


def read_reply(f):
    line = f.readline()
    if not line:
        raise EOFError("server closed")
    tag, rest = line[:1], line[1:].rstrip(b"\r\n")
    if tag == b"+":
        return rest.decode()
    if tag == b"-":
        return "ERR:" + rest.decode()
    if tag == b":":
        return int(rest)
    if tag == b"$":
        n = int(rest)
        if n < 0:
            return None
        data = f.read(n + 2)[:-2]
        return data.decode()
    if tag == b"*":
        n = int(rest)
        if n < 0:
            return None
        return [read_reply(f) for _ in range(n)]
    raise ValueError("bad tag %r" % tag)


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.f = self.s.makefile("rb")

    def call(self, *args):
        self.s.sendall(encode(list(args)))
        return read_reply(self.f)

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def start():
    log = open(os.path.join("data", "runtime", "verify", "srv.log"), "ab")
    p = subprocess.Popen(
        [EXE, "--port", str(PORT), "--seed", os.path.join("data", "seed.json"),
         "--appendonly", "yes", "--appendfsync", "everysec", "--aof-path", AOF],
        stdout=log, stderr=subprocess.STDOUT,
    )
    deadline = time.time() + 15
    while time.time() < deadline:
        try:
            c = Conn(PORT)
            c.call("PING")
            c.close()
            return p
        except OSError:
            time.sleep(0.2)
    raise RuntimeError("server did not start")


def stop(p):
    # 优先优雅停机：SHUTDOWN 会让服务端 fsync 后主动退出。
    # 再兜底 terminate/kill，用于验证「即使被硬杀，写穿的数据也已落盘」。
    try:
        c = Conn(PORT)
        c.call("SHUTDOWN")
        c.close()
    except (OSError, EOFError):
        pass
    try:
        p.wait(timeout=8)
    except subprocess.TimeoutExpired:
        p.terminate()
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
    time.sleep(0.3)


def snapshot(tag):
    c = Conn(PORT)
    dbsize = c.call("DBSIZE")
    stats = json.loads(c.call("REC.STATS"))
    exp = json.loads(c.call("REC.EXPLAIN", "2", "1"))
    foryou = json.loads(c.call("REC.FOR_YOU", "1", "2"))
    c.close()
    # hot_score 的分母含 age_hours，跨时刻无法逐位相等；保留 6 位小数足够区分回归。
    hot = round(exp["parts"]["hot_norm"], 6)
    fy = [(x["product_id"], round(x["score"], 3)) for x in foryou]
    print(f"  [{tag}] DBSIZE={dbsize} STATS={stats}")
    print(f"  [{tag}] hot_norm={hot} FOR_YOU={fy}")
    return dbsize, stats, hot, fy


def main():
    os.makedirs(os.path.dirname(AOF), exist_ok=True)
    for path in (AOF, AOF + ".ready"):
        if os.path.exists(path):
            os.remove(path)

    print("=== r1 首启 ===")
    p = start()
    r1 = snapshot("r1")
    # 模拟 Web 发布：INCR 拿 id → HSET → REC.UPSERT → REC.TRACK
    c = Conn(PORT)
    new_id = c.call("INCR", "meta:next_product_id")
    print(f"  INCR meta:next_product_id -> {new_id}")
    c.call("HSET", f"product:{new_id}", "id", str(new_id), "title", "验收测试台灯",
           "desc", "九成新", "price_cents", "4500", "tags", "台灯,宿舍",
           "seller_id", "1", "condition", "9", "campus", "前湖校区",
           "created_at", str(int(time.time())), "status", "onsale",
           "views", "0", "clicks", "0", "favs", "0")
    c.call("SADD", "tag:台灯", str(new_id))
    c.call("ZADD", "product:index:created", str(int(time.time())), str(new_id))
    c.call("ZADD", "zset:hot", "0", str(new_id))
    print(f"  REC.UPSERT {new_id} -> {c.call('REC.UPSERT', str(new_id))}")
    print(f"  REC.TRACK 1 {new_id} view -> {c.call('REC.TRACK', '1', str(new_id), 'view')}")
    print(f"  after-write DBSIZE={c.call('DBSIZE')} STATS={c.call('REC.STATS')}")
    c.close()
    # 写入后的基线快照：r2/r4 必须与它逐位一致。
    # 注意不能拿写入前的 r1 做基准——新增商品会改写 min-max 归一化区间，
    # 且 hot_score 分母含 age_hours，跨时刻本就不可比。
    r1w = snapshot("r1w")
    stop(p)

    print("=== r2 AOF 重启 ===")
    p = start()
    r2 = snapshot("r2")
    stop(p)

    print("=== r3 BGREWRITEAOF ===")
    p = start()
    c = Conn(PORT)
    print(f"  BGREWRITEAOF -> {c.call('BGREWRITEAOF')}")
    time.sleep(1.5)
    c.close()
    stop(p)
    print(f"  aof size={os.path.getsize(AOF)} bytes")

    print("=== r4 重写后重启 ===")
    p = start()
    r4 = snapshot("r4")
    stop(p)

    print("=== 结论 ===")
    ok = True
    if r2[0] <= 100:
        print(f"  FAIL: r2 DBSIZE={r2[0]}，种子又丢了")
        ok = False
    if r2[1]["products"] != 65:
        print(f"  FAIL: r2 products={r2[1]['products']} 应为 65")
        ok = False
    if abs(r2[2] - r1w[2]) > 1e-6:
        print(f"  FAIL: hot_norm 漂移 {r1w[2]} -> {r2[2]}")
        ok = False
    if r2[3] != r1w[3]:
        print(f"  FAIL: FOR_YOU 变了 {r1w[3]} -> {r2[3]}")
        ok = False
    if r4[1] != r2[1]:
        print(f"  FAIL: 重写后 STATS 变了 {r2[1]} -> {r4[1]}")
        ok = False
    if r4[0] != r2[0]:
        print(f"  FAIL: 重写后 DBSIZE 变了 {r2[0]} -> {r4[0]}")
        ok = False
    print("  PASS 全部一致" if ok else "  FAIL 见上")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
