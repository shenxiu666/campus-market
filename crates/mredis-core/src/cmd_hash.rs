//! 哈希命令。
//!
//! 哈希值内部就是 [`crate::dict::Dict`]，所以 `HSET`/`HGET` 的复杂度与键空间一样是
//! 均摊 O(1)，且同样享受增量 rehash 带来的平滑扩容。

use crate::db::{ExecCtx, parse_i64};
use crate::errors;
use crate::obj::Obj;
use mredis_proto::Resp;

/// `HSET key field value [field value ...]` —— 返回**新增**的字段数。
pub fn cmd_hset(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    if args.len() % 2 != 0 {
        return Resp::error(errors::wrong_args("hset"));
    }
    let key = args[1].clone();
    let hash = match ctx.db.ensure_hash(&key) {
        Ok(h) => h,
        Err(e) => return Resp::Error(e),
    };
    let mut added = 0i64;
    for pair in args[2..].chunks(2) {
        if hash.insert(pair[0].clone(), pair[1].clone()).is_none() {
            added += 1;
        }
    }
    ctx.mark_dirty(args);
    Resp::Int(added)
}

/// `HGET key field`
pub fn cmd_hget(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    // 先拷贝再统计：`get_hash` 的返回值借着 `ctx.db`，而 `note_hit` 要借 `ctx`。
    let got = ctx
        .db
        .get_hash(&args[1])
        .map(|h| h.and_then(|h| h.get(&args[2]).cloned()));
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

/// `HMSET key field value [field value ...]` —— 老式命令，固定回 `+OK`。
pub fn cmd_hmset(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    if args.len() % 2 != 0 {
        return Resp::error(errors::wrong_args("hmset"));
    }
    let key = args[1].clone();
    let hash = match ctx.db.ensure_hash(&key) {
        Ok(h) => h,
        Err(e) => return Resp::Error(e),
    };
    for pair in args[2..].chunks(2) {
        hash.insert(pair[0].clone(), pair[1].clone());
    }
    ctx.mark_dirty(args);
    Resp::ok()
}

/// `HMGET key field [field ...]`
pub fn cmd_hmget(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let hash = match ctx.db.get_hash(&args[1]) {
        Ok(h) => h,
        Err(e) => return Resp::Error(e),
    };
    let mut out = Vec::with_capacity(args.len() - 2);
    for field in &args[2..] {
        match hash.and_then(|h| h.get(field)) {
            Some(v) => out.push(Resp::Bulk(v.clone())),
            None => out.push(Resp::Null),
        }
    }
    Resp::Array(out)
}

/// `HGETALL key` —— 返回 `[field, value, ...]` 扁平数组。
pub fn cmd_hgetall(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    // 和 `HGET` 一样计入 keyspace 命中率：Web 层读商品/用户全走 HGETALL，
    // 不统计的话 `INFO` 的 `keyspace_hits` 会永远是 0、演示面板的命中率恒为 0%。
    let got = ctx
        .db
        .get_hash(&args[1])
        .map(|h| h.map(crate::cmd_string::hash_to_pairs));
    match got {
        Ok(Some(pairs)) => {
            ctx.note_hit();
            Resp::Array(pairs)
        }
        Ok(None) => {
            ctx.note_miss();
            Resp::Array(Vec::new())
        }
        Err(e) => Resp::Error(e),
    }
}

/// `HDEL key field [field ...]` —— 返回删掉的字段数；删空后顺手删键。
pub fn cmd_hdel(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let key = args[1].clone();
    let hash = match ctx.db.get_hash(&key) {
        Ok(Some(h)) => h,
        Ok(None) => return Resp::Int(0),
        Err(e) => return Resp::Error(e),
    };
    let mut removed = 0i64;
    // 先统计要删的字段，再借可变引用删除（不可变借用还没结束）。
    let targets: Vec<Vec<u8>> = args[2..]
        .iter()
        .filter(|f| hash.contains_key(f))
        .cloned()
        .collect();
    removed += targets.len() as i64;
    let empty = {
        let hash = match ctx.db.ensure_hash(&key) {
            Ok(h) => h,
            Err(e) => return Resp::Error(e),
        };
        for f in &targets {
            hash.remove(f);
        }
        hash.is_empty()
    };
    // Redis 语义：哈希为空就删掉键，`EXISTS`/`KEYS` 不应再看到它。
    if empty {
        ctx.db.remove(&key);
    }
    if removed > 0 {
        ctx.mark_dirty(args);
    }
    Resp::Int(removed)
}

/// `HEXISTS key field`
pub fn cmd_hexists(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get_hash(&args[1]) {
        Ok(Some(h)) => Resp::Int(i64::from(h.contains_key(&args[2]))),
        Ok(None) => Resp::Int(0),
        Err(e) => Resp::Error(e),
    }
}

/// `HINCRBY key field delta`
pub fn cmd_hincrby(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(delta) = parse_i64(&args[3]) else {
        return Resp::error(errors::NOT_INT);
    };
    let key = args[1].clone();
    let field = args[2].clone();
    let hash = match ctx.db.ensure_hash(&key) {
        Ok(h) => h,
        Err(e) => return Resp::Error(e),
    };
    let cur = match hash.get(&field) {
        Some(v) => match parse_i64(v) {
            Ok(n) => n,
            Err(e) => return Resp::Error(e),
        },
        None => 0,
    };
    let Some(next) = cur.checked_add(delta) else {
        return Resp::error(errors::NOT_INT);
    };
    hash.insert(field, next.to_string().into_bytes());
    ctx.mark_dirty(args);
    Resp::Int(next)
}

/// `HKEYS key`
pub fn cmd_hkeys(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get_hash(&args[1]) {
        Ok(Some(h)) => Resp::Array(h.keys().cloned().map(Resp::Bulk).collect()),
        Ok(None) => Resp::Array(Vec::new()),
        Err(e) => Resp::Error(e),
    }
}

/// `HVALS key`
pub fn cmd_hvals(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get_hash(&args[1]) {
        Ok(Some(h)) => Resp::Array(h.values().cloned().map(Resp::Bulk).collect()),
        Ok(None) => Resp::Array(Vec::new()),
        Err(e) => Resp::Error(e),
    }
}

/// `HLEN key`
pub fn cmd_hlen(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get_hash(&args[1]) {
        Ok(Some(h)) => Resp::Int(h.len() as i64),
        Ok(None) => Resp::Int(0),
        Err(e) => Resp::Error(e),
    }
}

/// 给非哈希键写入哈希字段（`--seed` 与 `REC.TRACK` 内部使用）。
///
/// 与 `HSET` 的区别：不经过 `ExecCtx` 的 AOF/dirty 逻辑，纯粹改内存，
/// 供「导入种子数据」「命令内部联动更新」这类不产生独立 AOF 记录的场景调用。
pub fn set_fields(db: &mut crate::db::Database, key: &[u8], fields: &[(&str, Vec<u8>)]) {
    let obj = Obj::Hash(crate::dict::Dict::new());
    if db.get(key).is_none() {
        db.set(key.to_vec(), obj);
    }
    if let Ok(hash) = db.ensure_hash(key) {
        for (f, v) in fields {
            hash.insert(f.as_bytes().to_vec(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch;
    use crate::db::Database;

    fn run(db: &mut Database, parts: &[&str]) -> Resp {
        let args: Vec<Vec<u8>> = parts.iter().map(|p| p.as_bytes().to_vec()).collect();
        let mut ctx = ExecCtx::new(db);
        dispatch(&mut ctx, &args)
    }

    #[test]
    fn hset_hget_hgetall() {
        let mut db = Database::new();
        assert_eq!(
            run(&mut db, &["HSET", "h", "a", "1", "b", "2"]),
            Resp::Int(2)
        );
        assert_eq!(run(&mut db, &["HSET", "h", "a", "9"]), Resp::Int(0));
        assert_eq!(run(&mut db, &["HGET", "h", "a"]), Resp::Bulk(b"9".to_vec()));
        assert_eq!(run(&mut db, &["HLEN", "h"]), Resp::Int(2));
        assert_eq!(run(&mut db, &["HEXISTS", "h", "b"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["HEXISTS", "h", "z"]), Resp::Int(0));
        let Resp::Array(all) = run(&mut db, &["HGETALL", "h"]) else {
            panic!("HGETALL 应返回数组");
        };
        assert_eq!(all.len(), 4);
        assert_eq!(run(&mut db, &["HGETALL", "zz"]), Resp::Array(vec![]));
    }

    #[test]
    fn hmget_returns_null_for_missing() {
        let mut db = Database::new();
        run(&mut db, &["HSET", "h", "a", "1"]);
        let Resp::Array(vals) = run(&mut db, &["HMGET", "h", "a", "zz"]) else {
            panic!("HMGET 应返回数组");
        };
        assert_eq!(vals[0], Resp::Bulk(b"1".to_vec()));
        assert_eq!(vals[1], Resp::Null);
    }

    #[test]
    fn hdel_removes_key_when_empty() {
        let mut db = Database::new();
        run(&mut db, &["HSET", "h", "a", "1", "b", "2"]);
        assert_eq!(run(&mut db, &["HDEL", "h", "a", "zz"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["HDEL", "h", "b"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["EXISTS", "h"]), Resp::Int(0));
    }

    #[test]
    fn hincrby_creates_and_rejects_non_int() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["HINCRBY", "h", "n", "5"]), Resp::Int(5));
        assert_eq!(run(&mut db, &["HINCRBY", "h", "n", "-2"]), Resp::Int(3));
        run(&mut db, &["HSET", "h", "s", "abc"]);
        assert!(run(&mut db, &["HINCRBY", "h", "s", "1"]).is_error());
    }

    #[test]
    fn hkeys_hvals() {
        let mut db = Database::new();
        run(&mut db, &["HSET", "h", "a", "1", "b", "2"]);
        let Resp::Array(ks) = run(&mut db, &["HKEYS", "h"]) else {
            panic!("应返回数组");
        };
        assert_eq!(ks.len(), 2);
        let Resp::Array(vs) = run(&mut db, &["HVALS", "h"]) else {
            panic!("应返回数组");
        };
        assert_eq!(vs.len(), 2);
        assert_eq!(run(&mut db, &["HKEYS", "zz"]), Resp::Array(vec![]));
    }

    #[test]
    fn wrong_type_is_reported() {
        let mut db = Database::new();
        run(&mut db, &["SET", "s", "v"]);
        let r = run(&mut db, &["HGET", "s", "f"]);
        assert!(r.is_error());
        assert_eq!(r.as_string().unwrap(), errors::WRONGTYPE);
    }

    #[test]
    fn set_fields_helper_writes_without_ctx() {
        let mut db = Database::new();
        set_fields(
            &mut db,
            b"product:1",
            &[("id", b"1".to_vec()), ("views", b"0".to_vec())],
        );
        assert_eq!(
            run(&mut db, &["HGET", "product:1", "views"]),
            Resp::Bulk(b"0".to_vec())
        );
    }
}
