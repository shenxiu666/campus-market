//! 服务生命周期命令：`SHUTDOWN`。
//!
//! 为什么需要一个专门的命令：`stop.ps1` 以前直接 `Kill()`，进程可能在
//! 「已经回了 +OK、但缓冲还没落盘」的窗口里被杀掉。`SHUTDOWN` 让停机变成
//! 一次有序操作——先 fsync，再让 accept 循环退出，最后正常返回。
//!
//! 用一个模块级原子变量而不是给整条调用链加参数：`SHUTDOWN` 是全局的，
//! `serve_client` 的签名里塞一个 `&AtomicBool` 只会污染所有命令处理器。

use crate::db::ExecCtx;
use crate::errors;
use mredis_proto::Resp;
use std::sync::atomic::{AtomicU8, Ordering};

const MODE_NONE: u8 = 0;
const MODE_SAVE: u8 = 1;
const MODE_NOSAVE: u8 = 2;

/// 全局停机状态。`serve()` 的 accept 循环每轮检查一次。
static SHUTDOWN_MODE: AtomicU8 = AtomicU8::new(MODE_NONE);

/// `SHUTDOWN [SAVE|NOSAVE]`
///
/// * 默认 / `SAVE`：先把 AOF fsync 到磁盘，再请求退出；
/// * `NOSAVE`：跳过 fsync，直接退出（写穿仍然保证进程崩溃不丢）。
pub fn cmd_shutdown(ctx: &mut ExecCtx<'_>, args: &[Vec<u8>]) -> Resp {
    let mode = match args.len() {
        1 => MODE_SAVE,
        2 => match args[1].to_ascii_uppercase().as_slice() {
            b"SAVE" => MODE_SAVE,
            b"NOSAVE" => MODE_NOSAVE,
            _ => return Resp::error(errors::SYNTAX),
        },
        _ => return Resp::error(errors::wrong_args("shutdown")),
    };

    if mode == MODE_SAVE
        && let Some(aof) = ctx.aof
        && let Ok(mut a) = aof.lock()
        && let Err(e) = a.sync()
    {
        // fsync 失败不阻止退出，但要留下痕迹。
        eprintln!("mredis-server: SHUTDOWN 前 fsync 失败: {e}");
    }

    SHUTDOWN_MODE.store(mode, Ordering::SeqCst);
    Resp::ok()
}

/// 是否已请求停机。
pub fn shutdown_requested() -> bool {
    SHUTDOWN_MODE.load(Ordering::SeqCst) != MODE_NONE
}

/// 停机时是否应跳过最终 fsync（`SHUTDOWN NOSAVE`）。
pub fn shutdown_skip_sync() -> bool {
    SHUTDOWN_MODE.load(Ordering::SeqCst) == MODE_NOSAVE
}

/// 复位状态（单元测试与嵌入式复用 `Server` 时使用）。
pub fn reset_shutdown() {
    SHUTDOWN_MODE.store(MODE_NONE, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn args(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn shutdown_sets_flag_and_replies_ok() {
        reset_shutdown();
        let mut db = Database::new();
        let mut ctx = ExecCtx::new(&mut db);
        let r = cmd_shutdown(&mut ctx, &args(&["SHUTDOWN"]));
        assert_eq!(r.as_string().as_deref(), Some("OK"));
        assert!(shutdown_requested());
        assert!(!shutdown_skip_sync());
        reset_shutdown();
    }

    #[test]
    fn nosave_records_skip_sync() {
        reset_shutdown();
        let mut db = Database::new();
        let mut ctx = ExecCtx::new(&mut db);
        let r = cmd_shutdown(&mut ctx, &args(&["SHUTDOWN", "nosave"]));
        assert_eq!(r.as_string().as_deref(), Some("OK"));
        assert!(shutdown_requested());
        assert!(shutdown_skip_sync());
        reset_shutdown();
    }

    #[test]
    fn rejects_unknown_argument_and_extra_args() {
        reset_shutdown();
        let mut db = Database::new();
        let mut ctx = ExecCtx::new(&mut db);
        let r = cmd_shutdown(&mut ctx, &args(&["SHUTDOWN", "NOW"]));
        assert!(r.is_error());
        let r = cmd_shutdown(&mut ctx, &args(&["SHUTDOWN", "SAVE", "X"]));
        assert!(r.is_error());
        assert!(!shutdown_requested(), "非法调用不应触发停机");
    }
}
