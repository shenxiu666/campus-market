"""Web 端到端回归（Lead 侧验收脚本）。

覆盖：健康检查 → 列表/筛选/排序 → 详情 → 发布（含校验错误）→
编辑/删除（真实 CRUD）→ 事件 → 推荐标签硬门槛与收藏排除 → 相似 → 热度 → 统计 → 静态资源。

用法（先启动服务，然后）：python scripts/web_e2e.py
"""

import json
import sys
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:8080"
fails = []


def req(path, method="GET", body=None):
    if body is None:
        data = None
    elif isinstance(body, bytes):
        data = body
    else:
        data = json.dumps(body).encode()
    r = urllib.request.Request(BASE + path, data=data, method=method)
    if data:
        r.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=15) as resp:
            raw = resp.read()
            ctype = resp.headers.get("content-type", "")
            return resp.status, ctype, raw
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("content-type", ""), e.read()


def get_json(path):
    st, ct, raw = req(path)
    return st, ct, json.loads(raw)


def post_json(path, body):
    """POST 一个 JSON 体，返回 (status, ctype, 解析后的 JSON 或 None)。"""
    st, ct, raw = req(path, "POST", body)
    try:
        return st, ct, json.loads(raw)
    except json.JSONDecodeError:
        return st, ct, None


def send_json(path, method, body=None):
    """PATCH / DELETE 等任意方法的 JSON 请求。"""
    st, ct, raw = req(path, method, body)
    try:
        return st, ct, json.loads(raw)
    except json.JSONDecodeError:
        return st, ct, None


def check(label, cond, detail=""):
    mark = "OK  " if cond else "FAIL"
    print(f"  [{mark}] {label}{(' — ' + detail) if detail else ''}")
    if not cond:
        fails.append(label)


print("=== 1. 健康与静态资源 ===")
st, ct, j = get_json("/api/health")
check("/api/health 200", st == 200, f"{j}")
check("health.ok", j.get("ok") is True)
for path, kind in [("/", "text/html"), ("/app.js", "javascript"), ("/style.css", "text/css")]:
    st, ct, raw = req(path)
    check(f"GET {path} 200 + {kind}", st == 200 and kind in ct, f"{len(raw)}B {ct}")

print("=== 2. 列表 / 筛选 / 排序 ===")
st, ct, j = get_json("/api/products?limit=3")
# 脚本可能重复跑（每跑一次多一个商品），只要求「种子基线还在」。
check("列表 200 + total>=64", st == 200 and j["total"] >= 64,
      f"total={j['total']} len={len(j['items'])}")
check("JSON content-type", "application/json" in ct)

st, _, j = get_json("/api/products?sort=price_asc&limit=5")
prices = [it["price_cents"] for it in j["items"]]
check("price_asc 单调", prices == sorted(prices), f"{prices}")

st, _, j = get_json("/api/products?sort=price_desc&limit=5")
prices = [it["price_cents"] for it in j["items"]]
check("price_desc 单调", prices == sorted(prices, reverse=True), f"{prices}")

st, _, j = get_json("/api/products?sort=time_desc&limit=5")
ts = [it["created_at"] for it in j["items"]]
check("time_desc 单调", ts == sorted(ts, reverse=True))

st, _, j = get_json("/api/products?sort=hot_desc&limit=5")
hot = [it["hot"] for it in j["items"]]
check("hot_desc 单调", hot == sorted(hot, reverse=True), f"{hot}")

st, _, j = get_json("/api/products?sort=condition_desc&limit=5")
cond = [it["condition"] for it in j["items"]]
check("condition_desc 单调", cond == sorted(cond, reverse=True), f"{cond}")

st, _, j = get_json("/api/products?sort=bogus")
check("非法 sort → 400 + field=sort", st == 400 and j.get("field") == "sort", f"{j}")

st, _, j = get_json("/api/products?tags=%E8%87%AA%E8%A1%8C%E8%BD%A6&limit=10")
check("tags=自行车 全部命中", all("自行车" in it["tags"] for it in j["items"]), f"total={j['total']}")

st, _, j = get_json("/api/products?q=%E8%87%AA%E8%A1%8C%E8%BD%A6")
check("q=自行车 搜索", j["total"] >= 1, f"total={j['total']}")

st, _, j = get_json("/api/products?min=300&max=500&limit=100")
cents = [it["price_cents"] for it in j["items"]]
check("价格区间过滤（min/max 单位：元）",
      all(30000 <= c <= 50000 for c in cents), f"{len(cents)} 条 {cents[:5]}")
st, _, j = get_json("/api/products?min=abc")
check("非法 min → 400 + field=min", st == 400 and j.get("field") == "min", f"{j}")
st, _, j = get_json("/api/products?min=500&max=100")
check("min>max → 400 + field=min", st == 400 and j.get("field") == "min", f"{j}")

print("=== 3. 详情与错误 ===")
st, _, j = get_json("/api/products/1")
check("详情 200", st == 200 and j["id"] == 1, f"{j.get('title')} {j.get('price')}")
check("price 是两位小数字符串", j["price"].count(".") == 1 and len(j["price"].split(".")[1]) == 2, j["price"])
check("详情 15 字段", len(j) == 15, f"{sorted(j)}")

st, _, j = get_json("/api/products/99999")
check("不存在 → 404 + JSON error", st == 404 and j.get("error") == "product not found")

print("=== 4. 发布（含校验）===")
st, _, j = post_json("/api/products", {
    "title": "验收台灯", "desc": "九成新，宿舍可用", "price": 45.0,
    "tags": ["台灯", "宿舍"], "seller_id": 1, "condition": 9, "campus": "前湖校区",
})
check("发布 201", st == 201, f"{j}")
new_id = j.get("id")

st, _, j = post_json("/api/products", {"title": "缺标签", "price": 10.0})
check("缺 tags → 400 + field=tags", st == 400 and j.get("field") == "tags", f"{j}")

st, _, j = post_json("/api/products", {"title": "价格为零", "price": 0, "tags": ["x"]})
check("price=0 → 400 + field=price", st == 400 and j.get("field") == "price", f"{j}")

st, _, j = post_json("/api/products", {"title": "价格字符串", "price": "12.5", "tags": ["x"]})
check("price 字符串 → 400 JSON", st == 400 and j is not None and "error" in j, f"{j}")

st, ct, raw = req("/api/products", "POST", b"{bad")
check("坏 JSON → 400 + JSON", st == 400 and "application/json" in ct, raw[:80].decode(errors="replace"))

print("=== 5. 新商品进入列表（REC.UPSERT 生效）===")
st, _, j = get_json(f"/api/products/{new_id}")
check(f"新商品 {new_id} 可读", st == 200 and j["title"] == "验收台灯")

st, _, j = get_json("/api/products?tags=%E5%8F%B0%E7%81%AF&limit=10")
check("tags=台灯 能搜到新商品", any(it["id"] == new_id for it in j["items"]), f"total={j['total']}")

print("=== 5b. 编辑 / 删除（真实 CRUD）===")
st, _, j = send_json(f"/api/products/{new_id}", "PATCH", {
    "title": "验收台灯（已改价）", "price_cents": 3999, "status": "reserved",
})
check("PATCH 200 + 标题/价格/状态更新",
      st == 200 and j["product"]["title"] == "验收台灯（已改价）"
      and j["product"]["price"] == "39.99" and j["product"]["status"] == "reserved",
      f"{j.get('product')}")

st, _, j = get_json(f"/api/products/{new_id}")
check("PATCH 结果持久可读", j["title"] == "验收台灯（已改价）" and j["price_cents"] == 3999)

st, _, j = send_json(f"/api/products/{new_id}", "PATCH", {"condition": 11})
check("PATCH 非法成色 → 400 + field=condition",
      st == 400 and j.get("field") == "condition", f"{j}")
st, _, j = send_json("/api/products/99999", "PATCH", {"title": "x"})
check("PATCH 不存在 → 404", st == 404, f"{j}")

st, _, j = send_json(f"/api/products/{new_id}", "DELETE")
check("DELETE 200", st == 200 and j.get("deleted") == new_id, f"{j}")
st, _, j = get_json(f"/api/products/{new_id}")
check("删除后详情 404", st == 404, f"{j}")
st, _, j = get_json("/api/products?tags=%E5%8F%B0%E7%81%AF&limit=10")
check("删除后搜索结果里不再出现", all(it["id"] != new_id for it in j["items"]), f"total={j['total']}")

# 硬门槛：每条推荐都必须命中当前用户至少一个兴趣标签
st, _, rec = get_json("/api/recommend?user_id=1&limit=10")
ids = [it["product_id"] for it in rec["items"]]
check("推荐 200 且非空", st == 200 and len(ids) > 0, f"{ids[:5]}")
check("每条推荐都有标签匹配理由",
      all(any(r["type"] == "tag_match" for r in it["reasons"]) for it in rec["items"]),
      f"{[[r['type'] for r in it['reasons']] for it in rec['items'][:3]]}")
print(f"       推荐详情: {[(it['product_id'], it['product']['title'], round(it['score'],4), [r['type'] for r in it['reasons']]) for it in rec['items'][:5]]}")

# 收藏是强负反馈：收藏榜首商品后，它应从推荐中消失
top = ids[0]
st, _, j = post_json(f"/api/products/{top}/event", {"user_id": 1, "kind": "fav"})
check("fav 事件 200", st == 200 and j.get("ok") is True)

st, _, rec2 = get_json("/api/recommend?user_id=1&limit=10")
ids2 = [it["product_id"] for it in rec2["items"]]
print(f"       fav 后推荐: {[(it['product_id'], round(it['score'],4)) for it in rec2['items'][:5]]}")
check("收藏后该商品被排除", top not in ids2, f"{top} in {ids2[:5]}")

print("=== 6. 相似 / 热度 / 用户 / 统计 ===")
st, _, j = get_json("/api/similar/1?limit=3")
check("similar 200 且不含自身",
      st == 200 and all(it["product_id"] != 1 for it in j["items"]),
      f"{[it['product_id'] for it in j['items']]}")

st, _, j = get_json("/api/hot?limit=3")
scores = [it["score"] for it in j["items"]]
check("hot 降序", st == 200 and scores == sorted(scores, reverse=True), f"{scores}")

st, _, j = get_json("/api/users")
check("users 8 个", st == 200 and len(j["items"]) == 8, f"{len(j['items'])}")

st, _, j = get_json("/api/stats")
check("stats 键齐全", all(k in j for k in
      ["aof", "connected_clients", "dbsize", "dict", "hit_rate", "keys",
       "rec", "recent_commands", "total_commands", "uptime_secs"]))
check("stats.rec.products >= 64（发布后删除，净增 0）", j["rec"]["products"] >= 64, f"{j['rec']}")
check("hit_rate 有值（HGETALL 计数已修）", j["hit_rate"] is not None and j["total_commands"] > 0,
      f"hit_rate={j['hit_rate']} total={j['total_commands']}")

st, _, j = get_json("/api/bench/sort?n=2000")
check("bench 三算法且一致", st == 200 and len(j["results"]) == 3 and j["consistent"] is True,
      f"{[(r['algorithm'], r['ms']) for r in j['results']]}")

print("=== 7. 边界与钳制 ===")
# 超界不是静默钳制，而是显式 400（比悄悄改参数更容易调试）。
st, _, j = get_json("/api/products?limit=9999")
check("limit=9999 → 400 + field=limit", st == 400 and j.get("field") == "limit", f"{j}")
st, _, j = get_json("/api/products?limit=0")
check("limit=0 → 400 + field=limit", st == 400 and j.get("field") == "limit", f"{j}")
st, _, j = get_json("/api/products?limit=200")
check("limit=200 是合法上界", st == 200 and j.get("limit") == 200, f"limit={j.get('limit')}")
st, _, j = get_json("/api/products?limit=1")
check("limit=1 是合法下界", st == 200 and len(j["items"]) <= 1, f"len={len(j['items'])}")
st, _, j = get_json("/api/recommend?user_id=99999&limit=3")
check("未知用户冷启动不崩", st == 200 and len(j["items"]) > 0,
      f"{[it['product_id'] for it in j['items']]}")

print()
if fails:
    print(f"FAILED {len(fails)} 项: {fails}")
    sys.exit(1)
print("全部通过")
