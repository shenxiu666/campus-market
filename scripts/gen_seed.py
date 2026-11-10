#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成 data/seed.json —— 校园二手交易种子数据。

设计原则：
1. 确定性（固定随机种子），每次生成结果一致，便于复现测试。
2. 覆盖推荐算法的关键场景：热门商品、冷门商品、冷启动用户、跨标签商品。
3. 价格分布贴近真实学生二手市场（几十元到几千元）。
"""

import json
import random
from pathlib import Path

random.seed(20261109)

CAMPUSES = ["前湖校区", "青山湖校区", "东湖校区", "医学部校区"]

# (标签, 价格区间(元), 标题模板)
CATALOG = [
    (["自行车", "运动"], (200, 900), ["捷安特山地车", "美利达公路车", "折叠自行车", "死飞单车", "电动滑板车"]),
    (["自行车", "通勤"], (150, 600), ["通勤自行车", "二手电动车"]),
    (["教材", "计算机"], (10, 80), ["《算法导论》第三版", "《深入理解计算机系统》", "《数据结构与算法分析》", "《操作系统概念》"]),
    (["教材", "数学"], (8, 60), ["《高等数学》上下册", "《线性代数》同济版", "《概率论与数理统计》"]),
    (["教材", "英语"], (5, 40), ["四级真题全套", "雅思真题 4-17", "考研英语词汇书"]),
    (["电子", "数码"], (300, 2500), ["iPad 第九代 64G", "小米平板 5", "Kindle 电子书阅读器", "蓝牙耳机", "机械键盘"]),
    (["电子", "电脑"], (800, 6000), ["联想 ThinkPad T480", "罗技 MX Master 3", "27 寸显示器", "游戏鼠标"]),
    (["家电", "宿舍"], (30, 400), ["宿舍小冰箱", "电热水壶", "桌面小风扇", "加湿器", "吹风机"]),
    (["家具", "宿舍"], (20, 300), ["宿舍床帘", "折叠书桌", "收纳箱三件套", "人体工学椅", "置物架"]),
    (["运动", "健身"], (30, 500), ["哑铃 15kg 一对", "瑜伽垫", "羽毛球拍", "篮球", "跑步机"]),
    (["乐器", "音乐"], (150, 1500), ["尤克里里", "民谣吉他", "电子琴", "口琴"]),
    (["衣物", "二手"], (15, 200), ["羽绒服", "运动外套", "正装一套", "军训服"]),
    (["图书", "文学"], (10, 60), ["《活着》余华", "《三体》全集", "《百年孤独》", "《人类简史》"]),
    (["考研", "资料"], (20, 150), ["考研数学复习全书", "考研政治肖四肖八", "专业课真题汇编"]),
    (["生活", "日用"], (5, 60), ["台灯", "插线板", "水杯", "雨伞", "行李箱"]),
    (["数码", "配件"], (20, 200), ["充电宝 20000mAh", "Type-C 数据线", "手机支架"]),
]

# 8 个用户，覆盖：多兴趣、单兴趣、无兴趣（冷启动）、高价位、低价位
USERS = [
    {"id": 1, "name": "张三", "campus": "前湖校区", "interests": ["自行车", "运动", "健身"], "avg_price": 45000},
    {"id": 2, "name": "李四", "campus": "前湖校区", "interests": ["教材", "计算机", "考研"], "avg_price": 4000},
    {"id": 3, "name": "王五", "campus": "青山湖校区", "interests": ["电子", "数码", "电脑"], "avg_price": 150000},
    {"id": 4, "name": "赵六", "campus": "东湖校区", "interests": ["家具", "宿舍", "生活"], "avg_price": 15000},
    {"id": 5, "name": "孙七", "campus": "前湖校区", "interests": ["乐器", "音乐"], "avg_price": 60000},
    {"id": 6, "name": "周八", "campus": "医学部校区", "interests": ["图书", "文学", "教材"], "avg_price": 3000},
    {"id": 7, "name": "吴九", "campus": "青山湖校区", "interests": [], "avg_price": 0},  # 冷启动用户
    {"id": 8, "name": "郑十", "campus": "东湖校区", "interests": ["家电", "宿舍", "数码"], "avg_price": 20000},
]

# 基准时间：2026-11-09 12:00:00 +08:00
NOW = 1794283200

products = []
pid = 1
# 保证每个用户都发布若干商品，且卖家分布有重复（用于「同卖家最多 2 个」重排演示）
seller_cycle = [1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 7, 8, 1, 3, 5, 2, 4, 6]

# 成色用词 → condition 数值。两者必须一致，否则卡片上会出现
# 「成色 9/10」配「七成新」这种自相矛盾的展示。
COND_BY_WORD = {"七成新": 7, "八成新": 8, "九成新": 9, "九五新": 10, "几乎全新": 10}

for tag_pair, (lo, hi), titles in CATALOG:
    for title in titles:
        price_yuan = random.randint(lo, hi)
        seller = seller_cycle[(pid - 1) % len(seller_cycle)]
        # 时间分布：越新的商品越靠前，跨度 30 天
        created_at = NOW - random.randint(0, 30 * 86400)
        cond_word = random.choice(["九成新", "八成新", "九五新", "七成新", "几乎全新"])
        wear = random.choice(["无划痕", "轻微使用痕迹", "功能完好", "配件齐全", "可小刀"])
        deal = random.choice(["支持当面交易", "可送到宿舍楼下", "仅限本校交易", "周末可约"])
        # 历史上这里还有一次独立抽样决定 condition，导致成色与文案不一致。
        # 现在 condition 由文案反推；保留这次抽样只为让后续随机流不变
        # （事件序列、共现对数等已有文档记录的数值都依赖它）。
        _ = random.choice([7, 8, 8, 9, 9, 9, 10])
        products.append(
            {
                "id": pid,
                "title": title,
                "desc": f"{title}，{cond_word}，{wear}，{deal}。",
                "price_cents": price_yuan * 100,
                "tags": tag_pair,
                "seller_id": seller,
                "condition": COND_BY_WORD[cond_word],
                "campus": CAMPUSES[(pid - 1) % len(CAMPUSES)],
                "created_at": created_at,
                "status": "onsale",
            }
        )
        pid += 1

# ---- 行为事件 ----
# 关键约束：协同过滤的共现矩阵只统计「同一用户 5 分钟窗口内」的行为对。
# 如果所有事件都按小时级间隔排布，共现对数为 0，W_CF=0.35 就静默失效。
# 因此这里按「会话（session）」生成：一个会话 3~5 个行为，间隔 20~180 秒，
# 保证同一会话内的任意两条都在 300 秒窗口内，从而产生真实的共现信号。
# 目标：
#   1. 热门商品：某些商品被多个用户浏览/收藏（热度榜有意义）
#   2. 协同过滤信号：用户 1、3、8 在同一会话里看过「机械键盘/充电宝/蓝牙耳机」，
#      形成相似用户；同时各自的兴趣会话也制造大量共现对
#   3. 冷启动用户 7 只有零散浏览（有热度贡献，但 interests 为空 → 走冷启动分支）
events = []
CO_WINDOW = 300  # 与 campus-rec 的 CO_WINDOW_SECS 对齐


def add_session(uid, product_ids, base_ts):
    """生成一个会话：会话内相邻行为间隔 20~180 秒（< 300 秒窗口）。"""
    ts = base_ts
    for p_id in product_ids:
        ts += random.randint(20, 180)
        kind = random.choice(["view", "view", "view", "click", "click", "fav"])
        events.append({"user_id": uid, "product_id": p_id, "kind": kind, "ts": ts})
    return ts


def pick(tag_pred, n):
    return [p["id"] for p in products if tag_pred(p["tags"])][:n]


# 每个用户的兴趣会话：2 个会话，每个会话 4~5 个商品
interest_sessions = {
    1: pick(lambda t: "自行车" in t or "运动" in t or "健身" in t, 9),
    2: pick(lambda t: "教材" in t or "计算机" in t or "考研" in t, 9),
    3: pick(lambda t: "电子" in t or "数码" in t, 8),
    4: pick(lambda t: "家具" in t or "宿舍" in t, 8),
    5: pick(lambda t: "乐器" in t, 4),
    6: pick(lambda t: "图书" in t or "文学" in t, 6),
    8: pick(lambda t: "家电" in t or "数码" in t, 7),
}

for uid, ids in interest_sessions.items():
    half = max(1, len(ids) // 2)
    base = NOW - random.randint(2, 10) * 86400
    t = add_session(uid, ids[:half], base)
    add_session(uid, ids[half:], t + random.randint(3600, 20000))

# 跨用户共同会话：用户 1、3、8 各自在同一会话里看「机械键盘 / 充电宝 / 蓝牙耳机」，
# 这是协同过滤「相似用户」的核心信号来源。
cross = [p["id"] for p in products if p["title"] in ("机械键盘", "充电宝 20000mAh", "蓝牙耳机")]
for i, uid in enumerate((1, 3, 8)):
    add_session(uid, cross, NOW - 86400 - i * 1800)

# 热门商品：商品 1、2、20 被 5 个用户在相近时间浏览，制造高热度与共现
hot_ids = [1, 2, 20]
for i, uid in enumerate((2, 4, 5, 6, 7)):
    add_session(uid, hot_ids, NOW - 3 * 86400 - i * 900)

# 冷启动用户 7 的零散浏览（不形成会话，只有单点行为）
for p_id in (3, 15, 33):
    events.append({"user_id": 7, "product_id": p_id, "kind": "view",
                   "ts": NOW - random.randint(1, 20) * 86400})

events.sort(key=lambda e: e["ts"])

data = {"users": USERS, "products": products, "events": events}
out = Path(__file__).resolve().parent.parent / "data" / "seed.json"
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(json.dumps(data, ensure_ascii=False, indent=1), encoding="utf-8")
print(f"wrote {out}: {len(USERS)} users, {len(products)} products, {len(events)} events")
