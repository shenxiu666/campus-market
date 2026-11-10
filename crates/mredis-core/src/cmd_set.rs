//! 集合命令。
//!
//! 集合值同样是 [`crate::dict::Dict`]（`Dict<()>`）：只存成员、不存值。
//! `SINTER`/`SUNION`/`SDIFF` 都先取最小集合做遍历基准，
//! 这样复杂度是 O(最小集合大小 × 单次查找)，而不是 O(所有集合之和)。

use crate::db::ExecCtx;
use crate::errors;
use crate::obj::Obj;
use mredis_proto::Resp;

/// `SADD key member [member ...]` —— 返回真正新增的成员数。
pub fn cmd_sadd(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let key = args[1].clone();
    let set = match ctx.db.ensure_set(&key) {
        Ok(s) => s,
        Err(e) => return Resp::Error(e),
    };
    let mut added = 0i64;
    for m in &args[2..] {
        if set.insert(m.clone(), ()).is_none() {
            added += 1;
        }
    }
    if added > 0 {
        ctx.mark_dirty(args);
    }
    Resp::Int(added)
}

/// `SREM key member [member ...]` —— 返回真正删掉的成员数；删空后删键。
pub fn cmd_srem(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let key = args[1].clone();
    let targets: Vec<Vec<u8>> = match ctx.db.get(&key) {
        None => return Resp::Int(0),
        Some(Obj::Set(s)) => args[2..]
            .iter()
            .filter(|m| s.contains_key(m))
            .cloned()
            .collect(),
        Some(_) => return Resp::error(errors::WRONGTYPE),
    };
    let removed = targets.len() as i64;
    if removed == 0 {
        return Resp::Int(0);
    }
    let empty = {
        let set = match ctx.db.ensure_set(&key) {
            Ok(s) => s,
            Err(e) => return Resp::Error(e),
        };
        for m in &targets {
            set.remove(m);
        }
        set.is_empty()
    };
    if empty {
        ctx.db.remove(&key);
    }
    ctx.mark_dirty(args);
    Resp::Int(removed)
}

/// `SMEMBERS key`
pub fn cmd_smembers(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get(&args[1]) {
        None => Resp::Array(Vec::new()),
        Some(Obj::Set(s)) => Resp::Array(s.keys().cloned().map(Resp::Bulk).collect()),
        Some(_) => Resp::error(errors::WRONGTYPE),
    }
}

/// `SISMEMBER key member`
pub fn cmd_sismember(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get(&args[1]) {
        None => Resp::Int(0),
        Some(Obj::Set(s)) => Resp::Int(i64::from(s.contains_key(&args[2]))),
        Some(_) => Resp::error(errors::WRONGTYPE),
    }
}

/// `SCARD key`
pub fn cmd_scard(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    match ctx.db.get(&args[1]) {
        None => Resp::Int(0),
        Some(Obj::Set(s)) => Resp::Int(s.len() as i64),
        Some(_) => Resp::error(errors::WRONGTYPE),
    }
}

/// 取集合的成员快照（不存在的键视为空集，类型不符返回错误）。
fn snapshot(ctx: &mut ExecCtx<'_>, key: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    match ctx.db.get(key) {
        None => Ok(Vec::new()),
        Some(Obj::Set(s)) => Ok(s.keys().cloned().collect()),
        Some(_) => Err(errors::WRONGTYPE.to_string()),
    }
}

/// `SINTER key [key ...]` —— 交集。
///
/// 以最小集合为基准遍历：若以最大集合为基准，每个元素都要去查所有其他集合，
/// 总代价会大一个量级。这是 Redis `sinterGenericCommand` 的同一思路。
pub fn cmd_sinter(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let mut sets: Vec<Vec<Vec<u8>>> = Vec::with_capacity(args.len() - 1);
    for key in &args[1..] {
        match snapshot(ctx, key) {
            Ok(s) => sets.push(s),
            Err(e) => return Resp::Error(e),
        }
    }
    // 任一集合为空，交集必为空。
    if sets.iter().any(|s| s.is_empty()) {
        return Resp::Array(Vec::new());
    }
    let (base, rest) = match sets.split_first() {
        Some((b, r)) => (b, r),
        None => return Resp::Array(Vec::new()),
    };
    // 选最小的那个当基准。
    let mut smallest = 0usize;
    for (i, s) in sets.iter().enumerate() {
        if s.len() < sets[smallest].len() {
            smallest = i;
        }
    }
    let mut out = Vec::new();
    for m in &sets[smallest] {
        if sets
            .iter()
            .enumerate()
            .all(|(i, s)| i == smallest || s.iter().any(|x| x == m))
        {
            out.push(Resp::Bulk(m.clone()));
        }
    }
    let _ = (base, rest);
    Resp::Array(out)
}

/// `SUNION key [key ...]` —— 并集（去重）。
pub fn cmd_sunion(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for key in &args[1..] {
        match snapshot(ctx, key) {
            Ok(members) => {
                for m in members {
                    if seen.insert(m.clone()) {
                        out.push(Resp::Bulk(m));
                    }
                }
            }
            Err(e) => return Resp::Error(e),
        }
    }
    Resp::Array(out)
}

/// `SDIFF key [key ...]` —— 差集：第一个集合减去后面所有集合。
pub fn cmd_sdiff(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let first = match snapshot(ctx, &args[1]) {
        Ok(s) => s,
        Err(e) => return Resp::Error(e),
    };
    let mut others: Vec<Vec<Vec<u8>>> = Vec::new();
    for key in &args[2..] {
        match snapshot(ctx, key) {
            Ok(s) => others.push(s),
            Err(e) => return Resp::Error(e),
        }
    }
    let out = first
        .into_iter()
        .filter(|m| !others.iter().any(|s| s.iter().any(|x| x == m)))
        .map(Resp::Bulk)
        .collect();
    Resp::Array(out)
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

    fn sorted_strings(resp: &Resp) -> Vec<String> {
        let mut v: Vec<String> = match resp {
            Resp::Array(items) => items.iter().filter_map(|r| r.as_string()).collect(),
            _ => Vec::new(),
        };
        v.sort();
        v
    }

    #[test]
    fn sadd_srem_scard() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["SADD", "s", "a", "b", "c"]), Resp::Int(3));
        assert_eq!(run(&mut db, &["SADD", "s", "a", "d"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["SCARD", "s"]), Resp::Int(4));
        assert_eq!(run(&mut db, &["SISMEMBER", "s", "b"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["SISMEMBER", "s", "z"]), Resp::Int(0));
        assert_eq!(run(&mut db, &["SREM", "s", "a", "zz"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["SCARD", "s"]), Resp::Int(3));
        assert_eq!(run(&mut db, &["SCARD", "nope"]), Resp::Int(0));
    }

    #[test]
    fn srem_empties_key() {
        let mut db = Database::new();
        run(&mut db, &["SADD", "s", "only"]);
        assert_eq!(run(&mut db, &["SREM", "s", "only"]), Resp::Int(1));
        assert_eq!(run(&mut db, &["EXISTS", "s"]), Resp::Int(0));
    }

    #[test]
    fn smembers_returns_all() {
        let mut db = Database::new();
        run(&mut db, &["SADD", "s", "a", "b"]);
        assert_eq!(
            sorted_strings(&run(&mut db, &["SMEMBERS", "s"])),
            ["a", "b"]
        );
        assert!(sorted_strings(&run(&mut db, &["SMEMBERS", "zz"])).is_empty());
    }

    #[test]
    fn set_algebra() {
        let mut db = Database::new();
        run(&mut db, &["SADD", "a", "1", "2", "3"]);
        run(&mut db, &["SADD", "b", "2", "3", "4"]);
        run(&mut db, &["SADD", "c", "3", "4", "5"]);
        assert_eq!(
            sorted_strings(&run(&mut db, &["SINTER", "a", "b"])),
            ["2", "3"]
        );
        assert_eq!(
            sorted_strings(&run(&mut db, &["SUNION", "a", "b"])),
            ["1", "2", "3", "4"]
        );
        assert_eq!(sorted_strings(&run(&mut db, &["SDIFF", "a", "b"])), ["1"]);
        // 三集合交集
        assert_eq!(
            sorted_strings(&run(&mut db, &["SINTER", "a", "b", "c"])),
            ["3"]
        );
        // 空集参与交集 → 空
        assert!(sorted_strings(&run(&mut db, &["SINTER", "a", "zz"])).is_empty());
    }

    #[test]
    fn wrong_type_rejected() {
        let mut db = Database::new();
        run(&mut db, &["SET", "s", "v"]);
        assert!(run(&mut db, &["SADD", "s", "a"]).is_error());
        assert!(run(&mut db, &["SMEMBERS", "s"]).is_error());
        assert!(run(&mut db, &["SINTER", "s", "t"]).is_error());
    }

    #[test]
    fn tag_sets_are_reusable() {
        // 模拟 `tag:<tag>` 的用法：一个商品 id 只计一次。
        let mut db = Database::new();
        run(&mut db, &["SADD", "tag:自行车", "1", "2"]);
        run(&mut db, &["SADD", "tag:自行车", "1", "3"]);
        assert_eq!(run(&mut db, &["SCARD", "tag:自行车"]), Resp::Int(3));
    }
}
