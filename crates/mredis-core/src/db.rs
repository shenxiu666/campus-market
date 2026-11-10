//! 键空间与命令执行上下文。
//!
//! `Database` 只负责「键 → 值」以及「键 → 过期时刻」两张表，类型检查与命令语义
//! 都在 `cmd_*` 里完成；`ExecCtx` 把「数据库 + 统计 + AOF + MULTI 状态」打包成
//! 一个执行上下文，命令处理器只依赖它。

use crate::aof::Aof;
use crate::dict::Dict;
use crate::errors;
use crate::expire::now_ms;
use crate::obj::Obj;
use crate::stats::Stats;
use campus_rec::Recommender;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// 单个数据库（本项目只用 db0）。
#[derive(Debug)]
pub struct Database {
    /// 键空间
    pub data: Dict<Obj>,
    /// 过期时间表：键 → 过期时刻（Unix 毫秒）
    pub expires: Dict<i64>,
    /// 推荐引擎（由 `REC.*` 命令与种子数据维护）
    pub rec: Recommender,
    /// 商品 id → 标签（删除商品时清理倒排索引用）
    pub product_tags: HashMap<u64, Vec<String>>,
}

impl Database {
    /// 新建空数据库。
    ///
    /// 推荐引擎用「空商品 / 空用户 / 空事件」构造：`Recommender::new` 内部会建好
    /// 空的倒排索引与共现矩阵，之后由 `--seed` 或 `REC.TRACK` 逐步填充。
    pub fn new() -> Self {
        Self {
            data: Dict::new(),
            expires: Dict::new(),
            rec: Recommender::new(Vec::new(), Vec::new(), &[]),
            product_tags: HashMap::new(),
        }
    }

    /// 惰性过期：键已过期则立刻删除并返回 `true`。
    ///
    /// Redis 用「惰性删除 + 主动扫描」两套机制：访问时先查过期表（这里），
    /// 后台再周期性抽样清理（[`Self::active_expire_cycle`]）。只做其中任一种都有
    /// 缺陷——只惰性删除会让过期键长期占内存，只主动删除则会漏掉刚过期的键。
    fn purge_if_expired(&mut self, key: &[u8]) -> bool {
        let Some(&deadline) = self.expires.get(key) else {
            return false;
        };
        if now_ms() < deadline {
            return false;
        }
        self.expires.remove(key);
        self.data.remove(key);
        true
    }

    /// 取键（已处理惰性过期）。
    pub fn get(&mut self, key: &[u8]) -> Option<&Obj> {
        if self.purge_if_expired(key) {
            return None;
        }
        self.data.get(key)
    }

    /// 取键的可变引用（已处理惰性过期）。
    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut Obj> {
        if self.purge_if_expired(key) {
            return None;
        }
        self.data.get_mut(key)
    }

    /// 写入键（不做类型检查，调用方负责）。
    ///
    /// 注意：这里**不**清理 TTL。`SET` 这类命令要显式调
    /// [`Self::clear_ttl`]，否则会出现「覆盖了值却保留旧过期时间」的诡异行为。
    pub fn set(&mut self, key: Vec<u8>, value: Obj) {
        self.data.insert(key, value);
    }

    /// 删除键（同时清过期表）。
    pub fn remove(&mut self, key: &[u8]) -> bool {
        self.expires.remove(key);
        self.data.remove(key).is_some()
    }

    /// 键是否存在（已处理惰性过期）。
    pub fn contains(&mut self, key: &[u8]) -> bool {
        if self.purge_if_expired(key) {
            return false;
        }
        self.data.contains_key(key)
    }

    /// 键总数（不含已过期未清理的）。
    pub fn dbsize(&mut self) -> usize {
        let now = now_ms();
        let mut dead: Vec<Vec<u8>> = Vec::new();
        for (k, &deadline) in self.expires.iter() {
            if deadline <= now {
                dead.push(k.clone());
            }
        }
        for k in &dead {
            self.expires.remove(k);
            self.data.remove(k);
        }
        self.data.len()
    }

    /// 清空数据库（键空间、过期表、商品标签缓存、推荐引擎）。
    pub fn flush(&mut self) {
        self.data.clear();
        self.expires.clear();
        self.product_tags.clear();
        // 推荐引擎的索引同样来自键空间，一起重置，避免 `REC.*` 返回已删除的商品。
        self.rec = Recommender::new(Vec::new(), Vec::new(), &[]);
    }

    /// 惰性过期：返回 `true` 表示键已过期并已被删除。
    pub fn is_expired(&mut self, key: &[u8]) -> bool {
        self.purge_if_expired(key)
    }

    /// 主动过期扫描：清理已过期的键，返回清理条数。
    ///
    /// Redis 每 100ms 随机抽样 20 个带 TTL 的键，过期比例超过 25% 就继续抽样，
    /// 用一个时间上限防止长时间阻塞。这里简化成「按 `expires` 表顺序扫一遍、
    /// 最多清理 `samples` 个」：
    /// * `expires` 只装设了 TTL 的键，通常远小于整个键空间，全扫一遍代价可接受；
    /// * 顺序扫描是确定性的，单测才能断言「清理了几个」。
    pub fn active_expire_cycle(&mut self, now_ms: i64, samples: usize) -> usize {
        if samples == 0 || self.expires.is_empty() {
            return 0;
        }
        let mut dead: Vec<Vec<u8>> = Vec::new();
        for (k, &deadline) in self.expires.iter() {
            if deadline <= now_ms {
                dead.push(k.clone());
                if dead.len() >= samples {
                    break;
                }
            }
        }
        for k in &dead {
            self.expires.remove(k);
            self.data.remove(k);
        }
        dead.len()
    }

    /// 剩余存活毫秒数；`-1` 无过期，`-2` 键不存在。
    pub fn pttl(&mut self, key: &[u8]) -> i64 {
        if self.purge_if_expired(key) {
            return -2;
        }
        if !self.data.contains_key(key) {
            return -2;
        }
        match self.expires.get(key) {
            Some(&deadline) => (deadline - now_ms()).max(0),
            None => -1,
        }
    }

    /// 清除键的过期时间，返回是否真的清掉了。
    pub fn clear_ttl(&mut self, key: &[u8]) -> bool {
        self.expires.remove(key).is_some()
    }

    /// 设置过期时刻（Unix 毫秒）。
    pub fn set_expire_at(&mut self, key: &[u8], at_ms: i64) {
        self.expires.insert(key.to_vec(), at_ms);
    }

    // ---- 带类型检查的取值/取值并创建辅助函数 ----
    //
    // 这些函数把「类型不符返回 WRONGTYPE」这条规则集中到一处，
    // 各命令处理器不必各自写一遍 match。

    /// 取字符串值；键不存在返回 `Ok(None)`，类型不符返回 `Err(WRONGTYPE)`。
    pub fn get_str(&mut self, key: &[u8]) -> Result<Option<&[u8]>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Obj::Str(s)) => Ok(Some(s.as_slice())),
            Some(_) => Err(errors::WRONGTYPE.to_string()),
        }
    }

    /// 取哈希；不存在 `Ok(None)`。
    pub fn get_hash(&mut self, key: &[u8]) -> Result<Option<&Dict<Vec<u8>>>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Obj::Hash(h)) => Ok(Some(h)),
            Some(_) => Err(errors::WRONGTYPE.to_string()),
        }
    }

    /// 取哈希（不存在则创建）。
    pub fn ensure_hash(&mut self, key: &[u8]) -> Result<&mut Dict<Vec<u8>>, String> {
        if self.get(key).is_none() {
            self.set(key.to_vec(), Obj::Hash(Dict::new()));
        }
        match self.get_mut(key) {
            Some(Obj::Hash(h)) => Ok(h),
            _ => Err(errors::WRONGTYPE.to_string()),
        }
    }

    /// 取列表（不存在则创建）。
    pub fn ensure_list(&mut self, key: &[u8]) -> Result<&mut VecDeque<Vec<u8>>, String> {
        if self.get(key).is_none() {
            self.set(key.to_vec(), Obj::List(VecDeque::new()));
        }
        match self.get_mut(key) {
            Some(Obj::List(l)) => Ok(l),
            _ => Err(errors::WRONGTYPE.to_string()),
        }
    }

    /// 取集合（不存在则创建）。
    pub fn ensure_set(&mut self, key: &[u8]) -> Result<&mut Dict<()>, String> {
        if self.get(key).is_none() {
            self.set(key.to_vec(), Obj::Set(Dict::new()));
        }
        match self.get_mut(key) {
            Some(Obj::Set(s)) => Ok(s),
            _ => Err(errors::WRONGTYPE.to_string()),
        }
    }

    /// 取有序集合（不存在则创建）。
    pub fn ensure_zset(&mut self, key: &[u8]) -> Result<&mut crate::zset::ZSet, String> {
        if self.get(key).is_none() {
            self.set(key.to_vec(), Obj::ZSet(crate::zset::ZSet::new()));
        }
        match self.get_mut(key) {
            Some(Obj::ZSet(z)) => Ok(z),
            _ => Err(errors::WRONGTYPE.to_string()),
        }
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

/// 一条连接上的 `MULTI` 事务状态。
///
/// 事务状态是**连接私有**的（两个客户端互不影响），所以它不能放进 `Database`，
/// 只能由服务端为每条连接各持一份，再借给 [`ExecCtx`]。
#[derive(Debug, Default)]
pub struct MultiState {
    /// 是否处于 MULTI 块中
    pub active: bool,
    /// 已排队但尚未执行的原始命令（含命令名）
    pub queue: Vec<Vec<Vec<u8>>>,
    /// 排队期间是否出现过错误（出现错误时 EXEC 直接放弃）
    pub error: bool,
}

impl MultiState {
    /// 新建空状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 进入事务。
    pub fn begin(&mut self) {
        self.active = true;
        self.queue.clear();
        self.error = false;
    }

    /// 退出事务并丢弃队列。
    pub fn reset(&mut self) {
        self.active = false;
        self.queue.clear();
        self.error = false;
    }
}

/// 命令执行上下文：数据库 + 统计 + AOF + 事务状态。
///
/// 除 `db` 外的字段都是 `Option`/可缺省：单测里直接 `ExecCtx::new(&mut db)` 就能跑，
/// 不需要假装存在一个服务端。
pub struct ExecCtx<'a> {
    pub db: &'a mut Database,
    /// 本连接是否处于 MULTI 队列状态
    pub in_multi: bool,
    /// 命令是否成功执行（决定是否写 AOF）
    pub dirty: bool,
    /// 全局统计（`INFO` 用）
    pub stats: Option<&'a Arc<Stats>>,
    /// AOF 写入器（`appendonly` 关闭时为 `None`）
    pub aof: Option<&'a Arc<Mutex<Aof>>>,
    /// 本连接的 MULTI 状态
    pub multi: Option<&'a mut MultiState>,
}

impl<'a> ExecCtx<'a> {
    /// 只带数据库的最小上下文（单测与 AOF 重放用）。
    pub fn new(db: &'a mut Database) -> Self {
        Self {
            db,
            in_multi: false,
            dirty: false,
            stats: None,
            aof: None,
            multi: None,
        }
    }

    /// 挂上统计句柄。
    pub fn with_stats(mut self, stats: &'a Arc<Stats>) -> Self {
        self.stats = Some(stats);
        self
    }

    /// 挂上 AOF 句柄。
    pub fn with_aof(mut self, aof: &'a Arc<Mutex<Aof>>) -> Self {
        self.aof = Some(aof);
        self
    }

    /// 挂上事务状态。
    pub fn with_multi(mut self, multi: &'a mut MultiState) -> Self {
        self.in_multi = multi.active;
        self.multi = Some(multi);
        self
    }

    /// 标记本次命令产生了写入，并把它追加进 AOF。
    ///
    /// 放在这里而不是服务端循环里，是为了让 `EXEC` 内部的每条子命令也能各自落盘
    /// （`MULTI/EXEC` 的原子性由单线程 + 单把锁保证，重放时逐条执行等价）。
    ///
    /// 写盘失败不再静默：持久化是「真实可用系统」的底线，失败了至少要在
    /// 日志里看得见。用 5 秒一次的限流避免磁盘满时把 stderr 刷爆。
    pub fn mark_dirty(&mut self, args: &[Vec<u8>]) {
        self.dirty = true;
        if let Some(aof) = self.aof {
            match aof.lock() {
                Ok(mut aof) => {
                    if let Err(e) = aof.append(args) {
                        log_aof_error("写入", &e);
                    } else if let Err(e) = aof.maybe_flush(now_ms()) {
                        log_aof_error("fsync", &e);
                    }
                }
                Err(poisoned) => {
                    let mut aof = poisoned.into_inner();
                    let _ = aof.flush_buf();
                    log_aof_error("加锁", &std::io::Error::other("AOF mutex poisoned"));
                }
            }
        }
    }

    /// 统计一次命中（`GET` 等读命令用）。
    pub fn note_hit(&self) {
        if let Some(s) = self.stats {
            s.incr_hit();
        }
    }

    /// 统计一次未命中。
    pub fn note_miss(&self) {
        if let Some(s) = self.stats {
            s.incr_miss();
        }
    }
}

/// AOF 错误日志的限流时间戳（Unix 毫秒），最多每 5 秒打印一次。
static AOF_ERR_LAST_LOG_MS: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(i64::MIN / 2);

/// 打印一条 AOF 错误日志（限流），避免磁盘异常时刷爆 stderr。
fn log_aof_error(what: &str, e: &std::io::Error) {
    use std::sync::atomic::Ordering;
    let now = now_ms();
    let last = AOF_ERR_LAST_LOG_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 5_000 {
        AOF_ERR_LAST_LOG_MS.store(now, Ordering::Relaxed);
        eprintln!("mredis-server: AOF {what}失败: {e}（持久化可能不完整）");
    }
}

/// 解析整数参数，失败返回 `NOT_INT` 文案。
pub(crate) fn parse_i64(bytes: &[u8]) -> Result<i64, String> {
    let s = std::str::from_utf8(bytes).map_err(|_| errors::NOT_INT.to_string())?;
    s.trim()
        .parse::<i64>()
        .map_err(|_| errors::NOT_INT.to_string())
}

/// 解析浮点参数，失败（含 NaN）返回 `NOT_FLOAT` 文案。
pub(crate) fn parse_f64(bytes: &[u8]) -> Result<f64, String> {
    let s = std::str::from_utf8(bytes).map_err(|_| errors::NOT_FLOAT.to_string())?;
    let v = s
        .trim()
        .parse::<f64>()
        .map_err(|_| errors::NOT_FLOAT.to_string())?;
    // NaN 参与排序会让跳表的比较全部失效，直接拒绝。
    if v.is_nan() {
        return Err(errors::NOT_FLOAT.to_string());
    }
    Ok(v)
}

/// 把浮点数格式化成 Redis 风格的字符串（整数不带小数点）。
pub(crate) fn format_float(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e17 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_remove_roundtrip() {
        let mut db = Database::new();
        db.set(b"k".to_vec(), Obj::Str(b"v".to_vec()));
        assert!(db.contains(b"k"));
        assert_eq!(db.dbsize(), 1);
        assert_eq!(db.get_str(b"k").unwrap(), Some(&b"v"[..]));
        assert!(db.remove(b"k"));
        assert!(!db.contains(b"k"));
        assert_eq!(db.dbsize(), 0);
    }

    #[test]
    fn wrong_type_reports_error() {
        let mut db = Database::new();
        db.set(b"k".to_vec(), Obj::List(VecDeque::new()));
        assert_eq!(db.get_str(b"k").unwrap_err(), errors::WRONGTYPE);
        assert!(db.get_hash(b"k").is_err());
    }

    #[test]
    fn pttl_reports_missing_and_no_expire() {
        let mut db = Database::new();
        assert_eq!(db.pttl(b"nope"), -2);
        db.set(b"k".to_vec(), Obj::Str(b"v".to_vec()));
        assert_eq!(db.pttl(b"k"), -1);
        db.set_expire_at(b"k", now_ms() + 100_000);
        let ttl = db.pttl(b"k");
        assert!(ttl > 90_000 && ttl <= 100_000, "ttl = {ttl}");
    }

    #[test]
    fn expired_key_disappears_on_access() {
        let mut db = Database::new();
        db.set(b"k".to_vec(), Obj::Str(b"v".to_vec()));
        db.set_expire_at(b"k", now_ms() - 1);
        assert!(db.is_expired(b"k"));
        assert!(!db.contains(b"k"));
        assert_eq!(db.dbsize(), 0);
    }

    #[test]
    fn active_expire_cycle_cleans_at_most_samples() {
        let mut db = Database::new();
        for i in 0..5 {
            let k = format!("k{i}").into_bytes();
            db.set(k.clone(), Obj::Str(b"v".to_vec()));
            db.set_expire_at(&k, now_ms() - 1);
        }
        // 注意断言 `data.len()` 而不是 `dbsize()`：后者会顺手把所有过期键都清掉
        // （它的语义是「活着的键数」），用它验证「只清 samples 个」会永远失败。
        assert_eq!(db.active_expire_cycle(now_ms(), 3), 3);
        assert_eq!(db.data.len(), 2);
        assert_eq!(db.active_expire_cycle(now_ms(), 100), 2);
        assert_eq!(db.data.len(), 0);
        assert_eq!(db.active_expire_cycle(now_ms(), 100), 0);
        // dbsize 做全量清理后应为 0。
        assert_eq!(db.dbsize(), 0);
    }

    #[test]
    fn ensure_helpers_create_then_return_existing() {
        let mut db = Database::new();
        db.ensure_hash(b"h")
            .unwrap()
            .insert(b"f".to_vec(), b"v".to_vec());
        assert_eq!(db.get_hash(b"h").unwrap().unwrap().len(), 1);
        db.ensure_list(b"l").unwrap().push_back(b"x".to_vec());
        assert_eq!(db.get(b"l").unwrap().len(), 1);
        db.ensure_set(b"s").unwrap().insert(b"m".to_vec(), ());
        db.ensure_zset(b"z").unwrap().insert(b"m".to_vec(), 1.0);
        assert_eq!(db.dbsize(), 4);
        // 类型冲突时 ensure_* 必须报错而不是覆盖。
        assert!(db.ensure_list(b"h").is_err());
    }

    #[test]
    fn flush_resets_everything() {
        let mut db = Database::new();
        db.set(b"k".to_vec(), Obj::Str(b"v".to_vec()));
        db.set_expire_at(b"k", now_ms() + 1000);
        db.flush();
        assert_eq!(db.dbsize(), 0);
        assert!(db.expires.is_empty());
        assert_eq!(db.rec.stats().products, 0);
    }

    #[test]
    fn parse_helpers_reject_bad_input() {
        assert_eq!(parse_i64(b"12").unwrap(), 12);
        assert_eq!(parse_i64(b"-3").unwrap(), -3);
        assert!(parse_i64(b"abc").is_err());
        assert!(parse_f64(b"1.5").unwrap() > 1.4);
        assert!(parse_f64(b"nan").is_err());
        assert_eq!(format_float(3.0), "3");
        assert_eq!(format_float(3.25), "3.25");
    }

    #[test]
    fn multi_state_begin_and_reset() {
        let mut m = MultiState::new();
        m.queue.push(vec![b"SET".to_vec()]);
        m.begin();
        assert!(m.active);
        assert!(m.queue.is_empty(), "begin 应清空旧队列");
        m.reset();
        assert!(!m.active);
    }
}
