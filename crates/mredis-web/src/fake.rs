//! 测试用的内存版迷你 Redis。
//!
//! 实现 `/api/*` 真正用到的那一小撮命令（`HGETALL`/`HSET`/`SADD`/`ZADD`/`ZRANGE`/
//! `SCAN`/`KEYS`/`INCR`/`SET`/`GET`/`DBSIZE`/`INFO`/`REC.*`），
//! 让 HTTP 层可以在**没有 mredis-core 的情况下**端到端被测。
//!
//! 这不是 Redis 的完整实现，也不打算是 —— 它只保证「命令语义与 SPEC 一致」，
//! 从而 `tower::ServiceExt::oneshot` 打出来的响应能代表真实部署的行为。

use crate::state::Backend;
use mredis_proto::Resp;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

/// 内存版存储。
#[derive(Default)]
pub struct FakeBackend {
    inner: Mutex<Store>,
    /// 是否模拟存储不可用（测 503 分支）
    down: bool,
}

#[derive(Default)]
struct Store {
    hashes: HashMap<String, BTreeMap<String, String>>,
    sets: HashMap<String, BTreeSet<String>>,
    zsets: HashMap<String, BTreeMap<String, f64>>,
    strings: HashMap<String, String>,
    /// 最近命令流（最新在前）
    recent: Vec<String>,
    /// 累计命令数
    total_commands: u64,
}

impl FakeBackend {
    /// 构造一个正常工作的假后端。
    pub fn new() -> Self {
        Self::default()
    }

    /// 构造一个「存储不可用」的假后端，用来测 503。
    pub fn down() -> Self {
        Self {
            inner: Mutex::new(Store::default()),
            down: true,
        }
    }

    /// 预置一个商品 HASH。
    pub fn with_product(
        self,
        id: u64,
        title: &str,
        price_cents: i64,
        tags: &[&str],
        created_at: i64,
        hot: f64,
    ) -> Self {
        {
            let mut store = self.lock();
            let mut hash = BTreeMap::new();
            hash.insert("id".into(), id.to_string());
            hash.insert("title".into(), title.into());
            hash.insert("desc".into(), format!("{title}的描述"));
            hash.insert("price_cents".into(), price_cents.to_string());
            hash.insert("tags".into(), tags.join(","));
            hash.insert("seller_id".into(), "1".into());
            hash.insert("condition".into(), "9".into());
            hash.insert("campus".into(), "前湖校区".into());
            hash.insert("created_at".into(), created_at.to_string());
            hash.insert("status".into(), "onsale".into());
            hash.insert("views".into(), "0".into());
            hash.insert("clicks".into(), "0".into());
            hash.insert("favs".into(), "0".into());
            store.hashes.insert(format!("product:{id}"), hash);
            for tag in tags {
                store
                    .sets
                    .entry(format!("tag:{tag}"))
                    .or_default()
                    .insert(id.to_string());
            }
            store
                .zsets
                .entry("product:index:created".into())
                .or_default()
                .insert(id.to_string(), created_at as f64);
            store
                .zsets
                .entry("zset:hot".into())
                .or_default()
                .insert(id.to_string(), hot);
        }
        self
    }

    /// 预置一个用户 HASH。
    pub fn with_user(self, id: u64, name: &str, interests: &[&str]) -> Self {
        {
            let mut store = self.lock();
            let mut hash = BTreeMap::new();
            hash.insert("id".into(), id.to_string());
            hash.insert("name".into(), name.into());
            hash.insert("campus".into(), "前湖校区".into());
            hash.insert("interests".into(), interests.join(","));
            hash.insert("avg_price".into(), "45000".into());
            store.hashes.insert(format!("user:{id}"), hash);
        }
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Backend for FakeBackend {
    fn call(&self, args: &[&str]) -> Result<Resp, String> {
        if self.down {
            return Err("connection refused".to_string());
        }
        if args.is_empty() {
            return Ok(Resp::Null);
        }
        let mut store = self.lock();
        let command = args[0].to_ascii_uppercase();
        store.total_commands += 1;
        let line = args.join(" ");
        store.recent.insert(0, line);
        store.recent.truncate(50);

        let reply = match command.as_str() {
            "PING" => Resp::simple("PONG"),
            "DBSIZE" => Resp::Int(store.key_count() as i64),
            "FLUSHDB" => {
                store.hashes.clear();
                store.sets.clear();
                store.zsets.clear();
                store.strings.clear();
                Resp::ok()
            }
            "GET" => match args.get(1) {
                Some(key) => match store.strings.get(*key) {
                    Some(value) => Resp::bulk(value.clone()),
                    None => Resp::Null,
                },
                None => Resp::error("ERR wrong number of arguments"),
            },
            "SET" => match (args.get(1), args.get(2)) {
                (Some(key), Some(value)) => {
                    store
                        .strings
                        .insert((*key).to_string(), (*value).to_string());
                    Resp::ok()
                }
                _ => Resp::error("ERR wrong number of arguments"),
            },
            "INCR" => match args.get(1) {
                Some(key) => {
                    let current = store
                        .strings
                        .get(*key)
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(0);
                    let next = current + 1;
                    store.strings.insert((*key).to_string(), next.to_string());
                    Resp::Int(next)
                }
                None => Resp::error("ERR wrong number of arguments"),
            },
            "HSET" => {
                if args.len() < 4 || args.len() % 2 != 0 {
                    Resp::error("ERR wrong number of arguments for 'hset' command")
                } else {
                    let key = args[1].to_string();
                    let hash = store.hashes.entry(key).or_default();
                    let mut added = 0i64;
                    let mut i = 2;
                    while i + 1 < args.len() {
                        if hash
                            .insert(args[i].to_string(), args[i + 1].to_string())
                            .is_none()
                        {
                            added += 1;
                        }
                        i += 2;
                    }
                    Resp::Int(added)
                }
            }
            "HGETALL" => match args.get(1) {
                Some(key) => match store.hashes.get(*key) {
                    Some(hash) => Resp::Array(
                        hash.iter()
                            .flat_map(|(k, v)| [Resp::bulk(k.clone()), Resp::bulk(v.clone())])
                            .collect(),
                    ),
                    None => Resp::Array(Vec::new()),
                },
                None => Resp::error("ERR wrong number of arguments"),
            },
            "SADD" => {
                if args.len() < 3 {
                    Resp::error("ERR wrong number of arguments")
                } else {
                    let set = store.sets.entry(args[1].to_string()).or_default();
                    let mut added = 0i64;
                    for member in &args[2..] {
                        if set.insert((*member).to_string()) {
                            added += 1;
                        }
                    }
                    Resp::Int(added)
                }
            }
            "ZADD" => {
                if args.len() < 4 {
                    Resp::error("ERR wrong number of arguments")
                } else {
                    let Ok(score) = args[1 + 1].parse::<f64>() else {
                        return Ok(Resp::error("ERR value is not a valid float"));
                    };
                    let zset = store.zsets.entry(args[1].to_string()).or_default();
                    let mut added = 0i64;
                    for member in &args[3..] {
                        if zset.insert((*member).to_string(), score).is_none() {
                            added += 1;
                        }
                    }
                    Resp::Int(added)
                }
            }
            "ZRANGE" => {
                let with_scores = args.iter().any(|a| a.eq_ignore_ascii_case("WITHSCORES"));
                match args.get(1) {
                    Some(key) => {
                        let Some(zset) = store.zsets.get(*key) else {
                            return Ok(Resp::Array(Vec::new()));
                        };
                        // (member, score) 升序，同分按成员字典序
                        let mut entries: Vec<(&String, &f64)> = zset.iter().collect();
                        entries.sort_by(|a, b| {
                            a.1.partial_cmp(b.1)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| a.0.cmp(b.0))
                        });
                        let mut out = Vec::new();
                        for (member, score) in entries {
                            out.push(Resp::bulk(member.clone()));
                            if with_scores {
                                out.push(Resp::bulk(format!("{score:.6}")));
                            }
                        }
                        Resp::Array(out)
                    }
                    None => Resp::error("ERR wrong number of arguments"),
                }
            }
            "KEYS" => {
                let pattern = args.get(1).copied().unwrap_or("*");
                let mut keys: Vec<String> = store
                    .all_keys()
                    .into_iter()
                    .filter(|k| glob_match(pattern, k))
                    .collect();
                keys.sort();
                Resp::Array(keys.into_iter().map(Resp::bulk).collect())
            }
            "SCAN" => {
                // 假后端一次返回全部匹配键，游标恒为 0
                let pattern = args
                    .iter()
                    .position(|a| a.eq_ignore_ascii_case("MATCH"))
                    .and_then(|i| args.get(i + 1))
                    .copied()
                    .unwrap_or("*");
                let mut keys: Vec<String> = store
                    .all_keys()
                    .into_iter()
                    .filter(|k| glob_match(pattern, k))
                    .collect();
                keys.sort();
                Resp::Array(vec![
                    Resp::bulk("0"),
                    Resp::Array(keys.into_iter().map(Resp::bulk).collect()),
                ])
            }
            "INFO" => Resp::bulk(store.info_text()),
            "REC.TRACK" => {
                if args.len() < 4 {
                    Resp::error("ERR wrong number of arguments for 'rec.track' command")
                } else {
                    let product_key = format!("product:{}", args[2]);
                    let field = match args[3].to_ascii_lowercase().as_str() {
                        "view" => "views",
                        "click" => "clicks",
                        "fav" => "favs",
                        other => return Ok(Resp::error(format!("ERR bad kind '{other}'"))),
                    };
                    if let Some(hash) = store.hashes.get_mut(&product_key) {
                        let current = hash
                            .get(field)
                            .and_then(|v| v.parse::<i64>().ok())
                            .unwrap_or(0);
                        hash.insert(field.to_string(), (current + 1).to_string());
                        let zset = store.zsets.entry("zset:hot".into()).or_default();
                        let score = zset.get(&args[2].to_string()).copied().unwrap_or(0.0);
                        zset.insert(args[2].to_string(), score + 1.0);
                        Resp::Int(1)
                    } else {
                        Resp::error("ERR product not found")
                    }
                }
            }
            "REC.FOR_YOU" => {
                let user_id = args.get(1).and_then(|v| v.parse::<u64>().ok());
                let limit = args
                    .get(2)
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(10);
                let Some(user_id) = user_id else {
                    return Ok(Resp::error("ERR bad user id"));
                };
                Resp::bulk(store.fake_for_you(user_id, limit))
            }
            "REC.SIMILAR" => {
                let limit = args
                    .get(2)
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(6);
                Resp::bulk(store.fake_similar(args.get(1).copied().unwrap_or("0"), limit))
            }
            "REC.HOT" => {
                let limit = args
                    .get(1)
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(10);
                Resp::bulk(store.fake_hot(limit))
            }
            // `PATCH` / `DELETE` 用到的键级删除命令，按最小语义实现即可。
            "DEL" => {
                let mut removed = 0i64;
                for key in &args[1..] {
                    let mut hit = store.hashes.remove(*key).is_some();
                    hit |= store.sets.remove(*key).is_some();
                    hit |= store.zsets.remove(*key).is_some();
                    hit |= store.strings.remove(*key).is_some();
                    if hit {
                        removed += 1;
                    }
                }
                Resp::Int(removed)
            }
            "SREM" => {
                if args.len() < 3 {
                    Resp::error("ERR wrong number of arguments")
                } else {
                    let mut removed = 0i64;
                    if let Some(set) = store.sets.get_mut(args[1]) {
                        for member in &args[2..] {
                            if set.remove(*member) {
                                removed += 1;
                            }
                        }
                    }
                    Resp::Int(removed)
                }
            }
            "ZREM" => {
                if args.len() < 3 {
                    Resp::error("ERR wrong number of arguments")
                } else {
                    let mut removed = 0i64;
                    if let Some(zset) = store.zsets.get_mut(args[1]) {
                        for member in &args[2..] {
                            if zset.remove(*member).is_some() {
                                removed += 1;
                            }
                        }
                    }
                    Resp::Int(removed)
                }
            }
            "REC.REBUILD" => Resp::ok(),
            // 真实实现里这条命令把 `product:<id>` 哈希灌进 `Recommender`；
            // 假后端没有推荐引擎，只要校验商品存在并回 1，就足以让
            // `POST /api/products` 的那条通知调用走通。
            "REC.UPSERT" => match args.get(1) {
                Some(pid) if store.hashes.contains_key(&format!("product:{pid}")) => Resp::Int(1),
                Some(_) => Resp::error("ERR product not found"),
                None => Resp::error("ERR wrong number of arguments for 'rec.upsert' command"),
            },
            "REC.STATS" => Resp::bulk(store.fake_rec_stats()),
            other => Resp::error(format!("ERR unknown command '{other}'")),
        };
        Ok(reply)
    }
}

impl Store {
    fn key_count(&self) -> usize {
        self.hashes.len() + self.sets.len() + self.zsets.len() + self.strings.len()
    }

    fn all_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        keys.extend(self.hashes.keys().cloned());
        keys.extend(self.sets.keys().cloned());
        keys.extend(self.zsets.keys().cloned());
        keys.extend(self.strings.keys().cloned());
        keys
    }

    fn product_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .hashes
            .keys()
            .filter_map(|k| k.strip_prefix("product:")?.parse::<u64>().ok())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// 假的 `REC.FOR_YOU`：按 zset:hot 分数降序，用户 7 给冷启动理由。
    fn fake_for_you(&self, user_id: u64, limit: usize) -> String {
        let cold = user_id == 7;
        let mut items: Vec<(u64, f64)> = self.product_ids().iter().map(|id| (*id, 0.0)).collect();
        if let Some(zset) = self.zsets.get("zset:hot") {
            for (id, score) in &mut items {
                *score = zset.get(&id.to_string()).copied().unwrap_or(0.0);
            }
        }
        items.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        let json: Vec<serde_json::Value> = items
            .into_iter()
            .take(limit)
            .map(|(id, score)| {
                let reasons = if cold {
                    serde_json::json!([{"type":"cold_start"}])
                } else {
                    serde_json::json!([{"type":"trending","factor":0.9}])
                };
                serde_json::json!({"product_id": id, "score": score, "reasons": reasons})
            })
            .collect();
        serde_json::to_string(&json).unwrap_or_else(|_| "[]".to_string())
    }

    fn fake_similar(&self, product_id: &str, limit: usize) -> String {
        let json: Vec<serde_json::Value> = self
            .product_ids()
            .into_iter()
            .filter(|id| id.to_string() != product_id)
            .take(limit)
            .map(|id| {
                serde_json::json!({
                    "product_id": id,
                    "score": 0.5,
                    "reasons": [{"type":"tag_match","tags":["自行车"]}],
                })
            })
            .collect();
        serde_json::to_string(&json).unwrap_or_else(|_| "[]".to_string())
    }

    fn fake_hot(&self, limit: usize) -> String {
        let json: Vec<serde_json::Value> = self
            .product_ids()
            .into_iter()
            .take(limit)
            .map(|id| {
                serde_json::json!({
                    "product_id": id,
                    "score": 0.98,
                    "reasons": [{"type":"trending","factor":1.0}],
                })
            })
            .collect();
        serde_json::to_string(&json).unwrap_or_else(|_| "[]".to_string())
    }

    fn fake_rec_stats(&self) -> String {
        let products = self.product_ids().len();
        let users = self
            .hashes
            .keys()
            .filter(|k| k.starts_with("user:") && !k.ends_with(":events"))
            .count();
        serde_json::json!({
            "products": products,
            "users": users,
            "tags": self.sets.keys().filter(|k| k.starts_with("tag:")).count(),
            "co_occurrence_pairs": 12,
            "events": 30,
        })
        .to_string()
    }

    fn info_text(&self) -> String {
        let hits = self.total_commands * 9;
        let misses = self.total_commands.max(1);
        let mut out = String::new();
        out.push_str("# Server\n");
        out.push_str("mredis_version:0.1.0\n");
        out.push_str("uptime_in_seconds:120\n");
        out.push_str("# Stats\n");
        out.push_str(&format!(
            "total_commands_processed:{}\n",
            self.total_commands
        ));
        out.push_str(&format!("keyspace_hits:{hits}\n"));
        out.push_str(&format!("keyspace_misses:{misses}\n"));
        out.push_str("# Keyspace\n");
        out.push_str(&format!("db0:keys={},expires=0\n", self.key_count()));
        out.push_str("# Dict\n");
        out.push_str("dict_buckets:16\n");
        out.push_str("dict_rehashing:0\n");
        out.push_str("dict_distribution:0,1,0,2,3\n");
        out.push_str("# Rec\n");
        out.push_str(&format!("rec_products:{}\n", self.product_ids().len()));
        out.push_str("# AOF\n");
        out.push_str("aof_enabled:1\n");
        out.push_str("aof_size:4096\n");
        out.push_str("aof_cmds:88\n");
        out.push_str("aof_policy:everysec\n");
        out.push_str("# Recent\n");
        for line in self.recent.iter().take(50) {
            out.push_str(&format!("recent_cmd:{line}\n"));
        }
        out
    }
}

/// 极简 glob：只支持 `*`。
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.split_once('*') {
        None => pattern == text,
        Some((prefix, suffix)) => {
            text.len() >= prefix.len() + suffix.len()
                && text.starts_with(prefix)
                && text.ends_with(suffix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("product:*", "product:12"));
        assert!(!glob_match("product:*", "user:1"));
        assert!(glob_match("user:*:events", "user:3:events"));
        assert!(!glob_match("user:*:events", "user:3"));
    }

    #[test]
    fn fake_backend_serves_hgetall() {
        let fake = FakeBackend::new().with_product(1, "车", 100, &["自行车"], 10, 0.5);
        let reply = fake.call(&["HGETALL", "product:1"]).expect("应成功");
        match reply {
            Resp::Array(items) => {
                let flat: Vec<String> = items.iter().filter_map(|v| v.as_string()).collect();
                assert!(flat.iter().any(|v| v == "车"));
            }
            other => panic!("应为数组，实际 {other:?}"),
        }
    }

    #[test]
    fn fake_backend_tracks_events() {
        let fake = FakeBackend::new().with_product(1, "车", 100, &[], 10, 0.0);
        let reply = fake.call(&["REC.TRACK", "1", "1", "fav"]).expect("应成功");
        assert_eq!(reply, Resp::Int(1));
        let reply = fake.call(&["HGETALL", "product:1"]).expect("应成功");
        let Resp::Array(items) = reply else {
            panic!("应为数组")
        };
        let flat: Vec<String> = items.iter().filter_map(|v| v.as_string()).collect();
        let pos = flat.iter().position(|v| v == "favs").expect("应有 favs");
        assert_eq!(flat[pos + 1], "1");
    }

    #[test]
    fn fake_backend_reports_down() {
        let fake = FakeBackend::down();
        assert!(fake.call(&["PING"]).is_err());
    }

    #[test]
    fn fake_backend_unknown_command_errors() {
        let fake = FakeBackend::new();
        let reply = fake.call(&["NOPE"]).expect("应返回回包");
        assert!(reply.is_error());
    }
}
