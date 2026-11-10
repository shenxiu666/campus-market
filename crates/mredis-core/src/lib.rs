//! # mredis-core —— mini-redis 的存储层、命令层与服务层
//!
//! 分层：
//! * `dict`  —— 自研哈希表（链地址 + 增量 rehash），对标 `redis/src/dict.c`
//! * `zset`  —— 跳表有序集合，对标 `redis/src/t_zset.c`
//! * `obj`   —— 值对象（字符串/哈希/列表/集合/有序集合）
//! * `db`    —— 键空间 + TTL + 命令分发入口
//! * `cmd_*` —— 各类命令处理器
//! * `aof`   —— AOF 持久化
//! * `server`—— TCP 监听、每连接线程、RESP 读写循环

pub mod aof;
pub mod cmd_hash;
pub mod cmd_keyspace;
pub mod cmd_list;
pub mod cmd_rec;
pub mod cmd_server;
pub mod cmd_set;
pub mod cmd_string;
pub mod cmd_zset;
pub mod command;
pub mod config;
pub mod db;
pub mod dict;
pub mod expire;
pub mod info;
pub mod obj;
pub mod recbuild;
pub mod seed;
pub mod server;
pub mod stats;
pub mod zset;

pub use db::{Database, ExecCtx};
pub use dict::Dict;
pub use obj::Obj;
pub use server::Server;
pub use stats::Stats;
pub use zset::ZSet;

/// 服务版本号，`INFO` 与 `--version` 共用。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 单测辅助（只在 `cargo test` 下编译）。
///
/// 为什么不用 `std::env::temp_dir()`：Windows 沙箱里 `%TEMP%` 对测试进程
/// 常常不可写（`PermissionDenied`），测试会因为**环境**而不是**代码**失败。
/// 统一放到 workspace 的 `target/test-tmp/` 下，它与构建产物同源，必然可写，
/// 也不会污染源码目录。
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;

    /// 建一个可写的测试目录并返回其路径。
    pub fn temp_dir(tag: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建测试目录");
        dir
    }

    /// 测试目录里的一个文件路径（不创建文件）。
    pub fn temp_file(tag: &str, name: &str) -> String {
        temp_dir(tag).join(name).to_string_lossy().into_owned()
    }
}

/// 统一错误文案，对齐 Redis 的措辞。
pub mod errors {
    /// 参数个数不对
    pub fn wrong_args(cmd: &str) -> String {
        format!("ERR wrong number of arguments for '{cmd}' command")
    }
    /// 类型不匹配
    pub const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";
    /// 不是整数
    pub const NOT_INT: &str = "ERR value is not an integer or out of range";
    /// 不是浮点数
    pub const NOT_FLOAT: &str = "ERR value is not a valid float";
    /// 语法错误
    pub const SYNTAX: &str = "ERR syntax error";
}
