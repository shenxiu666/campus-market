//! 种子数据导入（`mredis-server --seed <path>`）。
//!
//! 一键演示的主路径：把 `data/seed.json` 里的用户 / 商品 / 行为事件
//! 翻译成 SPEC 5.1 规定的键，同时把同一份数据灌进 `campus_rec::Recommender`。
//!
//! **为什么两边都要灌**：Redis 键空间是「给客户端看的状态」，推荐引擎是
//! 「给 `REC.*` 用的索引」。二者同源但形态不同——前者要能 `HGETALL product:1`，
//! 后者要能 O(1) 查标签倒排。只灌一边就会出现「Web 能列出商品、推荐却是空」。
//!
//! ## 幂等性与 AOF 的关系
//!
//! 导入**不写 AOF**，而且导入是**幂等**的（同一份 JSON 反复导入得到同一个键空间）。
//! 这两点合起来才是安全的设计：
//!
//! 启动顺序固定为「种子基线 → AOF 重放 → 重建 rec」。种子每次都**无条件**导入，
//! 因为它是确定性基线；AOF 里只记「基线之后发生过什么」，重放时不会与种子冲突
//! （AOF 里的写命令作用在同样的键上，后写覆盖先写，语义正确）。
//!
//! 若种子写进 AOF，重写（`BGREWRITEAOF`）之后种子命令会与真实数据混在一起，
//! 「哪些是基线、哪些是用户改的」就分不清了。
//!
//! 注意本函数里的 `db.rec = rec` 只是**中间态**：`Server::bind` 在 AOF 重放之后
//! 会用 [`crate::recbuild::rebuild_rec_from_keyspace`] 按最终键空间整体重建，
//! 覆盖掉这里的值。保留它是为了让 `seed::load_seed` 单独调用时（单测、工具）
//! 也能得到一个可用的推荐引擎。

use crate::cmd_hash::set_fields;
use crate::db::Database;
use campus_rec::Recommender;
use campus_rec::model::{Event, Product, User};
use serde::Deserialize;
use std::collections::BTreeMap;

/// `seed.json` 的顶层结构。
#[derive(Debug, Deserialize)]
struct SeedFile {
    #[serde(default)]
    users: Vec<User>,
    #[serde(default)]
    products: Vec<Product>,
    #[serde(default)]
    events: Vec<Event>,
}

/// 导入结果，供启动日志打印。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedSummary {
    pub products: usize,
    pub users: usize,
    pub events: usize,
}

impl SeedSummary {
    /// 启动日志用的一行文本（脚本会 grep 这一行）。
    pub fn line(&self, path: &str) -> String {
        format!(
            "Loaded seed: {} products, {} users, {} events from {}",
            self.products, self.users, self.events, path
        )
    }
}

/// 读取并导入种子文件。
///
/// 返回 `Err` 时调用方应视为启动失败：种子文件写错却静默启动，
/// 演示时只会看到「推荐为空」，比直接报错难查得多。
pub fn load_seed(db: &mut Database, path: &str) -> Result<SeedSummary, String> {
    let raw = std::fs::read(path).map_err(|e| format!("读取种子文件 {path} 失败: {e}"))?;
    let seed: SeedFile =
        serde_json::from_slice(&raw).map_err(|e| format!("解析种子文件 {path} 失败: {e}"))?;

    // 先把推荐引擎建好：`Recommender::new` 会按事件回填每个商品的
    // views/clicks/favs，后面写 `product:<id>` 哈希时直接用这份「已计数」的副本，
    // 保证键空间与索引对同一个商品的热度认知一致。
    let rec = Recommender::new(seed.products.clone(), seed.users.clone(), &seed.events);

    let mut hot: BTreeMap<u64, f64> = BTreeMap::new();
    for ev in &seed.events {
        *hot.entry(ev.product_id).or_insert(0.0) += ev.kind.weight();
    }

    // ---- 商品 ----
    for p in &rec.products {
        write_product(db, p);
        // 标签倒排：`tag:<tag>` 是集合，成员是商品 id 的十进制文本。
        for tag in &p.tags {
            let key = format!("tag:{tag}").into_bytes();
            if let Ok(set) = db.ensure_set(&key) {
                set.insert(p.id.to_string().into_bytes(), ());
            }
        }
        db.product_tags.insert(p.id, p.tags.clone());
        // 创建时间索引，供 Web 端「最新发布」排序。
        if let Ok(z) = db.ensure_zset(b"product:index:created") {
            z.insert(p.id.to_string().into_bytes(), p.created_at as f64);
        }
    }

    // ---- 用户 ----
    for u in &seed.users {
        write_user(db, u);
    }

    // ---- 热度榜 ----
    // 用事件权重累加而不是 `hot_score`：`REC.TRACK` 也是按权重累加的，
    // 两处口径一致，演示时「再点一次收藏，排名上升」才说得通。
    for (pid, score) in &hot {
        if let Ok(z) = db.ensure_zset(b"zset:hot") {
            z.insert(pid.to_string().into_bytes(), *score);
        }
    }

    // ---- 行为事件 ----
    for ev in &seed.events {
        push_event(db, ev);
    }

    // ---- 自增 id 游标 ----
    let next_pid = seed.products.iter().map(|p| p.id).max().unwrap_or(0) + 1;
    let next_uid = seed.users.iter().map(|u| u.id).max().unwrap_or(0) + 1;
    db.set(
        b"meta:next_product_id".to_vec(),
        crate::obj::Obj::Str(next_pid.to_string().into_bytes()),
    );
    db.set(
        b"meta:next_user_id".to_vec(),
        crate::obj::Obj::Str(next_uid.to_string().into_bytes()),
    );

    db.rec = rec;

    Ok(SeedSummary {
        products: seed.products.len(),
        users: seed.users.len(),
        events: seed.events.len(),
    })
}

/// 写一个 `product:<id>` 哈希，字段顺序与 SPEC 5.1 一致。
fn write_product(db: &mut Database, p: &Product) {
    let key = format!("product:{}", p.id).into_bytes();
    let fields: Vec<(&str, Vec<u8>)> = vec![
        ("id", p.id.to_string().into_bytes()),
        ("title", p.title.clone().into_bytes()),
        ("desc", p.desc.clone().into_bytes()),
        ("price_cents", p.price_cents.to_string().into_bytes()),
        // 标签在哈希里是英文逗号分隔的字符串（SPEC 5.1），空标签写空串。
        ("tags", p.tags.join(",").into_bytes()),
        ("seller_id", p.seller_id.to_string().into_bytes()),
        ("condition", p.condition.to_string().into_bytes()),
        ("campus", p.campus.clone().into_bytes()),
        ("created_at", p.created_at.to_string().into_bytes()),
        ("status", p.status.as_str().as_bytes().to_vec()),
        ("views", p.views.to_string().into_bytes()),
        ("clicks", p.clicks.to_string().into_bytes()),
        ("favs", p.favs.to_string().into_bytes()),
    ];
    set_fields(db, &key, &fields);
}

/// 写一个 `user:<id>` 哈希。
fn write_user(db: &mut Database, u: &User) {
    let key = format!("user:{}", u.id).into_bytes();
    let fields: Vec<(&str, Vec<u8>)> = vec![
        ("id", u.id.to_string().into_bytes()),
        ("name", u.name.clone().into_bytes()),
        ("campus", u.campus.clone().into_bytes()),
        ("interests", u.interests.join(",").into_bytes()),
        ("avg_price", u.avg_price.to_string().into_bytes()),
    ];
    set_fields(db, &key, &fields);
}

/// 把一条事件追加进 `user:<uid>:events`，只保留最近 200 条。
fn push_event(db: &mut Database, ev: &Event) {
    let key = format!("user:{}:events", ev.user_id).into_bytes();
    let payload = serde_json::json!({
        "product_id": ev.product_id,
        "kind": ev.kind.as_str(),
        "ts": ev.ts,
    })
    .to_string();
    if let Ok(list) = db.ensure_list(&key) {
        list.push_back(payload.into_bytes());
        while list.len() > 200 {
            list.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj::Obj;

    /// 最小可用种子：2 用户 / 2 商品 / 2 事件，覆盖标签、状态、空兴趣。
    const SAMPLE: &str = r#"{
      "users": [
        {"id":1,"name":"张三","campus":"前湖校区","interests":["自行车","运动"],"avg_price":40000},
        {"id":7,"name":"冷启动","campus":"前湖校区","interests":[],"avg_price":0}
      ],
      "products": [
        {"id":1,"title":"捷安特山地车","desc":"九成新","price_cents":38800,
         "tags":["自行车","运动"],"seller_id":1,"condition":9,"campus":"前湖校区",
         "created_at":1794083524,"status":"onsale"},
        {"id":2,"title":"高数教材","desc":"有笔记","price_cents":2500,
         "tags":["教材"],"seller_id":7,"condition":7,"campus":"前湖校区",
         "created_at":1794083999,"status":"reserved"}
      ],
      "events": [
        {"user_id":1,"product_id":1,"kind":"view","ts":1794025723},
        {"user_id":1,"product_id":1,"kind":"fav","ts":1794025730}
      ]
    }"#;

    fn temp_seed(name: &str, body: &str) -> String {
        let path = crate::testutil::temp_file(&format!("seed-{name}"), "seed.json");
        std::fs::write(&path, body).expect("写种子");
        path
    }

    fn hash_field(db: &mut Database, key: &[u8], field: &str) -> Option<String> {
        match db.get(key) {
            Some(Obj::Hash(h)) => h
                .get(field.as_bytes())
                .and_then(|v| std::str::from_utf8(v).ok())
                .map(|s| s.to_string()),
            _ => None,
        }
    }

    #[test]
    fn loads_products_users_tags_and_events() {
        let path = temp_seed("full", SAMPLE);
        let mut db = Database::new();
        let s = load_seed(&mut db, &path).expect("导入");
        assert_eq!(
            s,
            SeedSummary {
                products: 2,
                users: 2,
                events: 2
            }
        );
        assert_eq!(
            s.line("x.json"),
            "Loaded seed: 2 products, 2 users, 2 events from x.json"
        );

        // 商品哈希：标签用英文逗号连接，views/favs 由事件回填。
        assert_eq!(
            hash_field(&mut db, b"product:1", "tags").as_deref(),
            Some("自行车,运动")
        );
        assert_eq!(
            hash_field(&mut db, b"product:1", "status").as_deref(),
            Some("onsale")
        );
        assert_eq!(
            hash_field(&mut db, b"product:1", "views").as_deref(),
            Some("1")
        );
        assert_eq!(
            hash_field(&mut db, b"product:1", "favs").as_deref(),
            Some("1")
        );
        assert_eq!(
            hash_field(&mut db, b"product:1", "clicks").as_deref(),
            Some("0")
        );

        // 用户哈希：空兴趣写成空串。
        assert_eq!(
            hash_field(&mut db, b"user:7", "interests").as_deref(),
            Some("")
        );

        // 标签倒排（中文标签，用字符串而不是 byte literal）
        assert!(db.contains("tag:自行车".as_bytes()));
        assert!(db.contains("tag:教材".as_bytes()));

        // 自增游标
        assert!(matches!(db.get(b"meta:next_product_id"), Some(Obj::Str(v)) if v == b"3"));
        assert!(matches!(db.get(b"meta:next_user_id"), Some(Obj::Str(v)) if v == b"8"));

        // 事件流水（2 条）与热度（view 1 + fav 3 = 4）
        assert!(db.contains(b"user:1:events"));
        let score = match db.get(b"zset:hot") {
            Some(Obj::ZSet(z)) => z.score(b"1"),
            _ => None,
        };
        assert_eq!(score, Some(4.0));

        // 创建时间索引
        let created = match db.get(b"product:index:created") {
            Some(Obj::ZSet(z)) => z.score(b"1"),
            _ => None,
        };
        assert_eq!(created, Some(1794083524.0));
    }

    #[test]
    fn fills_recommender_so_rec_commands_are_not_empty() {
        let path = temp_seed("rec", SAMPLE);
        let mut db = Database::new();
        load_seed(&mut db, &path).expect("导入");
        let stats = db.rec.stats();
        assert_eq!(stats.products, 2);
        assert_eq!(stats.users, 2);
        assert!(stats.events >= 2);
        // 冷启动用户（无兴趣）也要能拿到热度榜兜底。
        assert!(!db.rec.recommend_for_user(7, 5, &[], 1794083600).is_empty());
    }

    #[test]
    fn missing_or_broken_file_reports_error() {
        let mut db = Database::new();
        let missing = temp_seed("missing", "{}");
        let missing = missing.replace("seed.json", "nope.json");
        assert!(load_seed(&mut db, &missing).is_err());
        let broken = temp_seed("broken", "{not json");
        assert!(load_seed(&mut db, &broken).is_err());
    }

    #[test]
    fn empty_seed_is_accepted() {
        let path = temp_seed("empty", r#"{"users":[],"products":[],"events":[]}"#);
        let mut db = Database::new();
        let s = load_seed(&mut db, &path).expect("空种子也应成功");
        assert_eq!(s.products, 0);
        assert_eq!(db.dbsize(), 2, "至少写下两个自增游标");
    }
}
