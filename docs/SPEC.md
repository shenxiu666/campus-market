# 接口契约与分工 SPEC

> 本文件是所有并行工作线的**唯一契约来源**。任何签名改动必须先改本文件并通知 Lead。

## 0. 目录与归属

| 路径 | 归属 | 说明 |
|---|---|---|
| `crates/mredis-proto/**` | Lead（已完成） | RESP2 编解码，测试全绿 |
| `crates/mredis-client/**` | Lead（已完成） | RESP 客户端，测试全绿 |
| `crates/mredis-core/**` | **core-store** | 存储层 + 命令层 + AOF + 服务端 |
| `crates/campus-rec/**` | **rec-engine** | 排序/哈希/推荐算法库 |
| `docs/ALGORITHMS.md` | **rec-engine** | 算法设计文档 |
| `crates/mredis-cli/**` | **web-cli** | `mredis-cli` + `mredis-seed` |
| `crates/mredis-web/**`, `web/**` | **web-cli** | axum 网关 + 前端 |
| `data/seed.json`, `scripts/**`, `README.md` | Lead | 数据、脚本、文档 |
| 现有 `../redis/**` | 只读 | 参考实现，禁止修改 |

**禁止越界写他人目录**。需要对方改接口 → 发消息给 Lead 或直接给对方，不要自己动手。

## 1. 已完成并冻结的接口

### 1.1 `mredis-proto`

```rust
pub enum Resp { Simple(String), Error(String), Int(i64), Bulk(Vec<u8>), Null, Array(Vec<Resp>) }
impl Resp {
    pub fn ok() -> Resp;
    pub fn simple(s: impl Into<String>) -> Resp;
    pub fn error(s: impl Into<String>) -> Resp;
    pub fn bulk(s: impl Into<Vec<u8>>) -> Resp;
    pub fn as_string(&self) -> Option<String>;
    pub fn as_int(&self) -> Option<i64>;
    pub fn is_error(&self) -> bool;
}

pub enum ProtoError { Incomplete, Invalid(String) }

pub struct Decoder;
impl Decoder {
    pub fn new() -> Self;
    pub fn feed(&mut self, bytes: &[u8]);
    pub fn buffered(&self) -> usize;
    pub fn next_command(&mut self) -> Result<Option<Vec<Vec<u8>>>, ProtoError>;
}

pub fn encode(resp: &Resp, out: &mut Vec<u8>);
pub fn encode_command(args: &[Vec<u8>], out: &mut Vec<u8>);

pub const PROTO_INLINE_MAX_SIZE: usize = 64 * 1024;
pub const PROTO_MAX_BULK_LEN: usize = 512 * 1024 * 1024;
pub const PROTO_MAX_MULTIBULK_LEN: usize = 1024 * 1024;
```

`Decoder::next_command` 已支持**跨 feed 的半帧续传**（状态机内部保存），core-store 直接循环 `feed` + `next_command` 即可。

### 1.2 `mredis-client`

```rust
pub enum ClientError { Io(io::Error), Proto(ProtoError), Server(String) }
pub struct Client;
impl Client {
    pub fn connect(addr: &str) -> Result<Client, ClientError>;
    pub fn call(&mut self, args: &[&str]) -> Result<Resp, ClientError>;
    pub fn call_bytes(&mut self, args: &[Vec<u8>]) -> Result<Resp, ClientError>;
    pub fn pipeline(&mut self, cmds: &[Vec<Vec<u8>>]) -> Result<Vec<Resp>, ClientError>;
    pub fn call_strings(&mut self, args: &[&str]) -> Result<Vec<String>, ClientError>;
    pub fn call_json(&mut self, args: &[&str]) -> Result<serde_json::Value, ClientError>;
    pub fn call_inline(&mut self, line: &str) -> Result<Resp, ClientError>;
}
pub fn to_strings(resp: &Resp) -> Vec<String>;
pub fn resp_to_json(resp: &Resp) -> serde_json::Value;
```

## 2. `mredis-core` 契约（core-store 负责实现）

### 2.1 必须保留的公开 API（其他 crate 依赖）

```rust
// lib.rs
pub mod aof, cmd_hash, cmd_keyspace, cmd_list, cmd_rec, cmd_set, cmd_string, cmd_zset,
           command, config, db, dict, expire, info, obj, server, stats, zset;
pub use db::{Database, ExecCtx};
pub use dict::Dict;
pub use obj::Obj;
pub use server::Server;
pub use stats::Stats;
pub use zset::ZSet;
pub const VERSION: &str;
pub mod errors { pub fn wrong_args(cmd:&str)->String; pub const WRONGTYPE:&str; pub const NOT_INT:&str; pub const NOT_FLOAT:&str; pub const SYNTAX:&str; }

// config.rs
pub enum FsyncPolicy { Always, EverySec, No }
pub struct Config { host, port, appendonly, aof_path, appendfsync, seed, flush_on_start, verbose }
pub enum ParseOutcome { Run(Config), Exit(String) }
impl Config { pub fn parse_args(args:&[String]) -> Result<ParseOutcome,String>; pub fn help() -> &'static str; }
impl Default for Config  // port 6380

// server.rs
pub struct Server { pub addr: SocketAddr, pub db: Arc<Mutex<Database>>, pub stats: Arc<Stats> }
impl Server {
    pub fn bind(config: &Config) -> io::Result<Server>;
    pub fn spawn_accept_loop(&self);
    pub fn run(config: Config) -> io::Result<()>;
}
```

**关键：`Server::bind` 后必须向 stdout 打印一行 `Ready to accept connections on port <实际端口>` 并 flush**，集成测试与脚本靠这一行取端口（支持 `--port 0`）。`Server::run` 里做同样的事。

### 2.2 `Database` 契约

```rust
pub struct Database {
    pub data: Dict<Obj>,
    pub expires: Dict<i64>,
    pub rec: campus_rec::Recommender,
    pub product_tags: HashMap<u64, Vec<String>>,
}
impl Database {
    pub fn new() -> Self;                    // 空 Recommender（空商品/用户/事件）
    pub fn get(&mut self, key:&[u8]) -> Option<&Obj>;
    pub fn get_mut(&mut self, key:&[u8]) -> Option<&mut Obj>;
    pub fn set(&mut self, key:Vec<u8>, value:Obj);
    pub fn remove(&mut self, key:&[u8]) -> bool;
    pub fn contains(&mut self, key:&[u8]) -> bool;
    pub fn dbsize(&mut self) -> usize;
    pub fn flush(&mut self);
    pub fn is_expired(&mut self, key:&[u8]) -> bool;
    pub fn active_expire_cycle(&mut self, now_ms:i64, samples:usize) -> usize;
    pub fn pttl(&mut self, key:&[u8]) -> i64;
}
```

### 2.3 `REC.*` 命令的输出格式（**web-cli 依赖，必须严格一致**）

所有 `REC.*` 返回值都是 **`Resp::Bulk` 承载的 JSON 字符串**（UTF-8），这样 CLI 可直接打印、Web 可直接 `serde_json::from_str`。

**`REC.FOR_YOU <user_id> [limit]`** → Bulk JSON 数组：
```json
[
  {
    "product_id": 12,
    "score": 0.8123,
    "reasons": [
      {"type":"tag_match","tags":["自行车","运动"]},
      {"type":"trending","factor":0.93},
      {"type":"price_fit"}
    ]
  }
]
```
理由类型枚举：`tag_match`（带 `tags`）、`similar_users`（带 `users`）、`campus_match`、`trending`（带 `factor`）、`price_fit`、`cold_start`。

**`REC.SIMILAR <product_id> [limit]`** → 同上结构（`reasons` 可为 `tag_match`/`similar_users`）。

**`REC.HOT [limit]`** → `[{"product_id":1,"score":0.98,"reasons":[{"type":"trending","factor":1.0}]}]`

**`REC.TRACK <user_id> <product_id> <view|click|fav>`** → `:1`（整数 1 表示成功）
- 同时：`HSET product:<pid> views/clicks/favs +1`、`LPUSH user:<uid>:events <json>` + `LTRIM 0 199`、`ZINCRBY zset:hot <增量> <pid>`、`rec.record_event(...)`

**`REC.EXPLAIN <product_id> <user_id>`** → Bulk JSON：
```json
{"product_id":2,"score":0.6001,
 "parts":{"tag_sim":0.6667,"cf_sim":1.0,"hot_norm":0.1127,
          "price_fit":0.7759,"campus_fit":0.0},
 "weights":{"tag":0.45,"cf":0.25,"hot":0.1,"price":0.05,"campus":0.15},
 "matched_tags":["自行车","运动"],"similar_users":[7,2,4]}
```

**`REC.STATS`** → Bulk JSON：`{"products":42,"users":6,"tags":37,"co_occurrence_pairs":128,"events":240}`

**`REC.UPSERT <product_id>`** → `+OK`
- 把 `product:<id>` 哈希的**当前内容**重新灌进推荐索引（`rec.upsert_product`），幂等。
- 存在的意义：Web 的 `POST /api/products` 只发 `HSET`/`SADD`/`ZADD`，这些命令不经过 `REC.*`，
  运行期新增的商品不会自动进 `campus-rec` 的倒排索引与共现表，于是永远进不了推荐结果。
  发布成功后由调用方补一条 `REC.UPSERT <id>` 即可。
- 商品不存在 → `-ERR product <id> not found`；哈希缺 `title` 字段 → `-ERR product <id> has no title`
  （`REC.TRACK` 只给 `product:<id>` 写 `views/clicks/favs` 三个计数器字段，
  这类「计数器壳」不是真商品，必须跳过，否则会把空商品塞进索引）。

**`REC.REBUILD`** → `+OK`
- 从键空间整体重建推荐索引（`recbuild::rebuild_rec_from_keyspace`），等价于重启时的第 4 步。
- 纯派生操作，**不写 AOF**（键空间才是事实来源）；用于 `DELETE` 之后清理已删商品，或手工修复索引。

**`REC.SEED`** 不需要（种子由服务端 `--seed <path>` 在持久化存储首次初始化时导入）。

### 2.4 命令表要求

`command.rs::commands()` 必须包含下列命令（名字大写，arity 含命令名本身）：

连接/服务：`PING`(1..2) `ECHO`(2) `SELECT`(2) `INFO`(1..2) `DBSIZE`(1) `FLUSHDB`(1) `COMMAND`(1..) `BGREWRITEAOF`(1) `SHUTDOWN`(1..2) `MULTI`(1) `EXEC`(1) `DISCARD`(1)
字符串/键：`SET`(3..) `GET`(2) `SETNX`(3) `MSET`(3..) `MGET`(2..) `INCR`(2) `DECR`(2) `INCRBY`(3) `DECRBY`(3) `APPEND`(3) `STRLEN`(2) `DEL`(2..) `EXISTS`(2..) `TYPE`(2) `EXPIRE`(3) `PEXPIRE`(3) `PEXPIREAT`(3) `TTL`(2) `PTTL`(2) `PERSIST`(2) `KEYS`(2) `SCAN`(2..)
哈希：`HSET`(4..) `HGET`(3) `HMSET`(4..) `HMGET`(3..) `HGETALL`(2) `HDEL`(3..) `HEXISTS`(3) `HINCRBY`(4) `HKEYS`(2) `HVALS`(2) `HLEN`(2)
列表：`LPUSH`(3..) `RPUSH`(3..) `LPOP`(2) `RPOP`(2) `LRANGE`(4) `LLEN`(2) `LTRIM`(4)
集合：`SADD`(3..) `SREM`(3..) `SMEMBERS`(2) `SISMEMBER`(3) `SCARD`(2) `SINTER`(3..) `SUNION`(3..) `SDIFF`(3..)
有序集合：`ZADD`(4..) `ZRANGE`(4..5) `ZREVRANGE`(4..5) `ZSCORE`(3) `ZRANK`(3) `ZREM`(3..) `ZCARD`(2) `ZINCRBY`(4)
领域：`REC.TRACK`(4) `REC.FOR_YOU`(2..3) `REC.SIMILAR`(2..3) `REC.HOT`(1..2) `REC.EXPLAIN`(3) `REC.STATS`(1) `REC.UPSERT`(2) `REC.REBUILD`(1)

`dispatch` 语义：
- 空命令（`*0`）→ 返回 `Resp::Null`，不报错
- 未知命令 → `-ERR unknown command '<name>'`
- arity 不符 → `-ERR wrong number of arguments for '<lowercase name>' command`
- 写命令成功 → `ctx.dirty = true`（server 据此写 AOF）
- `MULTI` 后的命令入队，`EXEC` 时顺序执行并返回 `Array`

## 3. `campus-rec` 契约（rec-engine 负责实现）

签名已冻结在 `crates/campus-rec/src/*.rs` 里（`todo!()` 占位）。要点：

- `Recommender::new(products, users, events)` 必须建立：`index`（倒排）、`co_occurrence`（共现）
- `recommend_for_user(user_id, limit, exclude, now)`：无该用户 → 返回热度榜且 `reasons=[ColdStart]`
- `query(...)`：先走倒排索引取候选，再过滤价格/关键字，再按 `SortKey` 排序
- `sort_products`：自研三路快排，**不许直接调 `sort_by`**
- `top_k`：二叉堆，**不许全排序后截断**
- 全部 `#[cfg(test)]` 单测必须覆盖：空集、单元素、全相同、逆序、随机 1000 组与标准库比对

## 4. `mredis-web` 契约（web-cli 负责实现）

### 4.1 路由（已固定在 `routes.rs`）

| 方法 | 路径 | 返回 |
|---|---|---|
| GET | `/` | HTML 单页 |
| GET | `/app.js` `/style.css` | 静态资源 |
| GET | `/api/health` | `{"ok":true,"redis":"127.0.0.1:6380","dbsize":42}` |
| GET | `/api/products?sort=&tags=&min=&max=&q=&limit=&offset=` | `{"total":42,"items":[ProductJson]}` |
| GET | `/api/products/{id}` | ProductJson |
| POST | `/api/products` | `{"id":43}`（201） |
| PATCH | `/api/products/{id}` | `{"ok":true,"product":ProductJson}`（部分字段） |
| DELETE | `/api/products/{id}` | `{"ok":true,"deleted":43}` |
| POST | `/api/products/{id}/event` | `{"ok":true,"product":ProductJson}` |
| GET | `/api/recommend?user_id=1&limit=10` | `{"user_id":1,"items":[RecommendItem]}` |
| GET | `/api/similar/{id}?limit=6` | `{"product_id":1,"items":[RecommendItem]}` |
| GET | `/api/hot?limit=10` | `{"items":[RecommendItem]}` |
| GET | `/api/users` | `{"items":[UserJson]}` |
| GET | `/api/stats` | 见 4.3 |
| GET | `/api/bench/sort?n=20000` | `{"n":20000,"results":[{"algorithm":"three_way_quick","ms":3.1},...]}` |

**ProductJson**（由 `HSET product:<id>` 的字段拼出）：
```json
{"id":12,"title":"捷安特山地车","desc":"...","price_cents":45000,"price":"450.00",
 "tags":["自行车","运动"],"seller_id":2,"condition":9,"campus":"前湖校区",
 "created_at":1700000000,"status":"onsale","views":12,"clicks":3,"favs":1,"hot":0.87}
```

**RecommendItem**：`{"product_id":12,"score":0.81,"reasons":[...],"product":ProductJson}`

**UserJson**：`{"id":1,"name":"张三","campus":"前湖校区","interests":["自行车"],"avg_price":30000}`

### 4.2 错误约定

- 参数非法 → 400 + `{"error":"...","field":"price"}`
- 商品不存在 → 404 + `{"error":"product not found"}`
- RESP 不可用 → 503 + `{"error":"storage unavailable"}`
- 所有响应 `content-type: application/json; charset=utf-8`

### 4.3 `/api/stats`

```json
{"dbsize":42,"keys":42,"dict":{"buckets":128,"rehashing":false,"distribution":[0,1,2,...]},
 "hit_rate":0.93,"total_commands":1024,"uptime_secs":120,
 "recent_commands":["HSET product:12 ...","REC.FOR_YOU 1 10"],
 "rec":{"products":42,"users":6,"tags":37,"co_occurrence_pairs":128},
 "aof":{"enabled":true,"size":4096,"commands":88}}
```
> `recent_commands` 与 `dict` 由服务端 `INFO` 提供；core-store 需在 `INFO` 里输出
> `dict_buckets:<n>`、`dict_rehashing:<0|1>`、`dict_distribution:<逗号分隔>`、
> `rec_products/rec_users/rec_tags/rec_cooccur`、`aof_enabled/aof_size/aof_cmds`。
> **INFO 输出约定见 5.2**。

### 4.4 前端要求

- 纯手写 HTML/CSS/JS，**零第三方库**（无 React/Vue/Chart.js）
- 4 个视图：市场、详情/发布、为你推荐、演示面板
- 演示面板：哈希桶分布用 `<canvas>` 手绘柱状图；最近命令流；排序算法耗时表
- 中文界面，响应式（窄屏单列）

## 5. 跨模块约定

### 5.1 键规范（写入方：`mredis-seed` / Web；读取方：core 的 `REC.*` 与 Web）

| 键 | 类型 | 字段 |
|---|---|---|
| `product:<id>` | HASH | `id title desc price_cents tags seller_id condition campus created_at status views clicks favs` |
| `user:<id>` | HASH | `id name campus interests avg_price` |
| `tag:<tag>` | SET | 商品 id |
| `user:<id>:events` | LIST | JSON `{"product_id":1,"kind":"view","ts":1700000000}` |
| `user:<id>:favs` | SET | 收藏的商品 id（权威值，不受 200 条事件窗口截断） |
| `zset:hot` | ZSET | 商品 id → 热度分 |
| `meta:next_product_id` / `meta:next_user_id` | STRING | 自增 |
| `product:index:created` | ZSET | 商品 id → created_at（「最新」排序用） |

`tags` 与 `interests` 字段为**英文逗号分隔**的字符串（如 `自行车,运动`），空串表示无标签。

### 5.2 `INFO` 输出（core-store 实现，web-cli 解析）

按 Redis 风格分节，`key:value` 每行一条：

```
# Server
mredis_version:0.1.0
uptime_in_seconds:120
# Clients
connected_clients:1
total_connections_received:5
# Stats
total_commands_processed:1024
keyspace_hits:900
keyspace_misses:70
# Keyspace
db0:keys=42,expires=0
# Dict
dict_buckets:128
dict_rehashing:0
dict_distribution:0,1,0,2,3,...
# Rec
rec_products:42
rec_users:6
rec_tags:37
rec_cooccur:128
rec_events:240
# AOF
aof_enabled:1
aof_size:4096
aof_cmds:88
aof_policy:everysec
# Recent
recent_cmd:HSET product:12 title ...
recent_cmd:REC.FOR_YOU 1 10
```

最近命令保留 50 条，**最新在前**（`recent_cmd:` 前缀，值内不得含换行）。

### 5.3 端口与启动

- RESP 默认 `127.0.0.1:6380`，`--port` 覆盖，`--port 0` 由系统分配并打印真实端口
- HTTP 默认 `127.0.0.1:8080`，`--http-port` 覆盖，`--redis-addr` 指定上游
- 两者都支持 `--help` / `--version`

### 5.4 服务端启动顺序（AOF + 种子的语义）

`Server::bind` 必须严格按下面四步执行，顺序不能换：

1. **`--flush-on-start`**：清空内存键空间，**并删除 `--aof-path` 指向的 AOF 文件
   与 `<aof>.ready` 初始化标记**（只清内存不删文件，下次启动 AOF 会把数据全带回来，等于没清）。
2. **AOF 读取与首次初始化**（`--appendonly yes` 时）：
   - 读取 AOF（末尾半帧要容错并截断到最后一个完整命令），得到命令列表 `cmds`。
   - `initialized = <aof>.ready 存在`；`needs_init = cmds.is_empty() || !initialized`。
   - `needs_init` 且有 `--seed`：导入种子，打印
     `Loaded seed: <n> products, <m> users, <k> events from <path>`，
     随后把整个键空间写成全量快照（`snapshot_commands` + `rewrite`），**最后**写入 `<aof>.ready`。
     崩溃在中间时下次重做（快照对每键先 `DEL`，幂等）。
   - 否则打印「已有持久化数据（N 条命令），跳过种子导入」。
3. **AOF 重放**：逐条执行 `cmds`。
   `--appendonly no` 时退化：每次启动都导入种子（纯内存演示模式）。
4. **`rebuild_rec_from_keyspace`**：把 `db.rec` 当成键空间的**纯派生视图**整体重建。

第 4 步的两个关键点：

- `Recommender::new` 内部会按 events 累加 `views/clicks/favs`，而哈希里这三个字段
  **已经是权威值**（种子和 `REC.TRACK` 都维护过），直接用真实计数构造会**翻倍**。
  做法：用计数清零的商品副本构造 `new`，再逐条 `upsert_product(真实计数版)` 覆盖。
- `db.rec` 不写进 AOF：它完全可以从键空间推导出来，写进去只会让 AOF 变成
  命令日志和派生数据的混合体，重放顺序稍有不慎就双重计算。
  同理，`BGREWRITEAOF` 的快照里也不含 `REC.*` 命令，重启后靠这一步重建。
  收藏用 `user:<uid>:favs` 权威集合恢复（`user:<uid>:events` 只保留最近 200 条）。

**写入与 fsync**：每条写命令 `Aof::append` 立即 `writer.flush()`（写穿到操作系统页缓存），
`appendfsync` 只决定 `fsync` 频率：`always` 每条同步、`everysec` 由后台线程每 200ms
检查一次（距上次 ≥1s 就 `sync_all`）、`no` 不同步。这样即使进程空闲时被强杀，
已确认的写命令也不会留在用户态缓冲区里丢失。

**`SHUTDOWN [SAVE|NOSAVE]`**：回复 `+OK` 后置停机标志，accept 循环退出；
除 `NOSAVE` 外，退出前再 `fsync` 一次。`scripts/stop.ps1` 优先走这条路径。

**`BGREWRITEAOF` 的快照语义**：重写出的 AOF 是**全量覆盖快照**，
所以除 `SET`（本身覆盖语义）外，每个键前面都必须先发一条 `DEL`。
`RPUSH` / `HSET` / `SADD` / `ZADD` 都是**合并语义**，而重放发生在已有数据之上，
不先 `DEL` 就会把 `user:<uid>:events` 这类列表追加两遍
（实测 76 条事件重放后变成 152 条）。

## 6. 完成定义（DoD）

每个工作线交付前必须自测：

```powershell
cargo fmt --all
cargo clippy -p <your-crate> --all-targets -- -D warnings
cargo test -p <your-crate>
```

- 不许留 `todo!()` / `unimplemented!()` / `dbg!()` / `println!` 调试残留（除服务端启动日志）
- 不许 `unwrap()` 处理用户输入（用 `match`/`ok_or`）
- 所有公开 API 带中文文档注释
- 单测覆盖：正常路径 + 至少 2 个边界/错误路径
