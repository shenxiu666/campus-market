//! 列表命令。
//!
//! 底层是 `VecDeque`：`LPUSH`/`RPUSH`/`LPOP`/`RPOP` 都是均摊 O(1)，
//! `LRANGE`/`LTRIM` 按区间操作。Redis 用 quicklist（链表套 ziplist）是为了
//! 兼顾内存与「两头操作」，教学项目用 `VecDeque` 已能体现同样的复杂度取舍。

use crate::db::ExecCtx;
use crate::errors;
use crate::obj::Obj;
use mredis_proto::Resp;

/// `LPUSH key value [value ...]` —— 依次从左边压入，返回新长度。
pub fn cmd_lpush(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    push(ctx, args, true)
}

/// `RPUSH key value [value ...]`
pub fn cmd_rpush(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    push(ctx, args, false)
}

fn push(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>], left: bool) -> Resp {
    let key = args[1].clone();
    let list = match ctx.db.ensure_list(&key) {
        Ok(l) => l,
        Err(e) => return Resp::Error(e),
    };
    for v in &args[2..] {
        if left {
            list.push_front(v.clone());
        } else {
            list.push_back(v.clone());
        }
    }
    let len = list.len() as i64;
    ctx.mark_dirty(args);
    Resp::Int(len)
}

/// `LPOP key`
pub fn cmd_lpop(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    pop(ctx, args, true)
}

/// `RPOP key`
pub fn cmd_rpop(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    pop(ctx, args, false)
}

fn pop(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>], left: bool) -> Resp {
    let key = args[1].clone();
    let (popped, empty) = match ctx.db.get_mut(&key) {
        None => return Resp::Null,
        Some(Obj::List(l)) => {
            let v = if left { l.pop_front() } else { l.pop_back() };
            (v, l.is_empty())
        }
        Some(_) => return Resp::error(errors::WRONGTYPE),
    };
    if empty {
        // 列表空了就删键，与 Redis 行为一致。
        ctx.db.remove(&key);
    }
    match popped {
        Some(v) => {
            ctx.mark_dirty(args);
            Resp::Bulk(v)
        }
        None => Resp::Null,
    }
}

/// `LRANGE key start stop` —— 负下标从尾部算，越界自动裁剪。
pub fn cmd_lrange(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(start) = crate::db::parse_i64(&args[2]) else {
        return Resp::error(errors::NOT_INT);
    };
    let Ok(stop) = crate::db::parse_i64(&args[3]) else {
        return Resp::error(errors::NOT_INT);
    };
    let list = match ctx.db.get(&args[1]) {
        None => return Resp::Array(Vec::new()),
        Some(Obj::List(l)) => l,
        Some(_) => return Resp::error(errors::WRONGTYPE),
    };
    let len = list.len() as i64;
    let Some((start, stop)) = normalize_range(start, stop, len) else {
        return Resp::Array(Vec::new());
    };
    let out = list
        .iter()
        .skip(start)
        .take(stop - start + 1)
        .cloned()
        .map(Resp::Bulk)
        .collect();
    Resp::Array(out)
}

/// `LLEN key`
pub fn cmd_llen(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get(&args[1]) {
        None => Resp::Int(0),
        Some(Obj::List(l)) => Resp::Int(l.len() as i64),
        Some(_) => Resp::error(errors::WRONGTYPE),
    }
}

/// `LTRIM key start stop` —— 只保留区间内的元素，其余丢弃。
pub fn cmd_ltrim(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let Ok(start) = crate::db::parse_i64(&args[2]) else {
        return Resp::error(errors::NOT_INT);
    };
    let Ok(stop) = crate::db::parse_i64(&args[3]) else {
        return Resp::error(errors::NOT_INT);
    };
    let key = args[1].clone();
    let (empty, trimmed) = match ctx.db.get_mut(&key) {
        None => return Resp::ok(),
        Some(Obj::List(l)) => {
            let len = l.len() as i64;
            match normalize_range(start, stop, len) {
                None => {
                    l.clear();
                    (true, true)
                }
                Some((start, stop)) => {
                    l.drain(..start);
                    l.truncate(stop - start + 1);
                    (l.is_empty(), true)
                }
            }
        }
        Some(_) => return Resp::error(errors::WRONGTYPE),
    };
    if empty {
        ctx.db.remove(&key);
    }
    if trimmed {
        ctx.mark_dirty(args);
    }
    Resp::ok()
}

/// 把可能为负的下标换算成 `[0, len)` 区间；完全越界返回 `None`。
pub(crate) fn normalize_range(start: i64, stop: i64, len: i64) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let start = if start < 0 { len + start } else { start };
    let stop = if stop < 0 { len + stop } else { stop };
    let start = start.max(0);
    let stop = stop.min(len - 1);
    if start > stop || start >= len {
        None
    } else {
        Some((start as usize, stop as usize))
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

    fn strings(resp: &Resp) -> Vec<String> {
        match resp {
            Resp::Array(items) => items.iter().filter_map(|r| r.as_string()).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn push_pop_both_ends() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["RPUSH", "l", "a", "b"]), Resp::Int(2));
        assert_eq!(run(&mut db, &["LPUSH", "l", "z"]), Resp::Int(3));
        assert_eq!(
            strings(&run(&mut db, &["LRANGE", "l", "0", "-1"])),
            ["z", "a", "b"]
        );
        assert_eq!(run(&mut db, &["LPOP", "l"]), Resp::Bulk(b"z".to_vec()));
        assert_eq!(run(&mut db, &["RPOP", "l"]), Resp::Bulk(b"b".to_vec()));
        assert_eq!(run(&mut db, &["LLEN", "l"]), Resp::Int(1));
    }

    #[test]
    fn pop_empties_key() {
        let mut db = Database::new();
        run(&mut db, &["RPUSH", "l", "only"]);
        assert_eq!(run(&mut db, &["LPOP", "l"]), Resp::Bulk(b"only".to_vec()));
        assert_eq!(run(&mut db, &["EXISTS", "l"]), Resp::Int(0));
        assert_eq!(run(&mut db, &["LPOP", "l"]), Resp::Null);
    }

    #[test]
    fn lrange_negative_and_out_of_bounds() {
        let mut db = Database::new();
        run(&mut db, &["RPUSH", "l", "a", "b", "c", "d"]);
        assert_eq!(
            strings(&run(&mut db, &["LRANGE", "l", "1", "2"])),
            ["b", "c"]
        );
        assert_eq!(
            strings(&run(&mut db, &["LRANGE", "l", "-2", "-1"])),
            ["c", "d"]
        );
        assert_eq!(
            strings(&run(&mut db, &["LRANGE", "l", "0", "100"])),
            ["a", "b", "c", "d"]
        );
        assert!(strings(&run(&mut db, &["LRANGE", "l", "5", "10"])).is_empty());
        assert!(strings(&run(&mut db, &["LRANGE", "zz", "0", "-1"])).is_empty());
    }

    #[test]
    fn ltrim_keeps_window() {
        let mut db = Database::new();
        run(&mut db, &["RPUSH", "l", "a", "b", "c", "d", "e"]);
        assert_eq!(run(&mut db, &["LTRIM", "l", "1", "3"]), Resp::ok());
        assert_eq!(
            strings(&run(&mut db, &["LRANGE", "l", "0", "-1"])),
            ["b", "c", "d"]
        );
        // 完全越界的 LTRIM 会清空并删键。
        run(&mut db, &["LTRIM", "l", "10", "20"]);
        assert_eq!(run(&mut db, &["EXISTS", "l"]), Resp::Int(0));
    }

    #[test]
    fn wrong_type_rejected() {
        let mut db = Database::new();
        run(&mut db, &["SET", "s", "v"]);
        assert!(run(&mut db, &["LPUSH", "s", "x"]).is_error());
        assert!(run(&mut db, &["LRANGE", "s", "0", "-1"]).is_error());
        assert!(run(&mut db, &["LLEN", "s"]).is_error());
    }

    #[test]
    fn normalize_range_edges() {
        assert_eq!(normalize_range(0, -1, 3), Some((0, 2)));
        assert_eq!(normalize_range(-100, 100, 3), Some((0, 2)));
        assert_eq!(normalize_range(2, 1, 3), None);
        assert_eq!(normalize_range(0, 0, 0), None);
        assert_eq!(normalize_range(-1, -1, 5), Some((4, 4)));
    }

    #[test]
    fn events_list_is_capped_by_ltrim() {
        // 模拟 `REC.TRACK` 的 `LPUSH + LTRIM 0 199` 组合。
        let mut db = Database::new();
        for i in 0..250 {
            run(&mut db, &["LPUSH", "user:1:events", &format!("e{i}")]);
            run(&mut db, &["LTRIM", "user:1:events", "0", "199"]);
        }
        assert_eq!(run(&mut db, &["LLEN", "user:1:events"]), Resp::Int(200));
        // 最新的在最前面。
        let head = strings(&run(&mut db, &["LRANGE", "user:1:events", "0", "0"]));
        assert_eq!(head, ["e249"]);
    }
}
