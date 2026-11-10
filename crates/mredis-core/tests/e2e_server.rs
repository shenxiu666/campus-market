//! 端到端测试：起**真实**的 `mredis-server` 进程，用 `mredis-client` 走 TCP 验证。
//!
//! 单元测试都在进程内直接调 `dispatch`，能覆盖逻辑但覆盖不到「协议编解码 +
//! TCP + 线程 + AOF 落盘 + 重启重放」这一整条链路。这里专门补上那一段：
//! 只有真起进程，`--port 0` 打印端口、`SIGINT`/EOF 刷 AOF 这些行为才被测到。
//!
//! 每个用例都自己挑一个临时目录放 AOF，互不干扰，也不碰仓库里的 `data/`。

use mredis_client::Client;
use mredis_proto::Resp;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 一个受管子进程：`Drop` 时确保被 kill，避免测试失败后留下孤儿进程占端口。
struct ServerProc {
    child: Child,
    port: u16,
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ServerProc {
    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn client(&self) -> Client {
        Client::connect(&self.addr()).expect("连接服务端")
    }
}

/// 测试用的独立目录（放在 workspace 的 `target/` 下，必然可写）。
fn work_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e")
        .join(tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建测试目录");
    dir
}

/// 启动服务端，从 stdout 读 `Ready to accept connections on port <n>`。
///
/// 端口必须从这行日志里拿：`--port 0` 时真实端口由内核分配，测试进程无从得知。
fn start(dir: &std::path::Path, extra: &[&str]) -> ServerProc {
    let exe = env!("CARGO_BIN_EXE_mredis-server");
    let aof = dir.join("appendonly.aof");
    let mut cmd = Command::new(exe);
    cmd.arg("--port")
        .arg("0")
        .arg("--aof-path")
        .arg(aof.to_string_lossy().as_ref());
    for a in extra {
        cmd.arg(a);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 mredis-server");

    let stdout = child.stdout.take().expect("stdout 管道");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    // 启动要读 AOF / 导入种子，最多等 10 秒。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut port = None;
    while Instant::now() < deadline {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(rest) = line
            .trim()
            .strip_prefix("Ready to accept connections on port ")
        {
            port = rest.trim().parse::<u16>().ok();
            break;
        }
    }
    let port = port.expect("未能从 stdout 解析出监听端口");
    // 继续抽干 stdout，否则子进程写满管道会阻塞。
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });
    ServerProc { child, port }
}

/// 等端口可连接。
fn wait_ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("端口 {port} 在 5 秒内未就绪");
}

fn s(resp: Resp) -> String {
    resp.as_string()
        .unwrap_or_else(|| format!("<非字符串: {resp:?}>"))
}

fn strs(resp: Resp) -> Vec<String> {
    mredis_client::to_strings(&resp)
}

/// 把 `REC.*` 的返回（Bulk 里装 JSON 文本）解析成 `serde_json::Value`。
///
/// 不能用 `resp_to_json`：它把 Bulk 当成普通字符串，不会二次解析 JSON。
fn json_of(resp: Resp) -> serde_json::Value {
    let text = resp
        .as_string()
        .unwrap_or_else(|| panic!("期望 Bulk 里的 JSON，得到 {resp:?}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("JSON 解析失败: {e}，原文: {text}"))
}

#[test]
fn set_and_get_over_tcp() {
    let dir = work_dir("set-get");
    let server = start(&dir, &[]);
    wait_ready(server.port);
    let mut c = server.client();

    assert_eq!(s(c.call(&["SET", "greeting", "hello"]).expect("SET")), "OK");
    assert_eq!(s(c.call(&["GET", "greeting"]).expect("GET")), "hello");
    // 不存在的键返回 Null（不是错误）
    assert_eq!(c.call(&["GET", "nope"]).expect("GET 缺失"), Resp::Null);
    // 覆盖写
    assert_eq!(s(c.call(&["SET", "greeting", "你好"]).expect("SET")), "OK");
    assert_eq!(s(c.call(&["GET", "greeting"]).expect("GET")), "你好");
    // 计数类命令
    assert_eq!(c.call(&["INCR", "n"]).expect("INCR"), Resp::Int(1));
    assert_eq!(
        c.call(&["INCRBY", "n", "9"]).expect("INCRBY"),
        Resp::Int(10)
    );
    assert_eq!(s(c.call(&["TYPE", "n"]).expect("TYPE")), "string");
    // 目前只有 greeting 和 n 两个键（GET 缺失的键不会创建它）。
    assert_eq!(c.call(&["DBSIZE"]).expect("DBSIZE"), Resp::Int(2));
}

#[test]
fn hash_roundtrip() {
    let dir = work_dir("hash");
    let server = start(&dir, &[]);
    wait_ready(server.port);
    let mut c = server.client();

    let added = c
        .call(&[
            "HSET",
            "product:1",
            "title",
            "捷安特山地车",
            "price_cents",
            "38800",
        ])
        .expect("HSET");
    assert_eq!(added, Resp::Int(2), "HSET 应返回新增字段数");

    assert_eq!(
        s(c.call(&["HGET", "product:1", "title"]).expect("HGET")),
        "捷安特山地车"
    );

    let all = strs(c.call(&["HGETALL", "product:1"]).expect("HGETALL"));
    assert_eq!(all.len(), 4, "HGETALL 应返回 field/value 交替的 4 个元素");
    assert!(all.contains(&"title".to_string()));
    assert!(all.contains(&"38800".to_string()));

    assert_eq!(c.call(&["HLEN", "product:1"]).expect("HLEN"), Resp::Int(2));
    assert_eq!(
        c.call(&["HEXISTS", "product:1", "title"]).expect("HEXISTS"),
        Resp::Int(1)
    );

    // 类型不符必须是 WRONGTYPE
    let bad = c.call(&["GET", "product:1"]).expect("GET 哈希");
    assert!(bad.is_error());
    assert!(
        bad.as_string().unwrap_or_default().starts_with("WRONGTYPE"),
        "类型错误文案不对: {bad:?}"
    );
}

#[test]
fn zset_ranking_and_range() {
    let dir = work_dir("zset");
    let server = start(&dir, &[]);
    wait_ready(server.port);
    let mut c = server.client();

    let added = c
        .call(&["ZADD", "zset:hot", "1.5", "a", "2.5", "b", "0.5", "c"])
        .expect("ZADD");
    assert_eq!(added, Resp::Int(3));

    // 按分数升序
    let ranged = strs(c.call(&["ZRANGE", "zset:hot", "0", "-1"]).expect("ZRANGE"));
    assert_eq!(ranged, vec!["c", "a", "b"]);

    // 带下标范围
    let mid = strs(
        c.call(&["ZRANGE", "zset:hot", "1", "2"])
            .expect("ZRANGE 区间"),
    );
    assert_eq!(mid, vec!["a", "b"]);

    // 逆序
    let rev = strs(
        c.call(&["ZREVRANGE", "zset:hot", "0", "-1"])
            .expect("ZREVRANGE"),
    );
    assert_eq!(rev, vec!["b", "a", "c"]);

    assert_eq!(
        s(c.call(&["ZSCORE", "zset:hot", "a"]).expect("ZSCORE")),
        "1.5"
    );
    assert_eq!(
        c.call(&["ZRANK", "zset:hot", "a"]).expect("ZRANK"),
        Resp::Int(1)
    );
    assert_eq!(c.call(&["ZCARD", "zset:hot"]).expect("ZCARD"), Resp::Int(3));

    // ZINCRBY 把 c 抬到最前
    assert_eq!(
        s(c.call(&["ZINCRBY", "zset:hot", "10", "c"])
            .expect("ZINCRBY")),
        "10.5"
    );
    let after = strs(c.call(&["ZRANGE", "zset:hot", "0", "-1"]).expect("ZRANGE"));
    assert_eq!(after, vec!["a", "b", "c"]);

    assert_eq!(
        c.call(&["ZREM", "zset:hot", "a"]).expect("ZREM"),
        Resp::Int(1)
    );
    assert_eq!(c.call(&["ZCARD", "zset:hot"]).expect("ZCARD"), Resp::Int(2));
}

#[test]
fn ttl_expires_key() {
    let dir = work_dir("ttl");
    let server = start(&dir, &[]);
    wait_ready(server.port);
    let mut c = server.client();

    assert_eq!(s(c.call(&["SET", "temp", "v"]).expect("SET")), "OK");
    // 没有 TTL 时返回 -1
    assert_eq!(c.call(&["TTL", "temp"]).expect("TTL"), Resp::Int(-1));
    assert_eq!(s(c.call(&["EXPIRE", "temp", "1"]).expect("EXPIRE")), "1");

    let ttl = c
        .call(&["TTL", "temp"])
        .expect("TTL")
        .as_int()
        .expect("整数");
    assert!((0..=1).contains(&ttl), "TTL 应为 0 或 1，实得 {ttl}");

    // 等它过期
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(c.call(&["GET", "temp"]).expect("过期后 GET"), Resp::Null);
    // 键不存在时 TTL 返回 -2
    assert_eq!(c.call(&["TTL", "temp"]).expect("TTL 缺失"), Resp::Int(-2));

    // PERSIST 能摘掉过期时间
    assert_eq!(s(c.call(&["SET", "keep", "v"]).expect("SET")), "OK");
    assert_eq!(
        c.call(&["PEXPIRE", "keep", "60000"]).expect("PEXPIRE"),
        Resp::Int(1)
    );
    assert_eq!(c.call(&["PERSIST", "keep"]).expect("PERSIST"), Resp::Int(1));
    assert_eq!(c.call(&["TTL", "keep"]).expect("TTL"), Resp::Int(-1));
}

#[test]
fn rec_track_then_for_you() {
    let dir = work_dir("rec");
    let seed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/seed.json");
    if !seed.exists() {
        eprintln!("跳过：缺少 data/seed.json");
        return;
    }
    let server = start(&dir, &["--seed", seed.to_string_lossy().as_ref()]);
    wait_ready(server.port);
    let mut c = server.client();

    // 种子导入后 REC.STATS 应反映真实规模（64 商品 / 8 用户 / 76 事件）。
    let stats_json = json_of(c.call(&["REC.STATS"]).expect("REC.STATS"));
    assert_eq!(stats_json["products"], 64);
    assert_eq!(stats_json["users"], 8);
    assert_eq!(stats_json["events"], 76);

    // 记录一次行为：返回整数 1
    assert_eq!(
        c.call(&["REC.TRACK", "1", "1", "fav"]).expect("REC.TRACK"),
        Resp::Int(1)
    );
    // 四份联动状态都应被更新
    let favs = s(c.call(&["HGET", "product:1", "favs"]).expect("HGET favs"));
    assert!(favs.parse::<i64>().expect("favs 应是整数") >= 1);
    // 种子本身已有用户 1 的历史事件，这里只断言「又长了一条」。
    let events_len = c
        .call(&["LLEN", "user:1:events"])
        .expect("LLEN")
        .as_int()
        .expect("整数");
    assert!(
        events_len >= 1,
        "user:1:events 应至少有 1 条，实得 {events_len}"
    );
    assert!(
        c.call(&["ZSCORE", "zset:hot", "1"])
            .expect("ZSCORE")
            .as_string()
            .is_some()
    );

    // 推荐结果：非空数组，且每项字段齐全
    let for_you = c.call(&["REC.FOR_YOU", "1", "5"]).expect("REC.FOR_YOU");
    let json = json_of(for_you);
    let arr = json.as_array().expect("REC.FOR_YOU 应返回数组");
    assert!(!arr.is_empty(), "有种子数据时推荐不应为空");
    assert!(arr.len() <= 5);
    for item in arr {
        assert!(item["product_id"].is_number(), "缺少 product_id: {item}");
        assert!(item["score"].is_number(), "缺少 score: {item}");
        assert!(item["reasons"].is_array(), "缺少 reasons: {item}");
        for r in item["reasons"].as_array().expect("reasons") {
            let ty = r["type"].as_str().expect("理由应有 type");
            assert!(
                [
                    "tag_match",
                    "similar_users",
                    "campus_match",
                    "trending",
                    "price_fit",
                    "cold_start"
                ]
                .contains(&ty),
                "未知理由类型: {ty}"
            );
        }
    }

    // 冷启动用户（seed 里用户 7 的 interests 为空）也要有结果
    let cold = json_of(c.call(&["REC.FOR_YOU", "7"]).expect("冷启动"));
    assert!(
        !cold.as_array().expect("数组").is_empty(),
        "冷启动应回落到热度榜"
    );

    // REC.HOT / REC.SIMILAR / REC.EXPLAIN / REC.STATS 的 JSON 结构
    assert!(json_of(c.call(&["REC.HOT", "3"]).expect("REC.HOT")).is_array());
    assert!(json_of(c.call(&["REC.SIMILAR", "1", "3"]).expect("REC.SIMILAR")).is_array());
    let explain = json_of(c.call(&["REC.EXPLAIN", "1", "1"]).expect("EXPLAIN"));
    if explain.is_object() {
        for k in [
            "product_id",
            "score",
            "parts",
            "weights",
            "matched_tags",
            "similar_users",
        ] {
            assert!(explain.get(k).is_some(), "REC.EXPLAIN 缺少字段 {k}");
        }
        for k in ["tag_sim", "cf_sim", "hot_norm", "price_fit"] {
            assert!(explain["parts"].get(k).is_some(), "parts 缺少 {k}");
        }
    }
}

#[test]
fn aof_survives_restart() {
    let dir = work_dir("aof-restart");
    // 第一轮：写入数据后杀掉进程（Drop 里 kill，AOF 已按 everysec/always 落盘）。
    {
        let server = start(&dir, &["--appendonly", "yes", "--appendfsync", "always"]);
        wait_ready(server.port);
        let mut c = server.client();
        assert_eq!(s(c.call(&["SET", "durable", "yes"]).expect("SET")), "OK");
        assert_eq!(
            s(c.call(&["RPUSH", "queue", "a", "b", "c"]).expect("RPUSH")),
            "3"
        );
        assert_eq!(s(c.call(&["HSET", "h", "f", "v"]).expect("HSET")), "1");
        // 显式发一次 BGREWRITEAOF，顺便验证重写后的文件依然可重放。
        let _ = c.call(&["BGREWRITEAOF"]);
    }

    let aof = dir.join("appendonly.aof");
    assert!(aof.exists(), "AOF 文件应已生成");
    assert!(
        std::fs::metadata(&aof).map(|m| m.len()).unwrap_or(0) > 0,
        "AOF 文件不应为空"
    );

    // 第二轮：同一个 AOF 路径重启，数据应完整回来。
    let server = start(&dir, &["--appendonly", "yes"]);
    wait_ready(server.port);
    let mut c = server.client();
    assert_eq!(s(c.call(&["GET", "durable"]).expect("GET")), "yes");
    assert_eq!(
        strs(c.call(&["LRANGE", "queue", "0", "-1"]).expect("LRANGE")),
        vec!["a", "b", "c"]
    );
    assert_eq!(s(c.call(&["HGET", "h", "f"]).expect("HGET")), "v");
}

/// 种子 + AOF 组合的回归测试。
///
/// 这是曾经的真实 bug：`Server::bind` 原本是「AOF 重放 → 若 dbsize()==0 才导种子」，
/// 而 AOF 重放先把键空间填成非空，于是 64 个种子商品被整个跳过，
/// 重启后 `DBSIZE` 从 110 掉到 7。修法：种子只在**首次初始化**时导入，
/// 导入后把整个键空间重写成快照并留下 `<aof>.ready` 标记；此后只重放增量。
///
/// 三轮覆盖三种加载路径：
/// * 第一轮：首次初始化（种子 → 写增量 → `REC.UPSERT` → `REC.TRACK` → 写快照）
/// * 第二轮：已有持久化数据（只重放 AOF）→ rebuild
/// * 第三轮：`BGREWRITEAOF` 后再来一遍（验证重写后的 AOF 也能重建 rec）
#[test]
fn seed_survives_aof_restart() {
    let dir = work_dir("seed-aof-restart");
    let seed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/seed.json");
    if !seed.exists() {
        eprintln!("跳过：缺少 data/seed.json");
        return;
    }
    let seed_arg = seed.to_string_lossy().into_owned();
    let base = [
        "--seed",
        seed_arg.as_str(),
        "--appendonly",
        "yes",
        "--appendfsync",
        "always",
    ];

    // ---- 第一轮 ----
    {
        let server = start(&dir, &base);
        wait_ready(server.port);
        let mut c = server.client();

        // 种子导入后 109 个键：64 商品 + 8 用户 + 25 标签 + zset:hot +
        // product:index:created + 8 个事件列表 + 2 个 meta 游标。
        assert_eq!(c.call(&["DBSIZE"]).expect("DBSIZE"), Resp::Int(109));
        let stats = json_of(c.call(&["REC.STATS"]).expect("REC.STATS"));
        assert_eq!(stats["products"], 64);
        assert_eq!(stats["users"], 8);
        assert_eq!(stats["events"], 76);

        // 模拟 Web 发布新商品：只发普通写命令 + 一次 REC.UPSERT。
        // 种子把游标设成 max(id)+1 = 65，所以 INCR 之后拿到 66。
        assert_eq!(
            c.call(&["INCR", "meta:next_product_id"]).expect("INCR"),
            Resp::Int(66)
        );
        c.call(&[
            "HSET",
            "product:66",
            "id",
            "66",
            "title",
            "新商品",
            "desc",
            "测试",
            "price_cents",
            "5000",
            "tags",
            "自行车",
            "seller_id",
            "1",
            "condition",
            "8",
            "campus",
            "前湖校区",
            "created_at",
            "1794083524",
            "status",
            "onsale",
            "views",
            "0",
            "clicks",
            "0",
            "favs",
            "0",
        ])
        .expect("HSET product:65");
        c.call(&["SADD", "tag:自行车", "66"]).expect("SADD");
        assert_eq!(s(c.call(&["REC.UPSERT", "66"]).expect("REC.UPSERT")), "OK");
        c.call(&["REC.TRACK", "1", "66", "view"])
            .expect("REC.TRACK");

        // 新商品立刻进索引（不必等重启）。
        let stats = json_of(c.call(&["REC.STATS"]).expect("REC.STATS"));
        assert_eq!(stats["products"], 65, "REC.UPSERT 后新商品应立即进索引");
        assert_eq!(stats["events"], 77);
        assert_eq!(c.call(&["DBSIZE"]).expect("DBSIZE"), Resp::Int(110));
    }

    // ---- 第二轮：同一 AOF 路径重启 ----
    let hot_norm_before;
    {
        let server = start(&dir, &base);
        wait_ready(server.port);
        let mut c = server.client();

        // 核心断言：不再掉到 7。
        assert_eq!(
            c.call(&["DBSIZE"]).expect("DBSIZE"),
            Resp::Int(110),
            "重启后种子数据不应丢失"
        );
        // 种子商品仍然可读。
        let p1 = strs(c.call(&["HGETALL", "product:1"]).expect("HGETALL"));
        assert!(!p1.is_empty(), "product:1 应仍存在");

        // rec 已由 rebuild 从键空间恢复（含运行期 upsert 的 65 号商品）。
        let stats = json_of(c.call(&["REC.STATS"]).expect("REC.STATS"));
        assert_eq!(
            stats["products"], 65,
            "rebuild 应恢复全部商品（64 种子 + 1 新增）"
        );
        assert_eq!(stats["users"], 8);
        assert_eq!(stats["events"], 77);

        // 推荐与解释都正常，且 hot_norm 不为 0（说明计数没被清零）。
        let for_you = json_of(c.call(&["REC.FOR_YOU", "1", "3"]).expect("REC.FOR_YOU"));
        let arr = for_you.as_array().expect("REC.FOR_YOU 应是数组");
        assert!(!arr.is_empty(), "重启后推荐不应为空");
        for item in arr {
            assert!(item["product_id"].is_number());
            assert!(item["score"].is_number());
            assert!(item["reasons"].is_array());
        }

        let explain = json_of(c.call(&["REC.EXPLAIN", "2", "1"]).expect("REC.EXPLAIN"));
        assert!(
            explain.is_object(),
            "REC.EXPLAIN 应返回对象，得到 {explain}"
        );
        hot_norm_before = explain["parts"]["hot_norm"]
            .as_f64()
            .expect("hot_norm 应是数字");
        assert!(
            hot_norm_before > 0.0,
            "重启后 hot_norm 应 > 0（计数没被清零），实得 {hot_norm_before}"
        );
    }

    // ---- 第三轮：BGREWRITEAOF 之后再重启 ----
    {
        let server = start(&dir, &base);
        wait_ready(server.port);
        let mut c = server.client();
        // BGREWRITEAOF 回的是 Redis 同款状态行，不是 +OK。
        assert!(
            s(c.call(&["BGREWRITEAOF"]).expect("BGREWRITEAOF")).contains("rewriting"),
            "BGREWRITEAOF 应回状态行"
        );
    }
    {
        let server = start(&dir, &base);
        wait_ready(server.port);
        let mut c = server.client();

        // 重写后的 AOF 只含键空间快照（没有 REC.TRACK / REC.UPSERT 命令），
        // 所以这一轮真正验证的是「rebuild 能从纯键空间恢复 rec」。
        let stats = json_of(c.call(&["REC.STATS"]).expect("REC.STATS"));
        assert_eq!(stats["products"], 65, "重写后的 AOF 也应能恢复全部商品");
        assert_eq!(stats["users"], 8);
        // 事件在 `user:<uid>:events` 里是带 ts 的 JSON，可重建；但重写后
        // 每条事件只剩列表里保留的那些（LTRIM 0 199 之内），数量应仍是 77。
        assert_eq!(stats["events"], 77);

        let for_you = json_of(c.call(&["REC.FOR_YOU", "1", "3"]).expect("REC.FOR_YOU"));
        assert!(!for_you.as_array().expect("数组").is_empty());

        let explain = json_of(c.call(&["REC.EXPLAIN", "2", "1"]).expect("REC.EXPLAIN"));
        let hot_norm_after = explain["parts"]["hot_norm"].as_f64().expect("hot_norm");
        // 计数是键空间里的权威值，重写前后必须逐位一致。
        assert!(
            (hot_norm_after - hot_norm_before).abs() < 1e-9,
            "hot_norm 在重写前后应一致：{hot_norm_before} vs {hot_norm_after}"
        );
    }
}

#[test]
fn multi_exec_is_atomic_and_returns_array() {
    let dir = work_dir("multi");
    let server = start(&dir, &[]);
    wait_ready(server.port);
    let mut c = server.client();

    assert_eq!(s(c.call(&["MULTI"]).expect("MULTI")), "OK");
    assert_eq!(s(c.call(&["SET", "a", "1"]).expect("SET 入队")), "QUEUED");
    assert_eq!(s(c.call(&["INCR", "a"]).expect("INCR 入队")), "QUEUED");
    let exec = c.call(&["EXEC"]).expect("EXEC");
    match exec {
        Resp::Array(items) => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].as_string().as_deref(), Some("OK"));
            assert_eq!(items[1], Resp::Int(2));
        }
        other => panic!("EXEC 应返回数组，得到 {other:?}"),
    }

    // DISCARD 丢弃队列
    assert_eq!(s(c.call(&["MULTI"]).expect("MULTI")), "OK");
    assert_eq!(s(c.call(&["SET", "b", "2"]).expect("SET 入队")), "QUEUED");
    assert_eq!(s(c.call(&["DISCARD"]).expect("DISCARD")), "OK");
    assert_eq!(c.call(&["EXISTS", "b"]).expect("EXISTS"), Resp::Int(0));
}

#[test]
fn errors_do_not_break_the_connection() {
    let dir = work_dir("errors");
    let server = start(&dir, &[]);
    wait_ready(server.port);
    let mut c = server.client();

    // 未知命令
    let unknown = c.call(&["NOSUCHCOMMAND", "x"]).expect("应有响应");
    assert!(unknown.is_error());
    assert!(
        unknown
            .as_string()
            .unwrap_or_default()
            .contains("unknown command"),
        "文案不对: {unknown:?}"
    );

    // arity 不符
    let arity = c.call(&["GET"]).expect("应有响应");
    assert!(arity.is_error());
    assert!(
        arity
            .as_string()
            .unwrap_or_default()
            .contains("wrong number of arguments"),
        "文案不对: {arity:?}"
    );

    // 连接仍然可用
    assert_eq!(s(c.call(&["PING"]).expect("PING")), "PONG");
    // PING 带消息时回显消息
    assert_eq!(s(c.call(&["PING", "hi"]).expect("PING msg")), "hi");
}

#[test]
fn info_has_all_sections_and_web_parseable_fields() {
    let dir = work_dir("info");
    let server = start(&dir, &[]);
    wait_ready(server.port);
    let mut c = server.client();

    let _ = c.call(&["SET", "k", "v"]);
    let text = s(c.call(&["INFO"]).expect("INFO"));

    for section in [
        "# Server",
        "# Clients",
        "# Stats",
        "# Keyspace",
        "# Dict",
        "# Rec",
        "# AOF",
        "# Recent",
    ] {
        assert!(text.contains(section), "INFO 缺少小节 {section}");
    }

    // 逐行 `key:value`，且 Web 端依赖的那几个字段存在。
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            fields.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    for key in [
        "mredis_version",
        "uptime_in_seconds",
        "connected_clients",
        "total_commands_processed",
        "keyspace_hits",
        "keyspace_misses",
        "dict_buckets",
        "dict_rehashing",
        "dict_distribution",
        "rec_products",
        "aof_enabled",
        "aof_policy",
    ] {
        assert!(fields.contains_key(key), "INFO 缺少字段 {key}");
    }
    // db0 用的是 `keys=..,expires=..` 的复合值
    let db0 = fields.get("db0").expect("缺少 db0");
    assert!(db0.starts_with("keys="), "db0 格式不对: {db0}");
    // dict_distribution 是逗号分隔的整数
    for n in fields["dict_distribution"].split(',') {
        assert!(n.parse::<u64>().is_ok(), "dict_distribution 含非整数: {n}");
    }
    // Recent 小节里应能看到刚才的命令
    assert!(text.contains("recent_cmd:"), "INFO 缺少 recent_cmd");
    assert!(
        text.contains("SET") || text.contains("PING") || text.contains("INFO"),
        "recent_cmd 应记录最近命令"
    );
}

#[test]
fn scan_and_keys_work() {
    let dir = work_dir("scan");
    let server = start(&dir, &[]);
    wait_ready(server.port);
    let mut c = server.client();

    for i in 0..50 {
        let _ = c.call(&["SET", &format!("key:{i}"), "v"]);
    }
    let keys = strs(c.call(&["KEYS", "key:*"]).expect("KEYS"));
    assert_eq!(keys.len(), 50);
    assert!(keys.iter().all(|k| k.starts_with("key:")));

    // SCAN 游标遍历：反复调用直到游标回到 0，收集全部键且不重复。
    let mut cursor = "0".to_string();
    let mut seen = std::collections::HashSet::new();
    let mut rounds = 0;
    loop {
        let reply = c.call(&["SCAN", &cursor, "COUNT", "7"]).expect("SCAN");
        let parts = match reply {
            Resp::Array(p) => p,
            other => panic!("SCAN 应返回数组，得到 {other:?}"),
        };
        assert_eq!(parts.len(), 2);
        cursor = parts[0].as_string().expect("游标应是字符串");
        for k in strs(parts[1].clone()) {
            assert!(seen.insert(k), "SCAN 返回了重复的键");
        }
        rounds += 1;
        assert!(rounds < 1000, "SCAN 游标未收敛");
        if cursor == "0" {
            break;
        }
    }
    assert_eq!(seen.len(), 50, "SCAN 应覆盖全部 50 个键且不重不漏");
}

#[test]
fn inline_command_and_pipeline() {
    let dir = work_dir("inline");
    let server = start(&dir, &[]);
    wait_ready(server.port);

    // inline 协议：一行纯文本也能当命令（对齐 redis-cli 的调试用法）。
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", server.port)).expect("连接");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.write_all(b"PING\r\n").expect("写 inline");
    stream.flush().ok();
    let mut reader = BufReader::new(stream.try_clone().expect("克隆"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("读回复");
    assert_eq!(line.trim(), "+PONG");

    // pipeline：一次发多条，按顺序收多条
    let mut c = server.client();
    let cmds = vec![
        vec![b"SET".to_vec(), b"p1".to_vec(), b"v1".to_vec()],
        vec![b"SET".to_vec(), b"p2".to_vec(), b"v2".to_vec()],
        vec![b"GET".to_vec(), b"p1".to_vec()],
    ];
    let replies = c.pipeline(&cmds).expect("pipeline");
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0].as_string().as_deref(), Some("OK"));
    assert_eq!(replies[2].as_string().as_deref(), Some("v1"));
}

/// **核心回归**：`everysec` 策略下，写入后立刻硬杀进程也不能丢数据。
///
/// 曾经的 bug：`Aof::append` 只把字节写进 `BufWriter`，而 `maybe_flush`
/// 只在「下一条命令到来」时才判断是否 fsync。Web 发完一次发布
/// （INCR + HSET + SADD + ZADD + REC.UPSERT）后就空闲，缓冲永远不落盘，
/// 强杀即丢——实测 AOF 只剩 41 字节的 INCR，重启后商品直接 404。
/// 现在的修法是 append 写穿到操作系统 + 后台线程每秒 fsync。
#[test]
fn everysec_writes_survive_immediate_hard_kill() {
    let dir = work_dir("everysec-hard-kill");
    {
        let server = start(&dir, &["--appendonly", "yes", "--appendfsync", "everysec"]);
        wait_ready(server.port);
        let mut c = server.client();
        // 一次发布 = 5 条命令，全塞进一个 pipeline：最容易踩到「缓冲不落盘」。
        let cmds = vec![
            vec![b"INCR".to_vec(), b"meta:next_product_id".to_vec()],
            vec![
                b"HSET".to_vec(),
                b"product:1".to_vec(),
                b"title".to_vec(),
                b"lamp".to_vec(),
            ],
            vec![b"SADD".to_vec(), b"tag:dorm".to_vec(), b"1".to_vec()],
            vec![
                b"ZADD".to_vec(),
                b"zset:hot".to_vec(),
                b"1.5".to_vec(),
                b"1".to_vec(),
            ],
            vec![b"REC.UPSERT".to_vec(), b"1".to_vec()],
        ];
        assert_eq!(c.pipeline(&cmds).expect("pipeline").len(), 5);
        // 关键：不 sleep、不发 SHUTDOWN，直接让 `server` 落出作用域，
        // Drop 里会 `kill()`（模拟关窗口 / 任务管理器结束进程）。
    }

    let server = start(&dir, &["--appendonly", "yes", "--appendfsync", "everysec"]);
    wait_ready(server.port);
    let mut c = server.client();
    assert_eq!(
        s(c.call(&["HGET", "product:1", "title"]).expect("HGET")),
        "lamp",
        "硬杀后商品标题应仍在"
    );
    assert_eq!(
        s(c.call(&["SISMEMBER", "tag:dorm", "1"]).expect("SISMEMBER")),
        "1",
        "硬杀后标签索引应仍在"
    );
    assert_eq!(
        s(c.call(&["GET", "meta:next_product_id"]).expect("GET")),
        "1",
        "硬杀后自增游标应仍在"
    );
}

/// 种子是**一次性**基线：删掉的种子商品不会在重启后复活。
///
/// 旧实现每次启动都重导种子，于是「下架/删除商品 → 重启 → 又回来了」。
/// 现在种子只在首次初始化时导入，之后 DEL 就是 DEL，写进 AOF 后永久生效。
#[test]
fn deleted_seed_product_stays_deleted_after_restart() {
    let dir = work_dir("delete-seed-persist");
    let seed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/seed.json");
    if !seed.exists() {
        eprintln!("跳过：缺少 data/seed.json");
        return;
    }
    let seed_arg = seed.to_string_lossy().into_owned();
    let base = [
        "--seed",
        seed_arg.as_str(),
        "--appendonly",
        "yes",
        "--appendfsync",
        "everysec",
    ];

    {
        let server = start(&dir, &base);
        wait_ready(server.port);
        let mut c = server.client();
        assert_eq!(
            c.call(&["EXISTS", "product:1"]).expect("EXISTS"),
            Resp::Int(1)
        );
        // 卖家撤单：删掉种子商品 1。
        assert_eq!(c.call(&["DEL", "product:1"]).expect("DEL"), Resp::Int(1));
    }

    let server = start(&dir, &base);
    wait_ready(server.port);
    let mut c = server.client();
    assert_eq!(
        c.call(&["EXISTS", "product:1"]).expect("EXISTS"),
        Resp::Int(0),
        "DEL 掉的种子商品重启后不应复活"
    );
    let stats = json_of(c.call(&["REC.STATS"]).expect("REC.STATS"));
    assert_eq!(stats["products"], 63, "推荐索引也应少掉这一件");
    assert_eq!(stats["users"], 8);
}

/// `SHUTDOWN` 应让服务端**自己**优雅退出并落盘，而不是等外部 kill。
#[test]
fn shutdown_command_stops_server_and_persists() {
    let dir = work_dir("shutdown");
    {
        let mut server = start(&dir, &["--appendonly", "yes", "--appendfsync", "everysec"]);
        wait_ready(server.port);
        let mut c = server.client();
        assert_eq!(s(c.call(&["SET", "k", "v"]).expect("SET")), "OK");
        assert_eq!(s(c.call(&["SHUTDOWN"]).expect("SHUTDOWN")), "OK");

        // 等进程自己退出（最多 5 秒）。
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while Instant::now() < deadline {
            if matches!(server.child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(exited, "SHUTDOWN 后进程应在 5 秒内退出");
    }

    let server = start(&dir, &["--appendonly", "yes"]);
    wait_ready(server.port);
    let mut c = server.client();
    assert_eq!(s(c.call(&["GET", "k"]).expect("GET")), "v");
}
