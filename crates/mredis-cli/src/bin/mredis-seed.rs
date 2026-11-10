//! `mredis-seed` —— 把 `data/seed.json` 导入运行中的 mini-redis 服务端。
//!
//! 写入的键严格遵循 `docs/SPEC.md` 5.1：
//!
//! | 键 | 类型 | 内容 |
//! |---|---|---|
//! | `product:<id>` | HASH | id/title/desc/price_cents/tags/seller_id/condition/campus/created_at/status/views/clicks/favs |
//! | `user:<id>` | HASH | id/name/campus/interests/avg_price |
//! | `tag:<tag>` | SET | 带该标签的商品 id |
//! | `user:<id>:events` | LIST | JSON `{"product_id":1,"kind":"view","ts":1700000000}`（最新在前，最多 200 条） |
//! | `zset:hot` | ZSET | 商品 id → 热度分 |
//! | `product:index:created` | ZSET | 商品 id → created_at |
//! | `meta:next_product_id` / `meta:next_user_id` | STRING | 自增 id |
//!
//! 行为事件会先聚合成 `views`/`clicks`/`favs` 三个计数再写进商品 HASH，
//! 同时按 `campus_rec::hot_score` 的重力衰减公式算出 `zset:hot` 的分数。

use campus_rec::model::{Event, Product, User};
use mredis_client::Client;
use mredis_proto::Resp;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::time::Instant;

/// 默认服务端地址。
const DEFAULT_ADDR: &str = "127.0.0.1:6380";
/// 默认种子文件路径（相对当前目录）。
const DEFAULT_SEED: &str = "data/seed.json";
/// 每个 pipeline 批次的命令条数。
const BATCH: usize = 200;
/// 每处理多少条记录打印一次进度。
const PROGRESS_EVERY: usize = 50;
/// 单个用户的最近行为列表保留条数。
const EVENT_LIST_CAP: usize = 200;
/// 版本号。
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `--help` 文本。
const HELP_TEXT: &str = "\
用法: mredis-seed [选项]

把 data/seed.json 导入运行中的 mini-redis 服务端。

选项:
      --seed <PATH>   种子文件路径（默认 data/seed.json）
      --addr <ADDR>   服务端地址（默认 127.0.0.1:6380）
      --flush         导入前先 FLUSHDB 清空数据库
      --help          显示本帮助并退出
      --version       显示版本号并退出
";

/// 种子文件内容。
#[derive(Debug, Default)]
struct Seed {
    users: Vec<User>,
    products: Vec<Product>,
    events: Vec<Event>,
}

/// 从 `seed.json` 的原始 JSON 反序列化。
fn parse_seed(text: &str) -> Result<Seed, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("种子文件不是合法 JSON: {e}"))?;
    let take = |key: &str| -> serde_json::Value {
        value
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new()))
    };
    let users: Vec<User> =
        serde_json::from_value(take("users")).map_err(|e| format!("users 字段解析失败: {e}"))?;
    let products: Vec<Product> = serde_json::from_value(take("products"))
        .map_err(|e| format!("products 字段解析失败: {e}"))?;
    let events: Vec<Event> =
        serde_json::from_value(take("events")).map_err(|e| format!("events 字段解析失败: {e}"))?;
    Ok(Seed {
        users,
        products,
        events,
    })
}

/// 命令行参数。
struct Options {
    seed_path: String,
    addr: String,
    flush: bool,
}

enum Parsed {
    Run(Options),
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut opts = Options {
        seed_path: DEFAULT_SEED.to_string(),
        addr: DEFAULT_ADDR.to_string(),
        flush: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" => return Ok(Parsed::Help),
            "--version" => return Ok(Parsed::Version),
            "--flush" => opts.flush = true,
            "--seed" => {
                i += 1;
                opts.seed_path = args
                    .get(i)
                    .ok_or_else(|| "--seed 需要一个路径参数".to_string())?
                    .clone();
            }
            "--addr" => {
                i += 1;
                opts.addr = args
                    .get(i)
                    .ok_or_else(|| "--addr 需要一个地址参数".to_string())?
                    .clone();
            }
            other => return Err(format!("未知选项: {other}")),
        }
        i += 1;
    }
    Ok(Parsed::Run(opts))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse_args(&args) {
        Ok(Parsed::Help) => {
            print!("{HELP_TEXT}");
            0
        }
        Ok(Parsed::Version) => {
            println!("mredis-seed {VERSION}");
            0
        }
        Ok(Parsed::Run(opts)) => run(&opts),
        Err(msg) => {
            eprintln!("mredis-seed: {msg}");
            eprintln!("试试 'mredis-seed --help'。");
            2
        }
    };
    std::process::exit(code);
}

fn run(opts: &Options) -> i32 {
    let started = Instant::now();
    let text = match std::fs::read_to_string(&opts.seed_path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("读取种子文件 {} 失败: {e}", opts.seed_path);
            return 1;
        }
    };
    let seed = match parse_seed(&text) {
        Ok(seed) => seed,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!(
        "已读取 {}：{} 个用户 / {} 个商品 / {} 条行为事件",
        opts.seed_path,
        seed.users.len(),
        seed.products.len(),
        seed.events.len()
    );

    let mut client = match Client::connect(&opts.addr) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("无法连接到 {}: {e}", opts.addr);
            return 1;
        }
    };

    if opts.flush {
        match client.call(&["FLUSHDB"]) {
            Ok(resp) if resp.is_error() => {
                eprintln!("FLUSHDB 失败: {}", resp.as_string().unwrap_or_default());
                return 1;
            }
            Ok(_) => println!("已执行 FLUSHDB，数据库已清空。"),
            Err(e) => {
                eprintln!("FLUSHDB 失败: {e}");
                return 1;
            }
        }
    }

    let now = current_unix_secs();
    let (next_pid, next_uid) = next_ids(&seed);
    let plan = Plan::build(&seed, now, next_pid, next_uid, campus_rec::hot::hot_score);

    match execute(&mut client, &plan) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("导入失败: {e}");
            return 1;
        }
    }

    let elapsed = started.elapsed();
    println!(
        "导入完成：写入 {} 个键 / {} 条命令，耗时 {:.3}s",
        plan.key_count,
        plan.commands.len(),
        elapsed.as_secs_f64()
    );
    println!("自增 id：meta:next_product_id={next_pid}，meta:next_user_id={next_uid}");
    0
}

/// 逐批下发 pipeline，并打印进度。
fn execute(client: &mut Client, plan: &Plan) -> Result<(), String> {
    let total = plan.commands.len();
    let mut done = 0usize;
    let mut records = 0usize;
    while done < total {
        let end = (done + BATCH).min(total);
        let batch = &plan.commands[done..end];
        let replies = client
            .pipeline(batch)
            .map_err(|e| format!("第 {} 条命令起的一批失败: {e}", done + 1))?;
        for (offset, reply) in replies.iter().enumerate() {
            if reply.is_error() {
                let text = reply.as_string().unwrap_or_else(|| "(空错误)".to_string());
                return Err(format!(
                    "第 {} 条命令被服务端拒绝: {text}",
                    done + offset + 1
                ));
            }
        }
        done = end;
        records += batch.len();
        if records / PROGRESS_EVERY > (records - batch.len()) / PROGRESS_EVERY {
            print!("\r进度: {records}/{total} 条命令");
            let _ = io::stdout().flush();
        }
    }
    if total > 0 {
        println!("\r进度: {total}/{total} 条命令");
    }
    Ok(())
}

/// 当前 Unix 秒；系统时间早于纪元时退化为 0。
fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 计算自增 id 的起点（max + 1，空数据从 1 开始）。
fn next_ids(seed: &Seed) -> (u64, u64) {
    let next_pid = seed.products.iter().map(|p| p.id).max().unwrap_or(0) + 1;
    let next_uid = seed.users.iter().map(|u| u.id).max().unwrap_or(0) + 1;
    (next_pid, next_uid)
}

/// 待下发的命令计划。
struct Plan {
    commands: Vec<Vec<Vec<u8>>>,
    /// 去重后的键数量。
    key_count: usize,
}

impl Plan {
    /// 生成全部写入命令。
    ///
    /// `score` 是热度打分函数，生产环境传 `campus_rec::hot::hot_score`；
    /// 单测里传一个纯计数的闭包，避免依赖算法 crate 的运行时实现。
    fn build<F>(seed: &Seed, now: i64, next_pid: u64, next_uid: u64, score: F) -> Plan
    where
        F: Fn(&Product, i64) -> f64,
    {
        let mut commands: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut keys: BTreeSet<String> = BTreeSet::new();

        // 1. 行为事件先聚合，得到每个商品的 views/clicks/favs 与每用户的最近事件
        let mut counters: BTreeMap<u64, (u64, u64, u64)> = BTreeMap::new();
        let mut user_events: BTreeMap<u64, Vec<Event>> = BTreeMap::new();
        for event in &seed.events {
            let entry = counters.entry(event.product_id).or_insert((0, 0, 0));
            match event.kind {
                campus_rec::model::EventKind::View => entry.0 += 1,
                campus_rec::model::EventKind::Click => entry.1 += 1,
                campus_rec::model::EventKind::Fav => entry.2 += 1,
            }
            user_events.entry(event.user_id).or_default().push(*event);
        }

        // 2. 用户 HASH
        for user in &seed.users {
            let key = format!("user:{}", user.id);
            keys.insert(key.clone());
            let interests = user.interests.join(",");
            commands.push(cmd([
                "HSET",
                key.as_str(),
                "id",
                user.id.to_string().as_str(),
                "name",
                user.name.as_str(),
                "campus",
                user.campus.as_str(),
                "interests",
                interests.as_str(),
                "avg_price",
                user.avg_price.to_string().as_str(),
            ]));
        }

        // 3. 商品 HASH（带聚合后的计数）
        for product in &seed.products {
            let key = format!("product:{}", product.id);
            keys.insert(key.clone());
            let (views, clicks, favs) = counters.get(&product.id).copied().unwrap_or((0, 0, 0));
            let tags = product.tags.join(",");
            let price = product.price_cents.to_string();
            let seller = product.seller_id.to_string();
            let condition = product.condition.to_string();
            let created = product.created_at.to_string();
            let views = views.to_string();
            let clicks = clicks.to_string();
            let favs = favs.to_string();
            commands.push(cmd([
                "HSET",
                key.as_str(),
                "id",
                product.id.to_string().as_str(),
                "title",
                product.title.as_str(),
                "desc",
                product.desc.as_str(),
                "price_cents",
                price.as_str(),
                "tags",
                tags.as_str(),
                "seller_id",
                seller.as_str(),
                "condition",
                condition.as_str(),
                "campus",
                product.campus.as_str(),
                "created_at",
                created.as_str(),
                "status",
                product.status.as_str(),
                "views",
                views.as_str(),
                "clicks",
                clicks.as_str(),
                "favs",
                favs.as_str(),
            ]));
        }

        // 4. 标签倒排：tag:<tag> → 商品 id 集合
        let mut tag_sets: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for product in &seed.products {
            for tag in &product.tags {
                let tag = tag.trim();
                if tag.is_empty() {
                    continue;
                }
                tag_sets
                    .entry(tag.to_string())
                    .or_default()
                    .push(product.id);
            }
        }
        for (tag, ids) in &tag_sets {
            let key = format!("tag:{tag}");
            keys.insert(key.clone());
            let mut parts = vec!["SADD".to_string(), key];
            parts.extend(ids.iter().map(|id| id.to_string()));
            commands.push(cmd(parts.iter().map(|s| s.as_str())));
        }

        // 5. 用户行为列表：按 ts 升序 LPUSH，最新自然落在表头
        for (user_id, mut events) in user_events {
            let key = format!("user:{user_id}:events");
            keys.insert(key.clone());
            events.sort_by_key(|e| e.ts);
            let mut parts = vec!["LPUSH".to_string(), key];
            for event in &events {
                parts.push(event_json(event));
            }
            commands.push(cmd(parts.iter().map(|s| s.as_str())));
            commands.push(cmd([
                "LTRIM",
                format!("user:{user_id}:events").as_str(),
                "0",
                EVENT_LIST_CAP.saturating_sub(1).to_string().as_str(),
            ]));
        }

        // 6. zset:hot 与 product:index:created
        for product in &seed.products {
            let hot = score(product, now);
            commands.push(cmd([
                "ZADD",
                "zset:hot",
                format!("{hot:.6}").as_str(),
                product.id.to_string().as_str(),
            ]));
            commands.push(cmd([
                "ZADD",
                "product:index:created",
                product.created_at.to_string().as_str(),
                product.id.to_string().as_str(),
            ]));
        }
        keys.insert("zset:hot".to_string());
        keys.insert("product:index:created".to_string());

        // 7. 自增 id
        keys.insert("meta:next_product_id".to_string());
        keys.insert("meta:next_user_id".to_string());
        commands.push(cmd([
            "SET",
            "meta:next_product_id",
            next_pid.to_string().as_str(),
        ]));
        commands.push(cmd([
            "SET",
            "meta:next_user_id",
            next_uid.to_string().as_str(),
        ]));

        Plan {
            commands,
            key_count: keys.len(),
        }
    }
}

/// 事件列表里存的 JSON。
fn event_json(event: &Event) -> String {
    serde_json::json!({
        "product_id": event.product_id,
        "kind": event.kind.as_str(),
        "ts": event.ts,
    })
    .to_string()
}

/// 把一组字符串拼成一条命令。
fn cmd<I, S>(parts: I) -> Vec<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    parts
        .into_iter()
        .map(|s| s.as_ref().as_bytes().to_vec())
        .collect()
}

/// 仅供 `execute` 判断回包类型用。
#[allow(dead_code)]
fn is_ok(reply: &Resp) -> bool {
    !reply.is_error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use campus_rec::model::{EventKind, Status};

    fn sample() -> Seed {
        Seed {
            users: vec![User {
                id: 1,
                name: "张三".into(),
                campus: "前湖校区".into(),
                interests: vec!["自行车".into(), "运动".into()],
                avg_price: 45000,
            }],
            products: vec![
                Product {
                    id: 7,
                    title: "捷安特山地车".into(),
                    desc: "九成新".into(),
                    price_cents: 45000,
                    tags: vec!["自行车".into(), "运动".into()],
                    seller_id: 1,
                    condition: 9,
                    campus: "前湖校区".into(),
                    created_at: 1_700_000_000,
                    status: Status::OnSale,
                    views: 0,
                    clicks: 0,
                    favs: 0,
                },
                Product {
                    id: 9,
                    title: "高数教材".into(),
                    desc: String::new(),
                    price_cents: 1500,
                    tags: vec!["教材".into()],
                    seller_id: 1,
                    condition: 7,
                    campus: "东湖校区".into(),
                    created_at: 1_700_000_500,
                    status: Status::OnSale,
                    views: 0,
                    clicks: 0,
                    favs: 0,
                },
            ],
            events: vec![
                Event {
                    user_id: 1,
                    product_id: 7,
                    kind: EventKind::View,
                    ts: 1_700_000_100,
                },
                Event {
                    user_id: 1,
                    product_id: 7,
                    kind: EventKind::Fav,
                    ts: 1_700_000_200,
                },
                Event {
                    user_id: 1,
                    product_id: 9,
                    kind: EventKind::Click,
                    ts: 1_700_000_300,
                },
            ],
        }
    }

    /// 用加权计数代替热度分，单测不依赖算法 crate 的运行时实现。
    fn counter_score(product: &Product, _now: i64) -> f64 {
        (product.views + 3 * product.clicks + 5 * product.favs) as f64
    }

    fn flat(command: &[Vec<u8>]) -> Vec<String> {
        command
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }

    #[test]
    fn parse_seed_reads_three_sections() {
        let text = r#"{"users":[{"id":1,"name":"张三"}],
                       "products":[{"id":1,"title":"车","price_cents":100,"seller_id":1}],
                       "events":[{"user_id":1,"product_id":1,"kind":"view","ts":5}]}"#;
        let seed = parse_seed(text).expect("应解析成功");
        assert_eq!(seed.users.len(), 1);
        assert_eq!(seed.products.len(), 1);
        assert_eq!(seed.events.len(), 1);
        assert_eq!(seed.events[0].kind, EventKind::View);
    }

    #[test]
    fn parse_seed_rejects_broken_json() {
        assert!(parse_seed("{not json").is_err());
        // 字段类型不对也要报错
        assert!(parse_seed(r#"{"users":[{"id":"abc"}]}"#).is_err());
    }

    #[test]
    fn next_ids_start_after_max() {
        let (pid, uid) = next_ids(&sample());
        assert_eq!(pid, 10);
        assert_eq!(uid, 2);
        let empty = Seed::default();
        assert_eq!(next_ids(&empty), (1, 1));
    }

    #[test]
    fn plan_writes_products_with_aggregated_counts() {
        let seed = sample();
        let plan = Plan::build(&seed, 1_700_001_000, 10, 2, counter_score);
        let product = plan
            .commands
            .iter()
            .map(|c| flat(c))
            .find(|c| c[0] == "HSET" && c[1] == "product:7")
            .expect("应写入 product:7");
        let pos = |field: &str| product.iter().position(|v| v == field).expect("字段应存在");
        assert_eq!(product[pos("views") + 1], "1");
        assert_eq!(product[pos("clicks") + 1], "0");
        assert_eq!(product[pos("favs") + 1], "1");
        assert_eq!(product[pos("tags") + 1], "自行车,运动");
        assert_eq!(product[pos("status") + 1], "onsale");
    }

    #[test]
    fn plan_builds_tag_sets_and_meta() {
        let seed = sample();
        let plan = Plan::build(&seed, 1_700_001_000, 10, 2, counter_score);
        let all: Vec<Vec<String>> = plan.commands.iter().map(|c| flat(c)).collect();
        let tag_bike = all
            .iter()
            .find(|c| c[0] == "SADD" && c[1] == "tag:自行车")
            .expect("应有 tag:自行车");
        assert_eq!(tag_bike[2], "7");
        let meta = all
            .iter()
            .find(|c| c[0] == "SET" && c[1] == "meta:next_product_id")
            .expect("应写 meta:next_product_id");
        assert_eq!(meta[2], "10");
    }

    #[test]
    fn plan_writes_events_and_hot_zset() {
        let seed = sample();
        let plan = Plan::build(&seed, 1_700_001_000, 10, 2, counter_score);
        let all: Vec<Vec<String>> = plan.commands.iter().map(|c| flat(c)).collect();
        let events = all
            .iter()
            .find(|c| c[0] == "LPUSH" && c[1] == "user:1:events")
            .expect("应写事件列表");
        // LPUSH key + 3 条事件（按 ts 升序推入，最后一条落在表头）
        assert_eq!(events.len(), 5);
        assert!(events[4].contains(r#""kind":"click""#));
        assert!(events[2].contains(r#""kind":"view""#));
        assert!(all.iter().any(|c| c[0] == "ZADD" && c[1] == "zset:hot"));
        assert!(
            all.iter()
                .any(|c| c[0] == "ZADD" && c[1] == "product:index:created")
        );
    }

    #[test]
    fn plan_counts_distinct_keys() {
        let seed = sample();
        let plan = Plan::build(&seed, 1_700_001_000, 10, 2, counter_score);
        // user:1, product:7, product:9, tag:自行车, tag:运动, tag:教材,
        // user:1:events, zset:hot, product:index:created, meta:*
        assert_eq!(plan.key_count, 11);
    }

    #[test]
    fn event_json_matches_spec() {
        let event = Event {
            user_id: 1,
            product_id: 12,
            kind: EventKind::Fav,
            ts: 1_700_000_000,
        };
        assert_eq!(
            event_json(&event),
            r#"{"kind":"fav","product_id":12,"ts":1700000000}"#
        );
    }

    #[test]
    fn parse_args_reads_overrides() {
        let args: Vec<String> = ["--seed", "x.json", "--addr", "1.2.3.4:5", "--flush"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_args(&args).expect("应解析成功") {
            Parsed::Run(opts) => {
                assert_eq!(opts.seed_path, "x.json");
                assert_eq!(opts.addr, "1.2.3.4:5");
                assert!(opts.flush);
            }
            _ => panic!("应为 Run"),
        }
    }

    #[test]
    fn parse_args_rejects_unknown() {
        assert!(parse_args(&["--wat".to_string()]).is_err());
        assert!(parse_args(&["--seed".to_string()]).is_err());
    }
}
