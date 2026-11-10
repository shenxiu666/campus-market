//! 从键空间重建推荐引擎（`db.rec`）。
//!
//! ## 为什么需要这个模块
//!
//! `Database` 里有两份「同一件事」的数据：
//!
//! * **键空间**（`db.data`）—— 权威状态，AOF 持久化的就是它；
//! * **`db.rec`**（`campus_rec::Recommender`）—— 内存索引，含倒排表、共现矩阵、
//!   用户画像，**不进 AOF**（它太大，而且完全可以由键空间推导）。
//!
//! 既然 rec 是键空间的**纯派生视图**，正确做法就是：**每次启动加载完键空间后，
//! 整体重建一次 rec**，而不是指望「种子导入」和 `REC.TRACK` 顺手维护它。
//!
//! 之前的设计是「seed 时灌一次 rec + REC.TRACK 增量维护」，它有个致命缺口：
//! AOF 重放只改键空间、不碰 rec，于是重启后推荐索引全空；`BGREWRITEAOF` 之后
//! 更糟——重写出的 AOF 里没有 `REC.TRACK` 命令，rec 永远回不来。
//!
//! 整体重建把「一致性」变成构造性质：只要键空间对，rec 必然对。

use crate::db::Database;
use crate::dict::Dict;
use crate::obj::Obj;
use campus_rec::Recommender;
use campus_rec::model::{Event, EventKind, Product, Status, User};
use std::collections::HashMap;

/// 重建结果，供启动日志使用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildSummary {
    /// 成功解析出的商品数
    pub products: usize,
    /// 成功解析出的用户数
    pub users: usize,
    /// 从 `user:<uid>:events` 收集到的事件数
    pub events: usize,
}

impl RebuildSummary {
    /// 启动日志用的一行文本。
    pub fn line(&self) -> String {
        format!(
            "Rebuilt recommender from keyspace: {} products, {} users, {} events",
            self.products, self.users, self.events
        )
    }
}

/// 把 `db.rec` 按当前键空间整体重建，并同步 `db.product_tags`。
///
/// 键空间为空时等价于 `Database::new()` 的初始状态（空索引）。
///
/// 三个数据源：
/// * `product:<id>` HASH  → `Product`
/// * `user:<id>` HASH     → `User`（自动排除 `user:<uid>:events`）
/// * `user:<uid>:events` LIST → `Event`
///
/// 另外读取 `user:<uid>:favs` SET 作为**权威收藏集合**：事件流水会被
/// `LTRIM` 截断到最近 200 条，收藏却不能因此丢失，否则「排除已收藏」
/// 的推荐规则会在重启后悄悄失效。
pub fn rebuild_rec_from_keyspace(db: &mut Database) -> RebuildSummary {
    // ---- 1. 商品 ----
    // 先把「哈希里的权威计数」收集出来。为什么不能直接把它们喂给
    // `Recommender::new`：`new` 会对每个事件再 `apply_event_count` 累加一次，
    // 而这些计数**本来就来自事件**，直接传会翻倍（重启一次涨一倍，很隐蔽）。
    let mut products: Vec<Product> = Vec::new();
    let mut tags: HashMap<u64, Vec<String>> = HashMap::new();
    for (key, obj) in db.data.iter() {
        let Some(id) = crate::cmd_rec::parse_prefixed_id(key, b"product:") else {
            continue;
        };
        let Obj::Hash(hash) = obj else {
            continue;
        };
        if let Some(p) = product_from_hash(id, hash) {
            tags.insert(p.id, p.tags.clone());
            products.push(p);
        }
    }

    // ---- 2. 用户 ----
    let mut users: Vec<User> = Vec::new();
    for (key, obj) in db.data.iter() {
        let Some(id) = crate::cmd_rec::parse_prefixed_id(key, b"user:") else {
            continue;
        };
        let Obj::Hash(hash) = obj else {
            continue;
        };
        if let Some(u) = user_from_hash(id, hash) {
            users.push(u);
        }
    }

    // ---- 3. 行为事件 ----
    let mut events: Vec<Event> = Vec::new();
    for (key, obj) in db.data.iter() {
        let Some(uid) = parse_events_key(key) else {
            continue;
        };
        let Obj::List(list) = obj else {
            continue;
        };
        for raw in list {
            if let Some(ev) = event_from_json(uid, raw) {
                events.push(ev);
            }
        }
    }

    // ---- 3b. 权威收藏集合 ----
    // 事件流水是会滚动的窗口，收藏集合不是：只要用户没取消收藏，
    // 重启后就必须还知道他收藏了什么。
    let mut fav_sets: HashMap<u64, Vec<u64>> = HashMap::new();
    for (key, obj) in db.data.iter() {
        let Some(uid) = parse_favs_key(key) else {
            continue;
        };
        let Obj::Set(set) = obj else {
            continue;
        };
        let mut ids: Vec<u64> = set
            .keys()
            .filter_map(|m| {
                std::str::from_utf8(m)
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if !ids.is_empty() {
            fav_sets.insert(uid, ids);
        }
    }

    // ---- 4. 构造 ----
    // 关键技巧：用**清零计数**的副本构造，让 `new` 里的事件累加成为唯一来源；
    // 再把哈希里的权威计数 `upsert_product` 覆盖回去。
    //
    // 这样两边都对：
    // * 计数不会被事件重复累加（覆盖成权威值）；
    // * `views/clicks/favs` 也不会归零（哈希里的值比 200 条流水更权威，
    //   因为流水会被 `LTRIM 0 199` 截断，而计数器不会）。
    let zeroed: Vec<Product> = products
        .iter()
        .map(|p| Product {
            views: 0,
            clicks: 0,
            favs: 0,
            ..p.clone()
        })
        .collect();

    let mut rec = Recommender::new(zeroed, users, &events);
    for p in &products {
        rec.upsert_product(p.clone());
    }
    // 收藏集合覆盖事件推导出的收藏：前者才是权威来源。
    for (uid, ids) in &fav_sets {
        rec.set_user_favs(*uid, ids.clone());
    }

    let summary = RebuildSummary {
        products: products.len(),
        users: rec.stats().users,
        events: rec.stats().events,
    };

    db.rec = rec;
    db.product_tags = tags;
    summary
}

/// 从一个 `product:<id>` 哈希解析出 [`Product`]；无法解析返回 `None`。
///
/// **只有 `title` 是硬性要求**：真实商品一定有标题，而没有标题的哈希几乎
/// 只可能来自「`REC.TRACK` 打到一个还没入库的商品」——那种哈希只有
/// `views`/`clicks`/`favs` 三个字段，不该被当成商品塞进推荐索引。
/// 其余字段缺失时取默认值，宁可少丢信息也不要整条丢弃。
pub(crate) fn product_from_hash(id: u64, hash: &Dict<Vec<u8>>) -> Option<Product> {
    let title = field_str(hash, "title")?;
    Some(Product {
        id,
        title,
        desc: field_str(hash, "desc").unwrap_or_default(),
        price_cents: field_i64(hash, "price_cents").unwrap_or(0),
        // SPEC 5.1：标签在哈希里是英文逗号分隔的字符串，空串表示无标签。
        tags: split_tags(&field_str(hash, "tags").unwrap_or_default()),
        seller_id: field_u64(hash, "seller_id").unwrap_or(0),
        condition: field_u64(hash, "condition")
            .unwrap_or(0)
            .min(u64::from(u8::MAX)) as u8,
        campus: field_str(hash, "campus").unwrap_or_default(),
        created_at: field_i64(hash, "created_at").unwrap_or(0),
        status: field_str(hash, "status")
            .and_then(|s| Status::parse(&s))
            .unwrap_or(Status::OnSale),
        views: field_u64(hash, "views").unwrap_or(0),
        clicks: field_u64(hash, "clicks").unwrap_or(0),
        favs: field_u64(hash, "favs").unwrap_or(0),
    })
}

/// 从一个 `user:<id>` 哈希解析出 [`User`]。
///
/// 与商品不同，用户没有必填字段：`name` 缺失时给空串也能正常参与推荐
/// （推荐只用 `interests` 与 `avg_price`）。
fn user_from_hash(id: u64, hash: &Dict<Vec<u8>>) -> Option<User> {
    Some(User {
        id,
        name: field_str(hash, "name").unwrap_or_default(),
        campus: field_str(hash, "campus").unwrap_or_default(),
        interests: split_tags(&field_str(hash, "interests").unwrap_or_default()),
        avg_price: field_i64(hash, "avg_price").unwrap_or(0),
    })
}

/// 解析 `user:<uid>:events` 里的 uid；不是这种键返回 `None`。
///
/// 注意与 `parse_prefixed_id(key, b"user:")` 的区别：后者对
/// `user:1:events` 返回 `None`（`1:events` 不是纯数字），正好把「事件列表」
/// 与「用户哈希」区分开。这里反过来专门抓事件列表。
fn parse_events_key(key: &[u8]) -> Option<u64> {
    let rest = key.strip_prefix(b"user:")?;
    let digits = rest.strip_suffix(b":events")?;
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse::<u64>().ok()
}

/// 解析 `user:<uid>:favs` 里的 uid；不是这种键返回 `None`。
fn parse_favs_key(key: &[u8]) -> Option<u64> {
    let rest = key.strip_prefix(b"user:")?;
    let digits = rest.strip_suffix(b":favs")?;
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse::<u64>().ok()
}

/// 解析一条事件 JSON：`{"product_id":1,"kind":"view","ts":1700000000}`。
///
/// **`ts` 必须用 JSON 里的真实值**，不能用「现在」：共现矩阵按
/// `CO_WINDOW_SECS`（300 秒）窗口配对，时间戳一旦被抹平，重启前后
/// 算出的共现关系就完全不同了。
fn event_from_json(user_id: u64, raw: &[u8]) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let product_id = v.get("product_id")?.as_u64()?;
    let kind = EventKind::parse(v.get("kind")?.as_str()?)?;
    let ts = v.get("ts").and_then(serde_json::Value::as_i64).unwrap_or(0);
    Some(Event {
        user_id,
        product_id,
        kind,
        ts,
    })
}

/// 取哈希里的字符串字段（UTF-8 解码失败视为缺失）。
fn field_str(hash: &Dict<Vec<u8>>, field: &str) -> Option<String> {
    let raw = hash.get(field.as_bytes())?;
    std::str::from_utf8(raw).ok().map(|s| s.to_string())
}

/// 取哈希里的整数字段；解析失败视为缺失。
fn field_i64(hash: &Dict<Vec<u8>>, field: &str) -> Option<i64> {
    field_str(hash, field)?.trim().parse::<i64>().ok()
}

/// 取哈希里的非负整数字段；负数与解析失败都视为缺失。
fn field_u64(hash: &Dict<Vec<u8>>, field: &str) -> Option<u64> {
    let n = field_i64(hash, field)?;
    if n < 0 { None } else { Some(n as u64) }
}

/// 把 `"自行车,运动"` 拆成 `["自行车", "运动"]`；空串得到空 vec。
///
/// 顺带过滤空片段：`"a,,b"` 不该产生一个名为 `""` 的标签，
/// 否则倒排索引里会多出一个谁都匹配不上的桶。
fn split_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd_hash::set_fields;

    /// 造一个最小的 `product:<id>` 哈希。
    fn put_product(db: &mut Database, id: u64, tags: &str, views: &str) {
        let key = format!("product:{id}").into_bytes();
        set_fields(
            db,
            &key,
            &[
                ("id", id.to_string().into_bytes()),
                ("title", format!("商品{id}").into_bytes()),
                ("desc", b"desc".to_vec()),
                ("price_cents", b"1000".to_vec()),
                ("tags", tags.as_bytes().to_vec()),
                ("seller_id", b"1".to_vec()),
                ("condition", b"9".to_vec()),
                ("campus", "前湖校区".as_bytes().to_vec()),
                ("created_at", b"1700000000".to_vec()),
                ("status", b"onsale".to_vec()),
                ("views", views.as_bytes().to_vec()),
                ("clicks", b"0".to_vec()),
                ("favs", b"0".to_vec()),
            ],
        );
    }

    fn put_user(db: &mut Database, id: u64, interests: &str) {
        let key = format!("user:{id}").into_bytes();
        set_fields(
            db,
            &key,
            &[
                ("id", id.to_string().into_bytes()),
                ("name", format!("用户{id}").into_bytes()),
                ("campus", "前湖校区".as_bytes().to_vec()),
                ("interests", interests.as_bytes().to_vec()),
                ("avg_price", b"5000".to_vec()),
            ],
        );
    }

    #[test]
    fn empty_keyspace_yields_empty_recommender() {
        let mut db = Database::new();
        let s = rebuild_rec_from_keyspace(&mut db);
        assert_eq!(s, RebuildSummary::default());
        assert_eq!(db.rec.stats().products, 0);
        assert_eq!(db.rec.stats().users, 0);
    }

    #[test]
    fn rebuilds_products_users_and_tags() {
        let mut db = Database::new();
        put_product(&mut db, 1, "自行车,运动", "3");
        put_product(&mut db, 2, "自行车", "0");
        put_user(&mut db, 1, "自行车");

        let s = rebuild_rec_from_keyspace(&mut db);
        assert_eq!(s.products, 2);
        assert_eq!(s.users, 1);
        assert_eq!(db.rec.stats().products, 2);
        assert_eq!(db.rec.stats().tags, 2);
        assert_eq!(
            db.product_tags.get(&1),
            Some(&vec!["自行车".to_string(), "运动".to_string()])
        );

        // 计数保持哈希里的权威值（这里没有事件，不会被归零）
        let p = db.rec.product(1).expect("商品 1 应在索引里");
        assert_eq!(p.views, 3);
    }

    #[test]
    fn event_counters_are_not_double_counted() {
        let mut db = Database::new();
        // 哈希里已经写着 views=2（来自 2 条事件的历史累加）
        put_product(&mut db, 1, "自行车", "2");
        put_user(&mut db, 1, "自行车");
        // 流水里也有 2 条 view
        let key = b"user:1:events".to_vec();
        for ts in [1700000000i64, 1700000010] {
            let payload = format!(r#"{{"product_id":1,"kind":"view","ts":{ts}}}"#);
            db.ensure_list(&key)
                .expect("建列表")
                .push_back(payload.into_bytes());
        }

        rebuild_rec_from_keyspace(&mut db);

        // 若把哈希计数直接喂给 `Recommender::new`，这里会变成 4（2 次累加）。
        let p = db.rec.product(1).expect("商品 1 应在索引里");
        assert_eq!(p.views, 2, "重启不应让计数翻倍");
        assert_eq!(db.rec.stats().events, 2);
    }

    #[test]
    fn product_without_title_is_skipped() {
        let mut db = Database::new();
        // 模拟 `REC.TRACK` 打到一个未入库的商品：哈希里只有计数器
        let key = b"product:9".to_vec();
        set_fields(&mut db, &key, &[("views", b"1".to_vec())]);

        let s = rebuild_rec_from_keyspace(&mut db);
        assert_eq!(s.products, 0, "没有 title 的哈希不该被当成商品");
        assert_eq!(db.rec.stats().products, 0);
    }

    #[test]
    fn events_list_is_not_mistaken_for_user_hash() {
        let mut db = Database::new();
        put_user(&mut db, 1, "运动");
        // 事件列表是 LIST，不是 HASH；两个判据都要能排除它
        let key = b"user:1:events".to_vec();
        db.ensure_list(&key)
            .expect("建列表")
            .push_back(br#"{"product_id":1,"kind":"fav","ts":1700000000}"#.to_vec());

        let s = rebuild_rec_from_keyspace(&mut db);
        assert_eq!(s.users, 1, "user:1:events 不该被当成用户");
        assert_eq!(s.events, 1);
    }

    #[test]
    fn malformed_event_json_is_skipped() {
        let mut db = Database::new();
        put_user(&mut db, 1, "运动");
        let key = b"user:1:events".to_vec();
        let list = db.ensure_list(&key).expect("建列表");
        list.push_back(b"not json".to_vec());
        list.push_back(br#"{"product_id":1,"kind":"unknown","ts":1}"#.to_vec());
        list.push_back(br#"{"product_id":2,"kind":"view","ts":2}"#.to_vec());

        let s = rebuild_rec_from_keyspace(&mut db);
        assert_eq!(s.events, 1, "只有最后一条是合法的");
    }

    #[test]
    fn split_tags_drops_empty_pieces() {
        assert_eq!(split_tags(""), Vec::<String>::new());
        assert_eq!(split_tags("a,,b"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            split_tags(" 自行车 , 运动 "),
            vec!["自行车".to_string(), "运动".to_string()]
        );
    }
}
