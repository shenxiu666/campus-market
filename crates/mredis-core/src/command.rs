//! 命令表与分发。
//!
//! 每个命令声明：名字、arity（正数表示「必须正好 n 个参数」，负数表示「至少 |n| 个」）、
//! 是否写命令（决定 AOF 与 `dirty` 标记）、处理器。

use crate::db::ExecCtx;
use crate::errors;
use mredis_proto::Resp;

/// 命令处理器签名。
pub type Handler = fn(&mut ExecCtx<'_>, &[Vec<u8>]) -> Resp;

/// 命令描述。
pub struct Command {
    pub name: &'static str,
    /// 含命令名本身在内的参数个数约束。
    pub arity: i32,
    /// 是否为写命令
    pub write: bool,
    pub handler: Handler,
}

/// 全部命令（按名字查找）。
///
/// 用一张静态表而不是 `match`：`COMMAND` 命令要能原样枚举它，
/// 而且 arity/写标记是数据，集中在一处比散落在各个 handler 里更好维护。
pub fn commands() -> &'static [Command] {
    use crate::cmd_hash as h;
    use crate::cmd_keyspace as k;
    use crate::cmd_list as l;
    use crate::cmd_rec as r;
    use crate::cmd_server as sv;
    use crate::cmd_set as s;
    use crate::cmd_string as t;
    use crate::cmd_zset as z;

    static COMMANDS: &[Command] = &[
        // 连接 / 服务
        Command {
            name: "PING",
            arity: -1,
            write: false,
            handler: k::cmd_ping,
        },
        Command {
            name: "ECHO",
            arity: 2,
            write: false,
            handler: k::cmd_echo,
        },
        Command {
            name: "SELECT",
            arity: 2,
            write: false,
            handler: k::cmd_select,
        },
        Command {
            name: "INFO",
            arity: -1,
            write: false,
            handler: k::cmd_info,
        },
        Command {
            name: "DBSIZE",
            arity: 1,
            write: false,
            handler: k::cmd_dbsize,
        },
        Command {
            name: "FLUSHDB",
            arity: 1,
            write: true,
            handler: k::cmd_flushdb,
        },
        Command {
            name: "COMMAND",
            arity: -1,
            write: false,
            handler: k::cmd_command,
        },
        Command {
            name: "BGREWRITEAOF",
            arity: 1,
            write: false,
            handler: k::cmd_bgrewriteaof,
        },
        Command {
            name: "SHUTDOWN",
            arity: -1,
            write: false,
            handler: sv::cmd_shutdown,
        },
        Command {
            name: "MULTI",
            arity: 1,
            write: false,
            handler: k::cmd_multi,
        },
        Command {
            name: "EXEC",
            arity: 1,
            write: false,
            handler: k::cmd_exec,
        },
        Command {
            name: "DISCARD",
            arity: 1,
            write: false,
            handler: k::cmd_discard,
        },
        // 字符串 / 键
        Command {
            name: "SET",
            arity: -3,
            write: true,
            handler: t::cmd_set,
        },
        Command {
            name: "GET",
            arity: 2,
            write: false,
            handler: t::cmd_get,
        },
        Command {
            name: "SETNX",
            arity: 3,
            write: true,
            handler: t::cmd_setnx,
        },
        Command {
            name: "MSET",
            arity: -3,
            write: true,
            handler: t::cmd_mset,
        },
        Command {
            name: "MGET",
            arity: -2,
            write: false,
            handler: t::cmd_mget,
        },
        Command {
            name: "INCR",
            arity: 2,
            write: true,
            handler: t::cmd_incr,
        },
        Command {
            name: "DECR",
            arity: 2,
            write: true,
            handler: t::cmd_decr,
        },
        Command {
            name: "INCRBY",
            arity: 3,
            write: true,
            handler: t::cmd_incrby,
        },
        Command {
            name: "DECRBY",
            arity: 3,
            write: true,
            handler: t::cmd_decrby,
        },
        Command {
            name: "APPEND",
            arity: 3,
            write: true,
            handler: t::cmd_append,
        },
        Command {
            name: "STRLEN",
            arity: 2,
            write: false,
            handler: t::cmd_strlen,
        },
        Command {
            name: "DEL",
            arity: -2,
            write: true,
            handler: t::cmd_del,
        },
        Command {
            name: "EXISTS",
            arity: -2,
            write: false,
            handler: t::cmd_exists,
        },
        Command {
            name: "TYPE",
            arity: 2,
            write: false,
            handler: t::cmd_type,
        },
        Command {
            name: "EXPIRE",
            arity: 3,
            write: true,
            handler: t::cmd_expire,
        },
        Command {
            name: "PEXPIRE",
            arity: 3,
            write: true,
            handler: t::cmd_pexpire,
        },
        Command {
            name: "PEXPIREAT",
            arity: 3,
            write: true,
            handler: t::cmd_pexpireat,
        },
        Command {
            name: "TTL",
            arity: 2,
            write: false,
            handler: t::cmd_ttl,
        },
        Command {
            name: "PTTL",
            arity: 2,
            write: false,
            handler: t::cmd_pttl,
        },
        Command {
            name: "PERSIST",
            arity: 2,
            write: true,
            handler: t::cmd_persist,
        },
        Command {
            name: "KEYS",
            arity: 2,
            write: false,
            handler: t::cmd_keys,
        },
        Command {
            name: "SCAN",
            arity: -2,
            write: false,
            handler: t::cmd_scan,
        },
        // 哈希
        Command {
            name: "HSET",
            arity: -4,
            write: true,
            handler: h::cmd_hset,
        },
        Command {
            name: "HGET",
            arity: 3,
            write: false,
            handler: h::cmd_hget,
        },
        Command {
            name: "HMSET",
            arity: -4,
            write: true,
            handler: h::cmd_hmset,
        },
        Command {
            name: "HMGET",
            arity: -3,
            write: false,
            handler: h::cmd_hmget,
        },
        Command {
            name: "HGETALL",
            arity: 2,
            write: false,
            handler: h::cmd_hgetall,
        },
        Command {
            name: "HDEL",
            arity: -3,
            write: true,
            handler: h::cmd_hdel,
        },
        Command {
            name: "HEXISTS",
            arity: 3,
            write: false,
            handler: h::cmd_hexists,
        },
        Command {
            name: "HINCRBY",
            arity: 4,
            write: true,
            handler: h::cmd_hincrby,
        },
        Command {
            name: "HKEYS",
            arity: 2,
            write: false,
            handler: h::cmd_hkeys,
        },
        Command {
            name: "HVALS",
            arity: 2,
            write: false,
            handler: h::cmd_hvals,
        },
        Command {
            name: "HLEN",
            arity: 2,
            write: false,
            handler: h::cmd_hlen,
        },
        // 列表
        Command {
            name: "LPUSH",
            arity: -3,
            write: true,
            handler: l::cmd_lpush,
        },
        Command {
            name: "RPUSH",
            arity: -3,
            write: true,
            handler: l::cmd_rpush,
        },
        Command {
            name: "LPOP",
            arity: 2,
            write: true,
            handler: l::cmd_lpop,
        },
        Command {
            name: "RPOP",
            arity: 2,
            write: true,
            handler: l::cmd_rpop,
        },
        Command {
            name: "LRANGE",
            arity: 4,
            write: false,
            handler: l::cmd_lrange,
        },
        Command {
            name: "LLEN",
            arity: 2,
            write: false,
            handler: l::cmd_llen,
        },
        Command {
            name: "LTRIM",
            arity: 4,
            write: true,
            handler: l::cmd_ltrim,
        },
        // 集合
        Command {
            name: "SADD",
            arity: -3,
            write: true,
            handler: s::cmd_sadd,
        },
        Command {
            name: "SREM",
            arity: -3,
            write: true,
            handler: s::cmd_srem,
        },
        Command {
            name: "SMEMBERS",
            arity: 2,
            write: false,
            handler: s::cmd_smembers,
        },
        Command {
            name: "SISMEMBER",
            arity: 3,
            write: false,
            handler: s::cmd_sismember,
        },
        Command {
            name: "SCARD",
            arity: 2,
            write: false,
            handler: s::cmd_scard,
        },
        Command {
            name: "SINTER",
            arity: -3,
            write: false,
            handler: s::cmd_sinter,
        },
        Command {
            name: "SUNION",
            arity: -3,
            write: false,
            handler: s::cmd_sunion,
        },
        Command {
            name: "SDIFF",
            arity: -3,
            write: false,
            handler: s::cmd_sdiff,
        },
        // 有序集合
        Command {
            name: "ZADD",
            arity: -4,
            write: true,
            handler: z::cmd_zadd,
        },
        Command {
            name: "ZRANGE",
            // 4 = `key start stop`，5 = 末尾多一个 WITHSCORES（handler 校验）
            arity: -4,
            write: false,
            handler: z::cmd_zrange,
        },
        Command {
            name: "ZREVRANGE",
            arity: -4,
            write: false,
            handler: z::cmd_zrevrange,
        },
        Command {
            name: "ZSCORE",
            arity: 3,
            write: false,
            handler: z::cmd_zscore,
        },
        Command {
            name: "ZRANK",
            arity: 3,
            write: false,
            handler: z::cmd_zrank,
        },
        Command {
            name: "ZREM",
            arity: -3,
            write: true,
            handler: z::cmd_zrem,
        },
        Command {
            name: "ZCARD",
            arity: 2,
            write: false,
            handler: z::cmd_zcard,
        },
        Command {
            name: "ZINCRBY",
            arity: 4,
            write: true,
            handler: z::cmd_zincrby,
        },
        // 领域扩展
        Command {
            name: "REC.TRACK",
            arity: 4,
            write: true,
            handler: r::cmd_rec_track,
        },
        Command {
            name: "REC.FOR_YOU",
            arity: -2,
            write: false,
            handler: r::cmd_rec_for_you,
        },
        Command {
            name: "REC.SIMILAR",
            arity: -2,
            write: false,
            handler: r::cmd_rec_similar,
        },
        Command {
            name: "REC.HOT",
            arity: -1,
            write: false,
            handler: r::cmd_rec_hot,
        },
        Command {
            name: "REC.EXPLAIN",
            arity: 3,
            write: false,
            handler: r::cmd_rec_explain,
        },
        Command {
            name: "REC.UPSERT",
            arity: 2,
            write: true,
            handler: r::cmd_rec_upsert,
        },
        Command {
            name: "REC.REBUILD",
            arity: 1,
            write: false,
            handler: r::cmd_rec_rebuild,
        },
        Command {
            name: "REC.STATS",
            arity: 1,
            write: false,
            handler: r::cmd_rec_stats,
        },
    ];
    COMMANDS
}

/// 按名字查命令（大小写不敏感）。
pub fn lookup(name: &str) -> Option<&'static Command> {
    commands()
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
}

/// 判断参数个数是否满足 arity 约束。
///
/// 正数 = 精确匹配；负数 = 至少 `|arity|` 个。上限约束（如 `PING [msg]`）
/// 由各 handler 自己兜，因为少数命令（`PING`/`INFO`/`REC.HOT`）才有上限。
fn arity_ok(arity: i32, argc: usize) -> bool {
    if arity >= 0 {
        argc == arity as usize
    } else {
        argc >= arity.unsigned_abs() as usize
    }
}

/// 执行一条命令，返回 RESP 回包。
///
/// 未知命令返回 `-ERR unknown command '...'`，参数个数错误返回 arity 错误。
pub fn dispatch(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    // `*0` 空命令（客户端心跳/断连残留）直接忽略，不当作错误。
    if args.is_empty() {
        return Resp::Null;
    }
    let name = upper(&args[0]);
    let Some(cmd) = lookup(&name) else {
        return Resp::error(format!(
            "ERR unknown command '{}'",
            String::from_utf8_lossy(&args[0])
        ));
    };
    if !arity_ok(cmd.arity, args.len()) {
        return Resp::error(errors::wrong_args(&cmd.name.to_ascii_lowercase()));
    }

    // MULTI 之后、EXEC 之前：除事务控制命令外一律入队。
    if ctx.in_multi {
        match name.as_str() {
            "MULTI" => return Resp::error("ERR MULTI calls can not be nested"),
            "EXEC" | "DISCARD" => {}
            _ => {
                if let Some(multi) = ctx.multi.as_deref_mut() {
                    multi.queue.push(args.to_vec());
                }
                return Resp::Simple("QUEUED".to_string());
            }
        }
    }

    (cmd.handler)(ctx, args)
}

/// 命令名规范化（大写）。
pub fn upper(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, MultiState};

    fn args(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn table_has_no_duplicate_names_and_all_lowercase_errors() {
        let all = commands();
        assert!(all.len() > 50, "命令表过小: {}", all.len());
        for (i, a) in all.iter().enumerate() {
            assert_eq!(
                a.name,
                a.name.to_ascii_uppercase(),
                "命令名必须大写: {}",
                a.name
            );
            for b in &all[i + 1..] {
                assert_ne!(a.name, b.name, "命令名重复: {}", a.name);
            }
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(lookup("get").map(|c| c.name), Some("GET"));
        assert_eq!(lookup("ReC.Track").map(|c| c.name), Some("REC.TRACK"));
        assert!(lookup("nope").is_none());
    }

    #[test]
    fn empty_command_returns_null() {
        let mut db = Database::new();
        let mut ctx = ExecCtx::new(&mut db);
        assert_eq!(dispatch(&mut ctx, &[]), Resp::Null);
    }

    #[test]
    fn unknown_command_reports_name() {
        let mut db = Database::new();
        let mut ctx = ExecCtx::new(&mut db);
        let r = dispatch(&mut ctx, &args(&["FOOBAR", "x"]));
        assert_eq!(r.as_string().unwrap(), "ERR unknown command 'FOOBAR'");
    }

    #[test]
    fn arity_error_uses_lowercase_name() {
        let mut db = Database::new();
        let mut ctx = ExecCtx::new(&mut db);
        let r = dispatch(&mut ctx, &args(&["GET"]));
        assert_eq!(
            r.as_string().unwrap(),
            "ERR wrong number of arguments for 'get' command"
        );
        let r = dispatch(&mut ctx, &args(&["EXISTS"]));
        assert!(r.is_error());
    }

    #[test]
    fn write_command_sets_dirty() {
        let mut db = Database::new();
        let mut ctx = ExecCtx::new(&mut db);
        assert!(!ctx.dirty);
        dispatch(&mut ctx, &args(&["SET", "k", "v"]));
        assert!(ctx.dirty);
    }

    #[test]
    fn read_command_keeps_dirty_false() {
        let mut db = Database::new();
        let mut ctx = ExecCtx::new(&mut db);
        dispatch(&mut ctx, &args(&["GET", "k"]));
        assert!(!ctx.dirty);
    }

    #[test]
    fn multi_queues_then_exec_runs_in_order() {
        let mut db = Database::new();
        let mut multi = MultiState::new();
        let mut ctx = ExecCtx::new(&mut db).with_multi(&mut multi);

        assert_eq!(
            dispatch(&mut ctx, &args(&["MULTI"])),
            Resp::Simple("OK".to_string())
        );
        assert_eq!(
            dispatch(&mut ctx, &args(&["SET", "k", "v"])),
            Resp::Simple("QUEUED".to_string())
        );
        assert_eq!(
            dispatch(&mut ctx, &args(&["INCR", "n"])),
            Resp::Simple("QUEUED".to_string())
        );
        // 入队阶段不应真的执行。
        assert_eq!(
            dispatch(&mut ctx, &args(&["GET", "k"])),
            Resp::Simple("QUEUED".to_string())
        );

        let Resp::Array(results) = dispatch(&mut ctx, &args(&["EXEC"])) else {
            panic!("EXEC 应返回数组");
        };
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], Resp::ok());
        assert_eq!(results[1], Resp::Int(1));
        assert_eq!(results[2], Resp::Bulk(b"v".to_vec()));
        // EXEC 之后回到普通状态。
        assert_eq!(
            dispatch(&mut ctx, &args(&["GET", "k"])),
            Resp::Bulk(b"v".to_vec())
        );
    }

    #[test]
    fn multi_nested_and_exec_without_multi_are_errors() {
        let mut db = Database::new();
        let mut multi = MultiState::new();
        let mut ctx = ExecCtx::new(&mut db).with_multi(&mut multi);
        assert!(dispatch(&mut ctx, &args(&["EXEC"])).is_error());
        assert!(dispatch(&mut ctx, &args(&["DISCARD"])).is_error());
        dispatch(&mut ctx, &args(&["MULTI"]));
        assert!(dispatch(&mut ctx, &args(&["MULTI"])).is_error());
        assert_eq!(dispatch(&mut ctx, &args(&["DISCARD"])), Resp::ok());
    }

    #[test]
    fn discard_drops_queue() {
        let mut db = Database::new();
        let mut multi = MultiState::new();
        let mut ctx = ExecCtx::new(&mut db).with_multi(&mut multi);
        dispatch(&mut ctx, &args(&["MULTI"]));
        dispatch(&mut ctx, &args(&["SET", "k", "v"]));
        dispatch(&mut ctx, &args(&["DISCARD"]));
        assert_eq!(dispatch(&mut ctx, &args(&["EXISTS", "k"])), Resp::Int(0));
    }
}
