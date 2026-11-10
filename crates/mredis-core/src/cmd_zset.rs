//! 有序集合命令。
//!
//! 底层是 [`crate::zset::ZSet`]（跳表 + 成员到分值的哈希索引）：
//! 按分值排序的区间查询 O(log n + k)，按成员查分值 O(1)。

use crate::db::{ExecCtx, format_float, parse_f64, parse_i64};
use crate::errors;
use crate::obj::Obj;
use mredis_proto::Resp;

/// `ZADD key score member [score member ...]` —— 返回**新增**成员数。
///
/// 已存在的成员只更新分值，不计入返回值（Redis 语义）。
pub fn cmd_zadd(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    if args.len() % 2 != 0 {
        return Resp::error(errors::wrong_args("zadd"));
    }
    // 先把所有分值解析完再落库：任一参数非法时整条命令失败，不留下半截写入。
    let mut pairs: Vec<(f64, Vec<u8>)> = Vec::with_capacity((args.len() - 2) / 2);
    for pair in args[2..].chunks(2) {
        match parse_f64(&pair[0]) {
            Ok(score) => pairs.push((score, pair[1].clone())),
            Err(e) => return Resp::Error(e),
        }
    }
    let key = args[1].clone();
    let zset = match ctx.db.ensure_zset(&key) {
        Ok(z) => z,
        Err(e) => return Resp::Error(e),
    };
    let mut added = 0i64;
    for (score, member) in pairs {
        if zset.insert(member, score) {
            added += 1;
        }
    }
    if added > 0 || args.len() > 4 {
        ctx.mark_dirty(args);
    }
    Resp::Int(added)
}

/// `ZRANGE key start stop [WITHSCORES]` —— 按分值升序，负下标从尾部算。
pub fn cmd_zrange(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    range_impl(ctx, args, false)
}

/// `ZREVRANGE key start stop [WITHSCORES]` —— 按分值降序。
pub fn cmd_zrevrange(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    range_impl(ctx, args, true)
}

fn range_impl(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>], rev: bool) -> Resp {
    // 可选的第 5 个参数必须是 WITHSCORES；多出来的参数一律报错，
    // 避免 `ZRANGE k 0 -1 LIMIT 0 10`（ZRANGEBYSCORE 的语法）被静默忽略。
    let with_scores = match args.len() {
        4 => false,
        5 if args[4].eq_ignore_ascii_case(b"WITHSCORES") => true,
        5 => return Resp::error(errors::SYNTAX),
        _ => return Resp::error(errors::wrong_args(if rev { "zrevrange" } else { "zrange" })),
    };
    let Ok(start) = parse_i64(&args[2]) else {
        return Resp::error(errors::NOT_INT);
    };
    let Ok(stop) = parse_i64(&args[3]) else {
        return Resp::error(errors::NOT_INT);
    };
    let items = match ctx.db.get(&args[1]) {
        None => return Resp::Array(Vec::new()),
        Some(Obj::ZSet(z)) => {
            if rev {
                z.range_rev(start, stop)
            } else {
                z.range(start, stop)
            }
        }
        Some(_) => return Resp::error(errors::WRONGTYPE),
    };
    if with_scores {
        // 成员与分值交替展开：[m1, s1, m2, s2, ...]（Redis 语义）。
        let mut out = Vec::with_capacity(items.len() * 2);
        for (m, s) in items {
            out.push(Resp::Bulk(m));
            out.push(Resp::Bulk(format_float(s).into_bytes()));
        }
        Resp::Array(out)
    } else {
        Resp::Array(items.into_iter().map(|(m, _)| Resp::Bulk(m)).collect())
    }
}

/// `ZSCORE key member`
pub fn cmd_zscore(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get(&args[1]) {
        None => Resp::Null,
        Some(Obj::ZSet(z)) => match z.score(&args[2]) {
            Some(s) => {
                ctx.note_hit();
                Resp::Bulk(format_float(s).into_bytes())
            }
            None => {
                ctx.note_miss();
                Resp::Null
            }
        },
        Some(_) => Resp::error(errors::WRONGTYPE),
    }
}

/// `ZRANK key member` —— 0 起的升序名次，不存在返回 Null。
pub fn cmd_zrank(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get(&args[1]) {
        None => Resp::Null,
        Some(Obj::ZSet(z)) => match z.rank(&args[2]) {
            Some(r) => Resp::Int(r as i64),
            None => Resp::Null,
        },
        Some(_) => Resp::error(errors::WRONGTYPE),
    }
}

/// `ZREM key member [member ...]` —— 返回删掉的成员数；删空后删键。
pub fn cmd_zrem(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let key = args[1].clone();
    let targets: Vec<Vec<u8>> = match ctx.db.get(&key) {
        None => return Resp::Int(0),
        Some(Obj::ZSet(z)) => args[2..]
            .iter()
            .filter(|m| z.score(m).is_some())
            .cloned()
            .collect(),
        Some(_) => return Resp::error(errors::WRONGTYPE),
    };
    let removed = targets.len() as i64;
    if removed == 0 {
        return Resp::Int(0);
    }
    let empty = {
        let zset = match ctx.db.ensure_zset(&key) {
            Ok(z) => z,
            Err(e) => return Resp::Error(e),
        };
        for m in &targets {
            zset.remove(m);
        }
        zset.is_empty()
    };
    if empty {
        ctx.db.remove(&key);
    }
    ctx.mark_dirty(args);
    Resp::Int(removed)
}

/// `ZCARD key`
pub fn cmd_zcard(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get(&args[1]) {
        None => Resp::Int(0),
        Some(Obj::ZSet(z)) => Resp::Int(z.len() as i64),
        Some(_) => Resp::error(errors::WRONGTYPE),
    }
}

/// `ZINCRBY key increment member` —— 返回累加后的新分值。
pub fn cmd_zincrby(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(delta) = parse_f64(&args[2]) else {
        return Resp::error(errors::NOT_FLOAT);
    };
    let key = args[1].clone();
    let member = args[3].clone();
    let zset = match ctx.db.ensure_zset(&key) {
        Ok(z) => z,
        Err(e) => return Resp::Error(e),
    };
    let next = zset.score(&member).unwrap_or(0.0) + delta;
    zset.insert(member, next);
    ctx.mark_dirty(args);
    Resp::Bulk(format_float(next).into_bytes())
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

    fn strings(resp: &Resp) -> Vec<String> {
        match resp {
            Resp::Array(items) => items.iter().filter_map(|r| r.as_string()).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn zadd_zrange_zscore() {
        let mut db = Database::new();
        assert_eq!(
            run(&mut db, &["ZADD", "z", "2", "b", "1", "a", "3", "c"]),
            Resp::Int(3)
        );
        assert_eq!(
            strings(&run(&mut db, &["ZRANGE", "z", "0", "-1"])),
            ["a", "b", "c"]
        );
        assert_eq!(
            run(&mut db, &["ZSCORE", "z", "b"]),
            Resp::Bulk(b"2".to_vec())
        );
        assert_eq!(run(&mut db, &["ZSCORE", "z", "zz"]), Resp::Null);
        assert_eq!(run(&mut db, &["ZCARD", "z"]), Resp::Int(3));
        // 更新已存在成员的分值，返回值 0 但顺序变化。
        assert_eq!(run(&mut db, &["ZADD", "z", "9", "a"]), Resp::Int(0));
        assert_eq!(
            strings(&run(&mut db, &["ZRANGE", "z", "0", "-1"])),
            ["b", "c", "a"]
        );
    }

    #[test]
    fn zrevrange_and_rank() {
        let mut db = Database::new();
        run(&mut db, &["ZADD", "z", "1", "a", "2", "b", "3", "c"]);
        assert_eq!(
            strings(&run(&mut db, &["ZREVRANGE", "z", "0", "-1"])),
            ["c", "b", "a"]
        );
        assert_eq!(run(&mut db, &["ZRANK", "z", "a"]), Resp::Int(0));
        assert_eq!(run(&mut db, &["ZRANK", "z", "c"]), Resp::Int(2));
        assert_eq!(run(&mut db, &["ZRANK", "z", "zz"]), Resp::Null);
    }

    #[test]
    fn zincrby_accumulates() {
        let mut db = Database::new();
        assert_eq!(
            run(&mut db, &["ZINCRBY", "z", "1.5", "m"]),
            Resp::Bulk(b"1.5".to_vec())
        );
        assert_eq!(
            run(&mut db, &["ZINCRBY", "z", "2.5", "m"]),
            Resp::Bulk(b"4".to_vec())
        );
        assert_eq!(
            run(&mut db, &["ZSCORE", "z", "m"]),
            Resp::Bulk(b"4".to_vec())
        );
    }

    #[test]
    fn zrem_removes_and_deletes_key() {
        let mut db = Database::new();
        run(&mut db, &["ZADD", "z", "1", "a", "2", "b"]);
        assert_eq!(run(&mut db, &["ZREM", "z", "a", "zz"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["ZREM", "z", "b"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["EXISTS", "z"]), Resp::Int(0));
    }

    #[test]
    fn zadd_rejects_bad_score() {
        let mut db = Database::new();
        let r = run(&mut db, &["ZADD", "z", "abc", "a"]);
        assert!(r.is_error());
        assert_eq!(r.as_string().unwrap(), errors::NOT_FLOAT);
        // 失败的命令不应留下部分数据。
        assert_eq!(run(&mut db, &["EXISTS", "z"]), Resp::Int(0));
    }

    #[test]
    fn wrong_type_rejected() {
        let mut db = Database::new();
        run(&mut db, &["SET", "s", "v"]);
        assert!(run(&mut db, &["ZADD", "s", "1", "a"]).is_error());
        assert!(run(&mut db, &["ZRANGE", "s", "0", "-1"]).is_error());
        assert!(run(&mut db, &["ZCARD", "s"]).is_error());
    }

    #[test]
    fn empty_zset_returns_empty_results() {
        let mut db = Database::new();
        assert_eq!(
            run(&mut db, &["ZRANGE", "zz", "0", "-1"]),
            Resp::Array(vec![])
        );
        assert_eq!(run(&mut db, &["ZCARD", "zz"]), Resp::Int(0));
        assert_eq!(run(&mut db, &["ZRANK", "zz", "a"]), Resp::Null);
        assert_eq!(run(&mut db, &["ZREM", "zz", "a"]), Resp::Int(0));
    }

    /// `WITHSCORES` 是 Web 层读热度榜的唯一途径（`store::hot_scores`）：
    /// 曾经因为 arity 固定为 4 而被拒，错误又被上层当成「无热度数据」静默吞掉，
    /// 导致 `sort=hot_desc` 与商品卡的 `hot` 字段全为 0。这条用例锁死契约。
    #[test]
    fn zrange_with_scores_returns_member_score_pairs() {
        let mut db = Database::new();
        run(&mut db, &["ZADD", "z", "1.5", "a", "2", "b"]);
        assert_eq!(
            strings(&run(&mut db, &["ZRANGE", "z", "0", "-1", "WITHSCORES"])),
            ["a", "1.5", "b", "2"]
        );
        assert_eq!(
            strings(&run(&mut db, &["ZREVRANGE", "z", "0", "-1", "WITHSCORES"])),
            ["b", "2", "a", "1.5"]
        );
        // 大小写不敏感（Redis 语义）
        assert_eq!(
            strings(&run(&mut db, &["ZRANGE", "z", "0", "-1", "withscores"])),
            ["a", "1.5", "b", "2"]
        );
        // 不带 WITHSCORES 时仍只回成员名
        assert_eq!(
            strings(&run(&mut db, &["ZRANGE", "z", "0", "-1"])),
            ["a", "b"]
        );
        // 空键 + WITHSCORES 回空数组而不是错误
        assert_eq!(
            run(&mut db, &["ZRANGE", "zz", "0", "-1", "WITHSCORES"]),
            Resp::Array(vec![])
        );
    }

    #[test]
    fn zrange_rejects_unknown_fifth_argument() {
        let mut db = Database::new();
        run(&mut db, &["ZADD", "z", "1", "a"]);
        let r = run(&mut db, &["ZRANGE", "z", "0", "-1", "LIMIT"]);
        assert_eq!(r.as_string().unwrap(), errors::SYNTAX);
        let r = run(&mut db, &["ZRANGE", "z", "0", "-1", "WITHSCORES", "extra"]);
        assert!(r.is_error());
    }
}
