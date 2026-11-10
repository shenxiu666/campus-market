//! 键空间/服务类命令：PING、INFO、DBSIZE、FLUSHDB、COMMAND 等。

use crate::command::{self, dispatch};
use crate::db::ExecCtx;
use crate::errors;
use crate::info::render_info;
use crate::stats::Stats;
use mredis_proto::Resp;

/// `PING [message]`
pub fn cmd_ping(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let _ = ctx;
    match args.len() {
        1 => Resp::Simple("PONG".to_string()),
        2 => Resp::Bulk(args[1].clone()),
        _ => Resp::error(errors::wrong_args("ping")),
    }
}

/// `ECHO message`
pub fn cmd_echo(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let _ = ctx;
    Resp::Bulk(args[1].clone())
}

/// `SELECT index` —— 本项目只有 0 号库，选别的库按 Redis 惯例报错。
pub fn cmd_select(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let _ = ctx;
    match args[1].as_slice() {
        b"0" => Resp::ok(),
        _ => Resp::error("ERR DB index is out of range"),
    }
}

/// `INFO [section]`
///
/// 小节过滤只做「包含」判断：`INFO dict` 返回 Dict 小节，`INFO` 返回全部。
/// 这样 Web 端可以按需取一段，而不必自己切文本。
pub fn cmd_info(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    if args.len() > 2 {
        return Resp::error(errors::wrong_args("info"));
    }
    let now = crate::expire::now_ms() as u64;

    // stats 与 aof 都是可选依赖：单测里没有服务端，就用一个临时 Stats。
    let owned_stats;
    let stats: &Stats = match ctx.stats {
        Some(s) => s,
        None => {
            owned_stats = Stats::new(now);
            &owned_stats
        }
    };
    // MutexGuard 必须在 render_info 调用期间存活，所以先绑到局部变量；
    // `render_info` 要 `Option<&Aof>`，这里把 guard 解一层引用。
    let aof_guard = match ctx.aof {
        Some(a) => a.lock().ok(),
        None => None,
    };
    let aof_ref = aof_guard.as_deref();

    let text = render_info(stats, ctx.db, aof_ref, now);

    let filtered = match args.get(1) {
        None => text,
        Some(section) => {
            let want = String::from_utf8_lossy(section).to_ascii_lowercase();
            filter_section(&text, &want)
        }
    };
    Resp::Bulk(filtered.into_bytes())
}

/// 只保留指定小节（以及 `# Server` 之外的全部行）。
fn filter_section(text: &str, want: &str) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in text.lines() {
        if let Some(name) = line.trim_end_matches('\r').strip_prefix("# ") {
            keep = name.to_ascii_lowercase().contains(want);
        }
        if keep {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    if out.is_empty() {
        // 小节名不认识时返回空串，与 Redis 行为一致。
        String::new()
    } else {
        out
    }
}

/// `DBSIZE`
pub fn cmd_dbsize(ctx: &mut ExecCtx<'_>, _args: &[Vec<u8>]) -> Resp {
    Resp::Int(ctx.db.dbsize() as i64)
}

/// `FLUSHDB` —— 清空键空间与推荐索引。
pub fn cmd_flushdb(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    ctx.db.flush();
    ctx.mark_dirty(args);
    Resp::ok()
}

/// `COMMAND` —— 返回命令表的机器可读形式：`[[name, arity, [flags]], ...]`。
pub fn cmd_command(ctx: &mut ExecCtx<'_>, _args: &[Vec<u8>]) -> Resp {
    let _ = ctx;
    let list = command::commands()
        .iter()
        .map(|c| {
            let mut flags = Vec::new();
            if c.write {
                flags.push(Resp::Bulk(b"write".to_vec()));
            } else {
                flags.push(Resp::Bulk(b"readonly".to_vec()));
            }
            Resp::Array(vec![
                Resp::Bulk(c.name.as_bytes().to_vec()),
                Resp::Int(c.arity as i64),
                Resp::Array(flags),
            ])
        })
        .collect();
    Resp::Array(list)
}

/// `BGREWRITEAOF` —— 用当前键空间生成一份最小命令集，原子替换 AOF 文件。
///
/// 名字里的 "background" 是历史包袱：真正的 Redis 会 fork 子进程，
/// 这里数据量很小，直接在当前线程重写（同步）即可，语义完全一致。
pub fn cmd_bgrewriteaof(ctx: &mut ExecCtx<'_>, _args: &[Vec<u8>]) -> Resp {
    let Some(aof) = ctx.aof else {
        return Resp::error("ERR AOF is not enabled");
    };
    let cmds = crate::aof::snapshot_commands(ctx.db);
    let mut guard = match aof.lock() {
        Ok(g) => g,
        Err(_) => return Resp::error("ERR AOF lock poisoned"),
    };
    match guard.rewrite(&cmds) {
        Ok(()) => Resp::simple("Background append only file rewriting started"),
        Err(e) => Resp::error(format!("ERR rewrite failed: {e}")),
    }
}

/// `MULTI` —— 进入事务，后续命令入队。
pub fn cmd_multi(ctx: &mut ExecCtx<'_>, _args: &[Vec<u8>]) -> Resp {
    match ctx.multi.as_deref_mut() {
        Some(multi) if !multi.active => {
            multi.begin();
            ctx.in_multi = true;
            Resp::ok()
        }
        Some(_) => Resp::error("ERR MULTI calls can not be nested"),
        None => Resp::error("ERR MULTI is not supported in this context"),
    }
}

/// `EXEC` —— 顺序执行队列并返回结果数组。
///
/// 入队阶段不检查参数错误（Redis 在入队时就检查 arity 并置 `error` 标记，
/// 我们由 `dispatch` 统一检查，因此这里只需处理「队列里有命令执行失败」的情况：
/// 单条失败不影响后续命令，错误以 `Resp::Error` 形式出现在数组对应位置）。
pub fn cmd_exec(ctx: &mut ExecCtx<'_>, _args: &[Vec<u8>]) -> Resp {
    let (queue, aborted) = {
        let Some(multi) = ctx.multi.as_deref_mut() else {
            return Resp::error("ERR EXEC without MULTI");
        };
        if !multi.active {
            return Resp::error("ERR EXEC without MULTI");
        }
        let queue = std::mem::take(&mut multi.queue);
        let aborted = multi.error;
        multi.active = false;
        (queue, aborted)
    };
    ctx.in_multi = false;
    if aborted {
        return Resp::error("EXECABORT Transaction discarded because of previous errors.");
    }
    let mut out = Vec::with_capacity(queue.len());
    for cmd in queue {
        out.push(dispatch(ctx, &cmd));
    }
    Resp::Array(out)
}

/// `DISCARD` —— 放弃事务队列。
pub fn cmd_discard(ctx: &mut ExecCtx<'_>, _args: &[Vec<u8>]) -> Resp {
    match ctx.multi.as_deref_mut() {
        Some(multi) if multi.active => {
            multi.reset();
            ctx.in_multi = false;
            Resp::ok()
        }
        Some(_) => Resp::error("ERR DISCARD without MULTI"),
        None => Resp::error("ERR DISCARD without MULTI"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, MultiState};

    fn run(db: &mut Database, parts: &[&str]) -> Resp {
        let args: Vec<Vec<u8>> = parts.iter().map(|p| p.as_bytes().to_vec()).collect();
        let mut ctx = ExecCtx::new(db);
        dispatch(&mut ctx, &args)
    }

    #[test]
    fn ping_and_echo() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["PING"]), Resp::Simple("PONG".to_string()));
        assert_eq!(run(&mut db, &["PING", "hi"]), Resp::Bulk(b"hi".to_vec()));
        assert!(run(&mut db, &["PING", "a", "b"]).is_error());
        assert_eq!(run(&mut db, &["ECHO", "x"]), Resp::Bulk(b"x".to_vec()));
    }

    #[test]
    fn select_only_supports_db_zero() {
        let mut db = Database::new();
        assert_eq!(run(&mut db, &["SELECT", "0"]), Resp::ok());
        assert!(run(&mut db, &["SELECT", "1"]).is_error());
    }

    #[test]
    fn dbsize_and_flushdb() {
        let mut db = Database::new();
        run(&mut db, &["SET", "a", "1"]);
        run(&mut db, &["SET", "b", "2"]);
        assert_eq!(run(&mut db, &["DBSIZE"]), Resp::Int(2));
        assert_eq!(run(&mut db, &["FLUSHDB"]), Resp::ok());
        assert_eq!(run(&mut db, &["DBSIZE"]), Resp::Int(0));
    }

    #[test]
    fn command_lists_every_entry() {
        let mut db = Database::new();
        let Resp::Array(list) = run(&mut db, &["COMMAND"]) else {
            panic!("COMMAND 应返回数组");
        };
        assert_eq!(list.len(), command::commands().len());
        let Resp::Array(first) = &list[0] else {
            panic!("每项应是数组");
        };
        assert_eq!(first[0], Resp::Bulk(b"PING".to_vec()));
    }

    #[test]
    fn info_contains_expected_sections() {
        let mut db = Database::new();
        run(&mut db, &["SET", "k", "v"]);
        let Resp::Bulk(text) = run(&mut db, &["INFO"]) else {
            panic!("INFO 应返回 Bulk");
        };
        let text = String::from_utf8_lossy(&text);
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
            assert!(text.contains(section), "缺少小节 {section}");
        }
        assert!(text.contains("db0:keys=1,expires=0"));
        // 小节过滤
        let Resp::Bulk(only) = run(&mut db, &["INFO", "dict"]) else {
            panic!("INFO dict 应返回 Bulk");
        };
        let only = String::from_utf8_lossy(&only);
        assert!(only.contains("# Dict"));
        assert!(!only.contains("# Keyspace"));
    }

    #[test]
    fn bgrewriteaof_requires_aof() {
        let mut db = Database::new();
        let r = run(&mut db, &["BGREWRITEAOF"]);
        assert!(r.is_error());
    }

    #[test]
    fn multi_exec_with_queue_works_through_dispatch() {
        let mut db = Database::new();
        let mut multi = MultiState::new();
        {
            let mut ctx = ExecCtx::new(&mut db).with_multi(&mut multi);
            let a = |parts: &[&str]| -> Vec<Vec<u8>> {
                parts.iter().map(|p| p.as_bytes().to_vec()).collect()
            };
            assert_eq!(dispatch(&mut ctx, &a(&["MULTI"])), Resp::ok());
            assert_eq!(
                dispatch(&mut ctx, &a(&["SET", "k", "v"])),
                Resp::Simple("QUEUED".to_string())
            );
            let Resp::Array(results) = dispatch(&mut ctx, &a(&["EXEC"])) else {
                panic!("EXEC 应返回数组");
            };
            assert_eq!(results.len(), 1);
        }
        assert_eq!(run(&mut db, &["GET", "k"]), Resp::Bulk(b"v".to_vec()));
    }

    #[test]
    fn info_section_filter_unknown_returns_empty() {
        let mut db = Database::new();
        let Resp::Bulk(only) = run(&mut db, &["INFO", "nosuchsection"]) else {
            panic!("应返回 Bulk");
        };
        assert!(only.is_empty());
    }
}
