//! 字符串、键管理、过期相关命令。
//!
//! 这些命令是所有上层类型的基础：`SET`/`GET` 操作 `Obj::Str`，`DEL`/`EXPIRE`/`TTL`
//! 则直接操作键空间与过期表，与值的类型无关。

use crate::db::{ExecCtx, parse_i64};
use crate::dict::Dict;
use crate::errors;
use crate::expire::now_ms;
use crate::obj::Obj;
use mredis_proto::Resp;

/// `SET key value` —— 覆盖写入并清除旧 TTL。
///
/// 清除 TTL 是 Redis 的既定语义：`SET` 之后这个键就是「新的」，
/// 沿用旧的过期时间会让用户觉得「我明明重设了值，怎么还是过期了」。
pub fn cmd_set(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let key = args[1].clone();
    let value = args[2].clone();
    // 支持 `SET k v EX <sec>` / `PX <ms>` 两个常见选项。
    let mut expire_at: Option<i64> = None;
    let mut i = 3;
    while i < args.len() {
        let opt = String::from_utf8_lossy(&args[i]).to_ascii_uppercase();
        match opt.as_str() {
            "EX" | "PX" => {
                let Some(raw) = args.get(i + 1) else {
                    return Resp::error(errors::SYNTAX);
                };
                let Ok(n) = parse_i64(raw) else {
                    return Resp::error(errors::NOT_INT);
                };
                if n <= 0 {
                    return Resp::error("ERR invalid expire time in 'set' command");
                }
                let ms = if opt == "EX" {
                    n.saturating_mul(1000)
                } else {
                    n
                };
                expire_at = Some(now_ms().saturating_add(ms));
                i += 2;
            }
            _ => return Resp::error(errors::SYNTAX),
        }
    }

    ctx.db.set(key.clone(), Obj::Str(value));
    match expire_at {
        Some(at) => ctx.db.set_expire_at(&key, at),
        None => {
            ctx.db.clear_ttl(&key);
        }
    }
    ctx.mark_dirty(args);
    Resp::ok()
}

/// `GET key`
pub fn cmd_get(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    // 先把值拷贝出来，结束对 `ctx.db` 的借用，才能再调 `ctx.note_hit()`——
    // 后者要借整个 `ctx`。这也是 `get_str` 返回 `&[u8]` 而不是 `Vec<u8>`
    // 时必须付的代价：一次拷贝换一次统计。
    let got = ctx.db.get_str(&args[1]).map(|v| v.map(|b| b.to_vec()));
    match got {
        Ok(Some(v)) => {
            ctx.note_hit();
            Resp::Bulk(v)
        }
        Ok(None) => {
            ctx.note_miss();
            Resp::Null
        }
        Err(e) => Resp::Error(e),
    }
}

/// `SETNX key value` —— 仅当键不存在时写入。
pub fn cmd_setnx(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    if ctx.db.contains(&args[1]) {
        return Resp::Int(0);
    }
    ctx.db.set(args[1].clone(), Obj::Str(args[2].clone()));
    ctx.db.clear_ttl(&args[1]);
    ctx.mark_dirty(args);
    Resp::Int(1)
}

/// `MSET k1 v1 [k2 v2 ...]`
pub fn cmd_mset(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    // arity 是「至少 3 个」，但必须是奇数个（命令 + 偶数个参数）。
    if args.len() % 2 == 0 {
        return Resp::error(errors::wrong_args("mset"));
    }
    for pair in args[1..].chunks(2) {
        let key = pair[0].clone();
        ctx.db.set(key.clone(), Obj::Str(pair[1].clone()));
        ctx.db.clear_ttl(&key);
    }
    ctx.mark_dirty(args);
    Resp::ok()
}

/// `MGET k1 [k2 ...]` —— 不存在的键返回 `Null`（不是错误）。
pub fn cmd_mget(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let mut out = Vec::with_capacity(args.len() - 1);
    for key in &args[1..] {
        // 同 `cmd_get`：先拷出值再统计，避免同时借用 `ctx.db` 与 `ctx`。
        let got = ctx.db.get_str(key).map(|v| v.map(|b| b.to_vec()));
        match got {
            Ok(Some(v)) => {
                ctx.note_hit();
                out.push(Resp::Bulk(v));
            }
            Ok(None) => {
                ctx.note_miss();
                out.push(Resp::Null);
            }
            // MGET 遇到类型不符的元素时按 Redis 语义返回 Null，整体不报错。
            Err(_) => out.push(Resp::Null),
        }
    }
    Resp::Array(out)
}

/// 在现有值上做整数加减，`delta` 为增量。
fn incr_by(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>], delta: i64) -> Resp {
    let key = &args[1];
    let cur = match ctx.db.get_str(key) {
        Ok(Some(v)) => match parse_i64(v) {
            Ok(n) => n,
            Err(e) => return Resp::Error(e),
        },
        Ok(None) => 0,
        Err(e) => return Resp::Error(e),
    };
    let Some(next) = cur.checked_add(delta) else {
        return Resp::error(errors::NOT_INT);
    };
    ctx.db
        .set(key.clone(), Obj::Str(next.to_string().into_bytes()));
    ctx.mark_dirty(args);
    Resp::Int(next)
}

/// `INCRBY key delta`
pub fn cmd_incrby(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(delta) = parse_i64(&args[2]) else {
        return Resp::error(errors::NOT_INT);
    };
    incr_by(ctx, args, delta)
}

/// `DECRBY key delta`
pub fn cmd_decrby(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(delta) = parse_i64(&args[2]) else {
        return Resp::error(errors::NOT_INT);
    };
    // `DECRBY k -5` 等价于 `INCRBY k 5`，用 neg 要防 i64::MIN。
    let Some(delta) = delta.checked_neg() else {
        return Resp::error(errors::NOT_INT);
    };
    incr_by(ctx, args, delta)
}

/// `INCR key`
pub fn cmd_incr(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    incr_by(ctx, args, 1)
}

/// `DECR key`
pub fn cmd_decr(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    incr_by(ctx, args, -1)
}

/// `APPEND key value` —— 返回追加后的长度。
pub fn cmd_append(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let key = args[1].clone();
    let extra = &args[2];
    let mut cur = match ctx.db.get_str(&key) {
        Ok(Some(v)) => v.to_vec(),
        Ok(None) => Vec::new(),
        Err(e) => return Resp::Error(e),
    };
    cur.extend_from_slice(extra);
    let len = cur.len() as i64;
    ctx.db.set(key, Obj::Str(cur));
    ctx.mark_dirty(args);
    Resp::Int(len)
}

/// `STRLEN key`
pub fn cmd_strlen(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get_str(&args[1]) {
        Ok(Some(v)) => Resp::Int(v.len() as i64),
        Ok(None) => Resp::Int(0),
        Err(e) => Resp::Error(e),
    }
}

/// `DEL k1 [k2 ...]` —— 返回真正删掉的个数。
pub fn cmd_del(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let mut removed = 0i64;
    for key in &args[1..] {
        if ctx.db.remove(key) {
            removed += 1;
            // 删掉商品键时同步摘掉推荐索引，避免 `REC.*` 返回幽灵商品。
            if let Some(id) = crate::cmd_rec::parse_prefixed_id(key, b"product:") {
                let tags = ctx.db.product_tags.remove(&id).unwrap_or_default();
                ctx.db.rec.index.remove(id, &tags);
            }
        }
    }
    if removed > 0 {
        ctx.mark_dirty(args);
    }
    Resp::Int(removed)
}

/// `EXISTS k1 [k2 ...]` —— 返回存在的个数（同名键重复出现也重复计数）。
pub fn cmd_exists(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let mut n = 0i64;
    for key in &args[1..] {
        if ctx.db.contains(key) {
            n += 1;
        }
    }
    Resp::Int(n)
}

/// `TYPE key`
pub fn cmd_type(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get(&args[1]) {
        Some(obj) => Resp::simple(obj.type_name()),
        None => Resp::simple("none"),
    }
}

/// `EXPIRE key seconds`
pub fn cmd_expire(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(secs) = parse_i64(&args[2]) else {
        return Resp::error(errors::NOT_INT);
    };
    set_relative_expire(ctx, args, secs.saturating_mul(1000))
}

/// `PEXPIRE key milliseconds`
pub fn cmd_pexpire(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(ms) = parse_i64(&args[2]) else {
        return Resp::error(errors::NOT_INT);
    };
    set_relative_expire(ctx, args, ms)
}

/// 相对过期：`delta_ms` 为负或 0 时立即删除键（Redis 语义）。
fn set_relative_expire(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>], delta_ms: i64) -> Resp {
    if !ctx.db.contains(&args[1]) {
        return Resp::Int(0);
    }
    if delta_ms <= 0 {
        ctx.db.remove(&args[1]);
        ctx.mark_dirty(args);
        return Resp::Int(1);
    }
    let at = now_ms().saturating_add(delta_ms);
    ctx.db.set_expire_at(&args[1], at);
    ctx.mark_dirty(args);
    Resp::Int(1)
}

/// `PEXPIREAT key unix_ms`
pub fn cmd_pexpireat(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(at) = parse_i64(&args[2]) else {
        return Resp::error(errors::NOT_INT);
    };
    if !ctx.db.contains(&args[1]) {
        return Resp::Int(0);
    }
    if at <= now_ms() {
        ctx.db.remove(&args[1]);
        ctx.mark_dirty(args);
        return Resp::Int(1);
    }
    ctx.db.set_expire_at(&args[1], at);
    ctx.mark_dirty(args);
    Resp::Int(1)
}

/// `TTL key` —— 秒级剩余时间（向上取整）。
pub fn cmd_ttl(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let ms = ctx.db.pttl(&args[1]);
    match ms {
        -2 => Resp::Int(-2),
        -1 => Resp::Int(-1),
        // Redis 对 TTL 向上取整：剩 1ms 也要显示 1 秒，否则会显示 0 让人误以为已过期。
        _ => Resp::Int((ms + 999) / 1000),
    }
}

/// `PTTL key`
pub fn cmd_pttl(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    Resp::Int(ctx.db.pttl(&args[1]))
}

/// `PERSIST key` —— 去掉过期时间。
pub fn cmd_persist(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    if !ctx.db.contains(&args[1]) {
        return Resp::Int(0);
    }
    if ctx.db.clear_ttl(&args[1]) {
        ctx.mark_dirty(args);
        Resp::Int(1)
    } else {
        Resp::Int(0)
    }
}

/// 简易通配符匹配（支持 `*` `?` `[...]`），与 Redis 的 `stringmatchlen` 等价。
///
/// 为什么不用正则：Redis 的 glob 语义与正则不同（`*` 不跨语义、`[...]` 是字符类），
/// 而且引入正则库只为 `KEYS` 一个命令不值当。这里用回溯实现，模式里 `*` 的
/// 数量通常很少，实际代价接近线性。
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    // 记录最近一个 `*` 的位置，匹配失败时回溯到这里再多吞一个字符。
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    star = Some((p, t));
                    p += 1;
                    continue;
                }
                b'?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' => {
                    // 解析字符类，找不到 `]` 时按普通字符处理。
                    if let Some((matched, next)) = match_class(&pattern[p..], text[t]) {
                        if matched {
                            p += next;
                            t += 1;
                            continue;
                        }
                    } else if pattern[p] == text[t] {
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
                b'\\' if p + 1 < pattern.len() => {
                    if pattern[p + 1] == text[t] {
                        p += 2;
                        t += 1;
                        continue;
                    }
                }
                c if c == text[t] => {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        }
        // 不匹配：如果之前有 `*`，让它多吞一个字符再试。
        match star {
            Some((sp, st)) => {
                p = sp + 1;
                t = st + 1;
                star = Some((sp, st + 1));
            }
            None => return false,
        }
    }
    // 文本走完了，模式里剩下的必须全是 `*`。
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// 解析 `[...]` 字符类。返回 `(是否命中, 模式里消耗的字节数)`。
fn match_class(pattern: &[u8], ch: u8) -> Option<(bool, usize)> {
    debug_assert_eq!(pattern.first(), Some(&b'['));
    let mut i = 1;
    let mut negate = false;
    if i < pattern.len() && (pattern[i] == b'^' || pattern[i] == b'!') {
        negate = true;
        i += 1;
    }
    let mut hit = false;
    while i < pattern.len() {
        if pattern[i] == b']' {
            return Some((hit != negate, i + 1));
        }
        // `a-z` 区间
        if i + 2 < pattern.len() && pattern[i + 1] == b'-' && pattern[i + 2] != b']' {
            if pattern[i] <= ch && ch <= pattern[i + 2] {
                hit = true;
            }
            i += 3;
        } else {
            if pattern[i] == ch {
                hit = true;
            }
            i += 1;
        }
    }
    // 没有闭合的 `]`，不是合法的字符类。
    None
}

/// `KEYS pattern`
pub fn cmd_keys(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let pattern = &args[1];
    let keys: Vec<Vec<u8>> = ctx
        .db
        .data
        .keys()
        .filter(|k| glob_match(pattern, k))
        .cloned()
        .collect();
    // 收集完再删过期键，避免边迭代边修改。
    let now = now_ms();
    let mut alive = Vec::with_capacity(keys.len());
    for k in keys {
        let expired = ctx.db.expires.get(&k).is_some_and(|&d| d <= now);
        if expired {
            ctx.db.expires.remove(&k);
            ctx.db.data.remove(&k);
        } else {
            alive.push(Resp::Bulk(k));
        }
    }
    Resp::Array(alive)
}

/// `SCAN cursor [MATCH pattern] [COUNT n]`
///
/// 返回 `[下一个游标, [键...]]`。游标语义由 [`Dict::scan`] 保证：
/// rehash 期间按反向二进制递增，不会漏也不会重复。
pub fn cmd_scan(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    // 手写十进制解析：`cursor` 是 u64，用 `parse` 也行，但这里要区分
    // 「非数字」与「溢出」，手写能给出统一的 NOT_INT 文案。
    let mut cursor: u64 = 0;
    for b in &args[1] {
        if !b.is_ascii_digit() {
            return Resp::error(errors::NOT_INT);
        }
        let Some(next) = cursor
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(b - b'0')))
        else {
            return Resp::error(errors::NOT_INT);
        };
        cursor = next;
    }

    let mut pattern: Option<Vec<u8>> = None;
    let mut count = 10usize;
    let mut i = 2;
    while i < args.len() {
        let opt = String::from_utf8_lossy(&args[i]).to_ascii_uppercase();
        match opt.as_str() {
            "MATCH" => {
                let Some(p) = args.get(i + 1) else {
                    return Resp::error(errors::SYNTAX);
                };
                pattern = Some(p.clone());
                i += 2;
            }
            "COUNT" => {
                let Some(raw) = args.get(i + 1) else {
                    return Resp::error(errors::SYNTAX);
                };
                let Ok(n) = parse_i64(raw) else {
                    return Resp::error(errors::NOT_INT);
                };
                if n <= 0 {
                    return Resp::error(errors::SYNTAX);
                }
                count = n as usize;
                i += 2;
            }
            _ => return Resp::error(errors::SYNTAX),
        }
    }

    let (next, keys) = ctx.db.data.scan(cursor, count);
    let now = now_ms();
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        let expired = ctx.db.expires.get(&k).is_some_and(|&d| d <= now);
        if expired {
            ctx.db.expires.remove(&k);
            ctx.db.data.remove(&k);
            continue;
        }
        if let Some(p) = &pattern {
            if !glob_match(p, &k) {
                continue;
            }
        }
        out.push(Resp::Bulk(k));
    }
    Resp::Array(vec![
        Resp::Bulk(next.to_string().into_bytes()),
        Resp::Array(out),
    ])
}

/// 把一个 `Dict<Vec<u8>>` 的全部字段导出成 `[field, value, field, value, ...]`。
pub(crate) fn hash_to_pairs(h: &Dict<Vec<u8>>) -> Vec<Resp> {
    let mut out = Vec::with_capacity(h.len() * 2);
    for (k, v) in h.iter() {
        out.push(Resp::Bulk(k.clone()));
        out.push(Resp::Bulk(v.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn run(db: &mut Database, parts: &[&str]) -> Resp {
        let args: Vec<Vec<u8>> = parts.iter().map(|p| p.as_bytes().to_vec()).collect();
        let mut ctx = ExecCtx::new(db);
        super::super::command::dispatch(&mut ctx, &args)
    }

    #[test]
    fn set_get_roundtrip_and_ttl_cleared() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["SET", "k", "v"]), Resp::ok());
        assert_eq!(run(&mut db, &["GET", "k"]), Resp::Bulk(b"v".to_vec()));
        assert_eq!(run(&mut db, &["GET", "nope"]), Resp::Null);
        run(&mut db, &["PEXPIRE", "k", "100000"]);
        assert!(run(&mut db, &["PTTL", "k"]).as_int().unwrap() > 0);
        // SET 覆盖后 TTL 必须消失。
        run(&mut db, &["SET", "k", "v2"]);
        assert_eq!(run(&mut db, &["PTTL", "k"]), Resp::Int(-1));
    }

    #[test]
    fn setnx_only_writes_when_absent() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["SETNX", "k", "a"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["SETNX", "k", "b"]), Resp::Int(0));
        assert_eq!(run(&mut db, &["GET", "k"]), Resp::Bulk(b"a".to_vec()));
    }

    #[test]
    fn incr_family_and_errors() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["INCR", "n"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["INCRBY", "n", "10"]), Resp::Int(11));
        assert_eq!(run(&mut db, &["DECR", "n"]), Resp::Int(10));
        assert_eq!(run(&mut db, &["DECRBY", "n", "4"]), Resp::Int(6));
        run(&mut db, &["SET", "s", "abc"]);
        assert!(run(&mut db, &["INCR", "s"]).is_error());
        // 溢出必须报错而不是 panic。
        run(&mut db, &["SET", "big", "9223372036854775807"]);
        assert!(run(&mut db, &["INCR", "big"]).is_error());
    }

    #[test]
    fn append_and_strlen() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["APPEND", "k", "abc"]), Resp::Int(3));
        assert_eq!(run(&mut db, &["APPEND", "k", "de"]), Resp::Int(5));
        assert_eq!(run(&mut db, &["STRLEN", "k"]), Resp::Int(5));
        assert_eq!(run(&mut db, &["STRLEN", "nope"]), Resp::Int(0));
    }

    #[test]
    fn del_exists_type() {
        let mut db = Database::new();
        run(&mut db, &["SET", "a", "1"]);
        run(&mut db, &["SET", "b", "2"]);
        assert_eq!(run(&mut db, &["EXISTS", "a", "b", "c"]), Resp::Int(2));
        assert_eq!(run(&mut db, &["TYPE", "a"]), Resp::simple("string"));
        assert_eq!(run(&mut db, &["TYPE", "zz"]), Resp::simple("none"));
        assert_eq!(run(&mut db, &["DEL", "a", "zz"]), Resp::Int(1));
    }

    #[test]
    fn expire_and_persist() {
        let mut db = Database::new();
        run(&mut db, &["SET", "k", "v"]);
        assert_eq!(run(&mut db, &["EXPIRE", "k", "100"]), Resp::Int(1));
        assert!(run(&mut db, &["TTL", "k"]).as_int().unwrap() > 0);
        assert_eq!(run(&mut db, &["PERSIST", "k"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["TTL", "k"]), Resp::Int(-1));
        assert_eq!(run(&mut db, &["TTL", "nope"]), Resp::Int(-2));
        // 过期时间为 0 或负数：立即删除。
        assert_eq!(run(&mut db, &["EXPIRE", "k", "0"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["EXISTS", "k"]), Resp::Int(0));
    }

    #[test]
    fn glob_matching_rules() {
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"user:*", b"user:1"));
        assert!(!glob_match(b"user:*", b"product:1"));
        assert!(glob_match(b"h?llo", b"hello"));
        assert!(glob_match(b"[abc]x", b"bx"));
        assert!(!glob_match(b"[abc]x", b"dx"));
        assert!(glob_match(b"[a-z]x", b"qx"));
        assert!(glob_match(b"[^a-z]x", b"Qx"));
        assert!(glob_match(b"a*b*c", b"axxbyyc"));
        assert!(!glob_match(b"a*b*c", b"axxbyy"));
        assert!(glob_match(b"", b""));
        assert!(!glob_match(b"", b"x"));
    }

    #[test]
    fn keys_with_pattern() {
        let mut db = Database::new();
        run(&mut db, &["SET", "user:1", "a"]);
        run(&mut db, &["SET", "user:2", "b"]);
        run(&mut db, &["SET", "product:1", "c"]);
        let Resp::Array(keys) = run(&mut db, &["KEYS", "user:*"]) else {
            panic!("KEYS 应返回数组");
        };
        assert_eq!(keys.len(), 2);
        let Resp::Array(all) = run(&mut db, &["KEYS", "*"]) else {
            panic!("KEYS 应返回数组");
        };
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn scan_returns_cursor_and_keys() {
        let mut db = Database::new();
        for i in 0..100 {
            run(&mut db, &["SET", &format!("k{i}"), "v"]);
        }
        let mut cursor = 0u64;
        let mut seen = std::collections::HashSet::new();
        loop {
            let Resp::Array(pair) = run(&mut db, &["SCAN", &cursor.to_string(), "COUNT", "10"])
            else {
                panic!("SCAN 应返回数组");
            };
            cursor = pair[0].as_int().unwrap() as u64;
            let Resp::Array(items) = &pair[1] else {
                panic!("第二项应是数组");
            };
            for it in items {
                seen.insert(it.as_string().unwrap());
            }
            if cursor == 0 {
                break;
            }
        }
        assert_eq!(seen.len(), 100, "SCAN 必须不重不漏");
    }

    #[test]
    fn scan_rejects_bad_cursor() {
        let mut db = Database::new();
        assert!(run(&mut db, &["SCAN", "abc"]).is_error());
        assert!(run(&mut db, &["SCAN", "0", "COUNT", "0"]).is_error());
    }

    #[test]
    fn mset_mget() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["MSET", "a", "1", "b", "2"]), Resp::ok());
        let Resp::Array(vals) = run(&mut db, &["MGET", "a", "b", "zz"]) else {
            panic!("MGET 应返回数组");
        };
        assert_eq!(vals[0], Resp::Bulk(b"1".to_vec()));
        assert_eq!(vals[2], Resp::Null);
        // 参数个数为偶数（键值不成对）必须报错。
        assert!(run(&mut db, &["MSET", "a", "1", "b"]).is_error());
    }
}
