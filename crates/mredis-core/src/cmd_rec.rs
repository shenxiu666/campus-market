//! 领域扩展命令：`REC.*`，把 `campus-rec` 的算法挂到 Redis 命令空间上。
//!
//! 这些命令既服务于 Web 层，也让项目能直接用 redis-cli 演示推荐算法。
//!
//! 设计要点：`REC.*` 返回的 JSON 字段名是 Web 前端的契约（见 SPEC 2.3），
//! 因此这里**手工**拼 `serde_json::Value` 而不是给 `campus_rec::Scored` 加
//! `Serialize`——算法 crate 不应该为了某个传输格式而被污染，而且 `Reason`
//! 的 JSON 形态（`{"type":"tag_match","tags":[...]}`）与 Rust 枚举的自然
//! 序列化结果（`{"TagMatch":{"tags":[...]}}`）并不一致。

use crate::db::{ExecCtx, format_float};
use crate::errors;
use campus_rec::{EventKind, Reason};
use mredis_proto::Resp;
use serde_json::{Value, json};

/// 未显式指定 `limit` 时的默认返回条数。
const DEFAULT_LIMIT: usize = 10;

/// 每个用户的最近行为最多保留多少条（SPEC 2.3 的 `LTRIM 0 199`）。
const EVENTS_KEEP: i64 = 200;

/// 打分保留 4 位小数：JSON 里 `0.8123` 比 `0.8123456789012345` 好读，
/// 也避免浮点尾数让前端「看起来每次都不一样」。
fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// 把推荐理由翻译成 SPEC 2.3 规定的 JSON 形态。
fn reason_json(r: &Reason) -> Value {
    match r {
        Reason::TagMatch { tags } => json!({"type": "tag_match", "tags": tags}),
        Reason::SimilarUsers { users } => json!({"type": "similar_users", "users": users}),
        Reason::CampusMatch => json!({"type": "campus_match"}),
        Reason::Trending { factor } => json!({"type": "trending", "factor": round4(*factor)}),
        Reason::PriceFit => json!({"type": "price_fit"}),
        Reason::ColdStart => json!({"type": "cold_start"}),
    }
}

/// 把 `Scored` 列表序列化成 SPEC 2.3 的数组。
fn scored_json(items: &[campus_rec::Scored]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|s| {
                json!({
                    "product_id": s.product_id,
                    "score": round4(s.score),
                    "reasons": s.reasons.iter().map(reason_json).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// 解析可选的 `limit` 参数；非法值返回错误文案。
fn parse_limit(args: &[Vec<u8>], idx: usize) -> Result<usize, String> {
    match args.get(idx) {
        None => Ok(DEFAULT_LIMIT),
        Some(raw) => {
            let n = crate::db::parse_i64(raw)?;
            // limit <= 0 在 Redis 里通常等价于「不返回」，这里统一当成 0。
            Ok(n.clamp(0, 10_000) as usize)
        }
    }
}

/// 解析用户 / 商品 id。
fn parse_id(bytes: &[u8]) -> Result<u64, String> {
    let n = crate::db::parse_i64(bytes)?;
    if n < 0 {
        return Err(errors::NOT_INT.to_string());
    }
    Ok(n as u64)
}

/// 给 `product:<id>` 的某个计数字段 +1，返回新值。
///
/// 商品可能不存在（比如客户端先 TRACK 再建商品），此时直接创建哈希并置 1，
/// 这样 `REC.TRACK` 永远不会因为「商品还没入库」而失败——埋点命令应该是
/// 尽力而为的。
fn bump_product_counter(ctx: &mut ExecCtx<'_>, pid: u64, field: &str) -> Result<i64, String> {
    let key = format!("product:{pid}").into_bytes();
    let hash = ctx.db.ensure_hash(&key)?;
    let cur = hash
        .get(field.as_bytes())
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let next = cur.saturating_add(1);
    hash.insert(field.as_bytes().to_vec(), next.to_string().into_bytes());
    Ok(next)
}

/// `REC.TRACK <user_id> <product_id> <view|click|fav>` —— 记录一次用户行为。
///
/// 一条命令要同时维护五份状态，缺任何一份都会让后面的演示不完整：
/// * `product:<pid>` 的 `views/clicks/favs`：`REC.EXPLAIN` 的热度项与 INFO 都读它；
/// * `user:<uid>:events`：用户行为流水，只留最近 200 条，防止无限增长；
/// * `user:<uid>:favs`：收藏集合（权威数据，不受 200 条窗口影响）；
/// * `zset:hot`：热度榜，用有序集合而不是现算，让 `REC.HOT` 退化成一次 `top(k)`；
/// * `db.rec`：推荐引擎内存里的共现矩阵与用户画像。
///
/// 这五份状态**必须**在同一个命令里更新：它们代表同一件事的五个视角，
/// 分开更新就必然出现「榜单里已加分、解释里却还是旧热度」的不一致。
pub fn cmd_rec_track(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    if args.len() != 4 {
        return Resp::error(errors::wrong_args("rec.track"));
    }
    let uid = match parse_id(&args[1]) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let pid = match parse_id(&args[2]) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let kind_str = match std::str::from_utf8(&args[3]) {
        Ok(s) => s,
        Err(_) => return Resp::error(errors::SYNTAX),
    };
    let Some(kind) = EventKind::parse(kind_str) else {
        return Resp::error(errors::SYNTAX);
    };

    let ts = crate::expire::now_secs();

    // 1) 商品计数器
    let field = match kind {
        EventKind::View => "views",
        EventKind::Click => "clicks",
        EventKind::Fav => "favs",
    };
    if let Err(e) = bump_product_counter(ctx, pid, field) {
        return Resp::error(e);
    }

    // 2) 用户行为流水（LPUSH 到头部 = 最新在前，再 LTRIM 只留 200 条）
    let events_key = format!("user:{uid}:events").into_bytes();
    let payload = json!({
        "product_id": pid,
        "kind": kind.as_str(),
        "ts": ts,
    })
    .to_string();
    match ctx.db.ensure_list(&events_key) {
        Ok(list) => {
            list.push_front(payload.into_bytes());
            while list.len() > EVENTS_KEEP as usize {
                list.pop_back();
            }
        }
        Err(e) => return Resp::error(e),
    }

    // 3) 收藏集合：独立于 200 条事件窗口的权威数据。
    //    没有它，用户收藏超过 200 次行为后，重启会忘记早期收藏，
    //    「排除已收藏」的推荐规则就会失效。
    if kind == EventKind::Fav {
        let favs_key = format!("user:{uid}:favs").into_bytes();
        match ctx.db.ensure_set(&favs_key) {
            Ok(set) => {
                set.insert(pid.to_string().into_bytes(), ());
            }
            Err(e) => return Resp::error(e),
        }
    }

    // 4) 热度榜：增量用事件权重（view=1 / click=2 / fav=3），
    //    这样一次收藏比一次浏览更能抬高排名。
    let delta = kind.weight();
    if let Ok(z) = ctx.db.ensure_zset(b"zset:hot") {
        let member = pid.to_string().into_bytes();
        let base = z.score(&member).unwrap_or(0.0);
        z.insert(member, base + delta);
    }

    // 5) 推荐引擎
    ctx.db.rec.record_event(campus_rec::model::Event {
        user_id: uid,
        product_id: pid,
        kind,
        ts,
    });

    // AOF 里记的是原始命令，重放时这一条会重新触发上面五步，天然幂等。
    ctx.mark_dirty(args);
    Resp::Int(1)
}

/// `REC.FOR_YOU <user_id> [limit]` —— 返回 JSON 数组的推荐结果（含理由）。
pub fn cmd_rec_for_you(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let uid = match parse_id(&args[1]) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let limit = match parse_limit(args, 2) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let items = ctx
        .db
        .rec
        .recommend_for_user(uid, limit, &[], crate::expire::now_secs());
    Resp::bulk(scored_json(&items).to_string())
}

/// `REC.SIMILAR <product_id> [limit]` —— 相似商品。
pub fn cmd_rec_similar(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let pid = match parse_id(&args[1]) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let limit = match parse_limit(args, 2) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let items = ctx
        .db
        .rec
        .similar_products(pid, limit, crate::expire::now_secs());
    Resp::bulk(scored_json(&items).to_string())
}

/// `REC.HOT [limit]` —— 热度榜。
pub fn cmd_rec_hot(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let limit = match parse_limit(args, 1) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let items = ctx.db.rec.hot_ranking(limit, crate::expire::now_secs());
    Resp::bulk(scored_json(&items).to_string())
}

/// `REC.REBUILD` —— 按当前键空间整体重建推荐索引。
///
/// 商品被删除或批量修改后使用（Web 的 `DELETE /api/products/{id}` 就靠它）。
/// 这是纯派生操作：**不写 AOF**，因为键空间才是事实来源，重启时本来就会
/// 重新构建一次；把它记进 AOF 只会让文件无谓增长。
pub fn cmd_rec_rebuild(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    if args.len() != 1 {
        return Resp::error(errors::wrong_args("rec.rebuild"));
    }
    let summary = crate::recbuild::rebuild_rec_from_keyspace(ctx.db);
    eprintln!("mredis-server: {}", summary.line());
    Resp::ok()
}

/// `REC.EXPLAIN <product_id> <user_id>` —— 逐项拆解打分。
///
/// `weights` 直接读 `campus_rec` 的常量而不是写死数字：算法侧调权重时
/// 解释面板会跟着变，不会出现「文档说 0.45、实际按 0.5 算」的漂移。
pub fn cmd_rec_explain(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let pid = match parse_id(&args[1]) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let uid = match parse_id(&args[2]) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };
    let Some(exp) = ctx.db.rec.explain(uid, pid, crate::expire::now_secs()) else {
        // 商品或用户不在索引里：返回 Null，让调用方区分「算不出来」和「算出来是 0」。
        return Resp::Null;
    };
    let body = json!({
        "product_id": exp.product_id,
        "score": round4(exp.score),
        "parts": {
            "tag_sim": round4(exp.tag_sim),
            "cf_sim": round4(exp.cf_sim),
            "hot_norm": round4(exp.hot_norm),
            "price_fit": round4(exp.price_fit),
            "campus_fit": round4(exp.campus_fit),
        },
        "weights": {
            "tag": campus_rec::W_TAG,
            "cf": campus_rec::W_CF,
            "hot": campus_rec::W_HOT,
            "price": campus_rec::W_PRICE,
            "campus": campus_rec::W_CAMPUS,
        },
        "matched_tags": exp.matched_tags,
        "similar_users": exp.similar_users,
    });
    Resp::bulk(body.to_string())
}

/// `REC.UPSERT <product_id>` —— 把 `product:<id>` 哈希同步进推荐索引。
///
/// ## 为什么需要这条命令
///
/// Web 端发布商品（`POST /api/products`）只发一串普通写命令
/// （`HSET product:65 ...` + `SADD tag:xxx 65` + `ZADD product:index:created`），
/// 这些命令**只改键空间**，不碰 `db.rec`。而 `recommend_for_user` 的候选集
/// 来自 `rec.products`，所以新商品在**本次运行期**永远进不了推荐结果。
///
/// 重启后 [`crate::recbuild::rebuild_rec_from_keyspace`] 能把索引补回来，
/// 但「发布完要重启一次才被推荐」显然不能接受，因此需要这条显式同步命令。
///
/// ## 语义
///
/// 从 `product:<id>` 哈希读出字段 → `rec.upsert_product(...)`。
/// 商品不存在返回错误；重复调用幂等（`upsert_product` 内部先摘旧标签再挂新标签）。
pub fn cmd_rec_upsert(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let pid = match parse_id(&args[1]) {
        Ok(v) => v,
        Err(e) => return Resp::error(e),
    };

    let key = format!("product:{pid}").into_bytes();
    // 先确认存在且是哈希：不存在时 `get_hash` 返回 `Ok(None)`。
    match ctx.db.get_hash(&key) {
        Ok(Some(_)) => {}
        Ok(None) => return Resp::error(format!("ERR product {pid} not found")),
        Err(e) => return Resp::error(e),
    }

    // 借用到此结束，下面要可变借用 `ctx.db.rec`。
    let product = {
        let Ok(Some(hash)) = ctx.db.get_hash(&key) else {
            return Resp::error(format!("ERR product {pid} not found"));
        };
        match crate::recbuild::product_from_hash(pid, hash) {
            Some(p) => p,
            // 哈希存在但没有 `title`：说明它只是 `REC.TRACK` 打点留下的计数器，
            // 不是一个真实商品。
            None => return Resp::error(format!("ERR product {pid} has no title")),
        }
    };

    // 标签倒排由 `upsert_product` 维护，但 `db.product_tags` 是我们自己的
    // 缓存（`DEL product:<id>` 时用它摘标签），必须同步。
    ctx.db.product_tags.insert(pid, product.tags.clone());
    ctx.db.rec.upsert_product(product);

    // 进 AOF：重放时会再 upsert 一次（幂等），但更重要的是让
    // 「重启后 rebuild 之前」的中间态也正确。
    ctx.mark_dirty(args);
    Resp::ok()
}

/// `REC.STATS` —— 索引规模统计（商品数、标签数、共现对数）。
pub fn cmd_rec_stats(ctx: &mut ExecCtx<'_>, _args: &[Vec<u8>]) -> Resp {
    let s = ctx.db.rec.stats();
    let body = json!({
        "products": s.products,
        "users": s.users,
        "tags": s.tags,
        "co_occurrence_pairs": s.co_occurrence_pairs,
        "events": s.events,
    });
    Resp::bulk(body.to_string())
}

/// 供 `INFO` 之外的地方复用的浮点格式化（避免各文件各写一份）。
#[allow(dead_code)]
pub(crate) fn fmt_score(v: f64) -> String {
    format_float(v)
}

/// 解析形如 `product:12` / `user:7` 的键，返回 id 部分。
///
/// 只做「前缀 + 十进制数字」的严格匹配：`product:12abc` 不算命中，
/// 否则 `DEL product:12abc` 会误删 `product:12` 的索引项。
pub(crate) fn parse_prefixed_id(key: &[u8], prefix: &[u8]) -> Option<u64> {
    let rest = key.strip_prefix(prefix)?;
    if rest.is_empty() || !rest.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(rest).ok()?.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch;
    use crate::db::Database;
    use mredis_proto::Resp;

    fn call(db: &mut Database, parts: &[&str]) -> Resp {
        let args: Vec<Vec<u8>> = parts.iter().map(|p| p.as_bytes().to_vec()).collect();
        let mut ctx = ExecCtx::new(db);
        dispatch(&mut ctx, &args)
    }

    fn json_of(resp: Resp) -> Value {
        match resp {
            Resp::Bulk(b) => serde_json::from_slice(&b).expect("应是合法 JSON"),
            other => panic!("期望 Bulk JSON，得到 {other:?}"),
        }
    }

    #[test]
    fn track_updates_four_views_of_the_same_event() {
        let mut db = Database::new();
        assert_eq!(
            call(&mut db, &["REC.TRACK", "1", "7", "view"]),
            Resp::Int(1)
        );
        assert_eq!(
            call(&mut db, &["REC.TRACK", "1", "7", "click"]),
            Resp::Int(1)
        );
        assert_eq!(call(&mut db, &["REC.TRACK", "1", "7", "fav"]), Resp::Int(1));

        // 商品计数器
        let h = match db.get(b"product:7") {
            Some(crate::obj::Obj::Hash(h)) => h,
            other => panic!("product:7 应是哈希，得到 {other:?}"),
        };
        assert_eq!(
            h.get(b"views".as_slice()).map(|v| v.as_slice()),
            Some(&b"1"[..])
        );
        assert_eq!(
            h.get(b"clicks".as_slice()).map(|v| v.as_slice()),
            Some(&b"1"[..])
        );
        assert_eq!(
            h.get(b"favs".as_slice()).map(|v| v.as_slice()),
            Some(&b"1"[..])
        );

        // 行为流水：最新在前
        let len = call(&mut db, &["LLEN", "user:1:events"]);
        assert_eq!(len, Resp::Int(3));
        let head = call(&mut db, &["LRANGE", "user:1:events", "0", "0"]);
        match head {
            Resp::Array(items) => {
                let s = items[0].as_string().expect("应是字符串");
                assert!(s.contains("\"kind\":\"fav\""), "最新一条应是 fav：{s}");
            }
            other => panic!("LRANGE 应返回数组，得到 {other:?}"),
        }

        // 热度：1 + 2 + 3 = 6
        let score = call(&mut db, &["ZSCORE", "zset:hot", "7"]);
        assert_eq!(score.as_string().as_deref(), Some("6"));
    }

    #[test]
    fn track_rejects_bad_kind_and_bad_id() {
        let mut db = Database::new();
        assert!(call(&mut db, &["REC.TRACK", "1", "2", "like"]).is_error());
        assert!(call(&mut db, &["REC.TRACK", "abc", "2", "view"]).is_error());
        assert!(call(&mut db, &["REC.TRACK", "-1", "2", "view"]).is_error());
        assert!(call(&mut db, &["REC.TRACK", "1", "2"]).is_error());
    }

    #[test]
    fn events_list_is_trimmed_to_200() {
        let mut db = Database::new();
        for _ in 0..205 {
            let r = call(&mut db, &["REC.TRACK", "9", "1", "view"]);
            assert_eq!(r, Resp::Int(1));
        }
        assert_eq!(call(&mut db, &["LLEN", "user:9:events"]), Resp::Int(200));
    }

    #[test]
    fn for_you_without_user_returns_empty_array() {
        let mut db = Database::new();
        let v = json_of(call(&mut db, &["REC.FOR_YOU", "1"]));
        assert!(v.is_array());
        assert!(v.as_array().expect("数组").is_empty());
    }

    #[test]
    fn similar_and_hot_are_json_arrays() {
        let mut db = Database::new();
        assert!(json_of(call(&mut db, &["REC.SIMILAR", "1"])).is_array());
        assert!(json_of(call(&mut db, &["REC.HOT"])).is_array());
    }

    #[test]
    fn stats_shape_matches_spec() {
        let mut db = Database::new();
        let v = json_of(call(&mut db, &["REC.STATS"]));
        for key in ["products", "users", "tags", "co_occurrence_pairs", "events"] {
            assert!(v.get(key).is_some(), "缺少字段 {key}");
        }
        assert_eq!(v["events"], 0);
    }

    #[test]
    fn explain_returns_null_when_unknown() {
        let mut db = Database::new();
        assert_eq!(call(&mut db, &["REC.EXPLAIN", "1", "2"]), Resp::Null);
    }

    #[test]
    fn limit_zero_is_allowed_and_negative_rejected() {
        let mut db = Database::new();
        assert!(json_of(call(&mut db, &["REC.HOT", "0"])).is_array());
        assert!(call(&mut db, &["REC.HOT", "abc"]).is_error());
    }

    /// 造一个 `product:<id>` 哈希（字段与 SPEC 5.1 一致）。
    fn put_product(db: &mut Database, id: &str, tags: &str) {
        call(
            db,
            &[
                "HSET",
                &format!("product:{id}"),
                "id",
                id,
                "title",
                &format!("商品{id}"),
                "desc",
                "描述",
                "price_cents",
                "1000",
                "tags",
                tags,
                "seller_id",
                "1",
                "condition",
                "9",
                "campus",
                "前湖校区",
                "created_at",
                "1700000000",
                "status",
                "onsale",
                "views",
                "0",
                "clicks",
                "0",
                "favs",
                "0",
            ],
        );
    }

    #[test]
    fn upsert_adds_product_to_index_and_is_idempotent() {
        let mut db = Database::new();
        put_product(&mut db, "10", "自行车,运动");

        // 入库前：索引里没有商品
        assert_eq!(json_of(call(&mut db, &["REC.STATS"]))["products"], 0);

        assert_eq!(call(&mut db, &["REC.UPSERT", "10"]), Resp::ok());
        assert_eq!(json_of(call(&mut db, &["REC.STATS"]))["products"], 1);

        // 幂等：同 id 再 upsert 一次，商品数不变（不能出现重复索引项）
        assert_eq!(call(&mut db, &["REC.UPSERT", "10"]), Resp::ok());
        assert_eq!(json_of(call(&mut db, &["REC.STATS"]))["products"], 1);
        assert_eq!(json_of(call(&mut db, &["REC.STATS"]))["tags"], 2);

        // 共享标签召回：upsert 一个同标签商品后，`REC.SIMILAR 10` 能召回它
        put_product(&mut db, "11", "自行车");
        assert_eq!(call(&mut db, &["REC.UPSERT", "11"]), Resp::ok());
        let similar = json_of(call(&mut db, &["REC.SIMILAR", "10", "5"]));
        let ids: Vec<u64> = similar
            .as_array()
            .expect("数组")
            .iter()
            .filter_map(|v| v["product_id"].as_u64())
            .collect();
        assert!(ids.contains(&11), "共享标签的商品应被召回，实得 {ids:?}");
    }

    #[test]
    fn upsert_rejects_missing_or_titleless_product() {
        let mut db = Database::new();
        // 完全不存在
        let missing = call(&mut db, &["REC.UPSERT", "99"]);
        assert!(missing.is_error());
        assert!(
            missing
                .as_string()
                .unwrap_or_default()
                .contains("not found"),
            "文案不对: {missing:?}"
        );

        // 只有计数器、没有 title（`REC.TRACK` 打到未入库商品的产物）
        call(&mut db, &["REC.TRACK", "1", "98", "view"]);
        let titleless = call(&mut db, &["REC.UPSERT", "98"]);
        assert!(titleless.is_error());
        assert!(
            titleless
                .as_string()
                .unwrap_or_default()
                .contains("no title"),
            "文案不对: {titleless:?}"
        );

        // 参数非法
        assert!(call(&mut db, &["REC.UPSERT", "abc"]).is_error());
        assert!(call(&mut db, &["REC.UPSERT", "-1"]).is_error());
        assert!(call(&mut db, &["REC.UPSERT"]).is_error());
    }

    #[test]
    fn rebuild_drops_products_removed_from_keyspace() {
        let mut db = Database::new();
        put_product(&mut db, "10", "自行车,运动");
        assert_eq!(call(&mut db, &["REC.UPSERT", "10"]), Resp::ok());
        assert_eq!(json_of(call(&mut db, &["REC.STATS"]))["products"], 1);

        // 模拟「只删键空间、索引没跟着清」，rebuild 应该把索引拉回一致。
        call(&mut db, &["DEL", "product:10"]);
        assert_eq!(call(&mut db, &["REC.REBUILD"]), Resp::ok());
        assert_eq!(json_of(call(&mut db, &["REC.STATS"]))["products"], 0);
    }

    #[test]
    fn fav_track_writes_authoritative_fav_set() {
        let mut db = Database::new();
        put_product(&mut db, "2", "自行车");
        assert_eq!(call(&mut db, &["REC.TRACK", "1", "2", "fav"]), Resp::Int(1));
        // `user:<uid>:favs` 是独立于 200 条事件窗口的权威收藏集合。
        assert_eq!(
            call(&mut db, &["SISMEMBER", "user:1:favs", "2"]),
            Resp::Int(1)
        );
        // 非 fav 行为不应写进收藏集合。
        call(&mut db, &["REC.TRACK", "1", "3", "view"]);
        assert_eq!(
            call(&mut db, &["SISMEMBER", "user:1:favs", "3"]),
            Resp::Int(0)
        );
    }
}
