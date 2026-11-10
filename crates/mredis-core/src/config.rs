//! 服务端配置（手写参数解析，不引入 clap）。

/// AOF 刷盘策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    Always,
    EverySec,
    No,
}

impl FsyncPolicy {
    pub fn parse(s: &str) -> Option<FsyncPolicy> {
        match s {
            "always" => Some(FsyncPolicy::Always),
            "everysec" => Some(FsyncPolicy::EverySec),
            "no" => Some(FsyncPolicy::No),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            FsyncPolicy::Always => "always",
            FsyncPolicy::EverySec => "everysec",
            FsyncPolicy::No => "no",
        }
    }
}

/// 服务端配置。
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub appendonly: bool,
    pub aof_path: String,
    pub appendfsync: FsyncPolicy,
    /// 种子数据文件（仅在持久化存储首次初始化时导入）
    pub seed: Option<String>,
    /// 启动时清空已有数据
    pub flush_on_start: bool,
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 6380,
            appendonly: true,
            aof_path: "data/runtime/appendonly.aof".to_string(),
            appendfsync: FsyncPolicy::EverySec,
            seed: None,
            flush_on_start: false,
            verbose: false,
        }
    }
}

/// 解析结果。
pub enum ParseOutcome {
    Run(Config),
    /// 打印帮助/版本后正常退出
    Exit(String),
}

/// 取出 `--opt <value>` 形式里的 `<value>`，并把游标推进到值上。
fn take_value(args: &[String], i: &mut usize, name: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("选项 {name} 需要一个参数"))
}

impl Config {
    /// 从命令行参数解析；失败返回错误文案。
    ///
    /// `args` **不含**程序名（`std::env::args()` 的第一个元素），
    /// 由 `mredis-server` 的 `main` 负责跳过。
    ///
    /// 手写解析而不是引入 clap：本项目只有一个二进制、十来个别名，多一个
    /// 编译期依赖不值得；而且选项名是脚本契约（见 `help()`），手写更可控。
    pub fn parse_args(args: &[String]) -> Result<ParseOutcome, String> {
        let mut cfg = Config::default();
        let mut i = 0usize;
        while i < args.len() {
            let arg = args[i].as_str();
            // 带值选项统一走 `take_value`：它把游标推到值上，循环末尾再统一 +1。
            // 未知选项直接报错而不是静默忽略，否则脚本里写错一个字母会变成
            // 「参数被吃掉、行为悄悄改变」，非常难查。
            match arg {
                "-h" | "--help" => return Ok(ParseOutcome::Exit(Config::help().to_string())),
                "-v" | "--version" => {
                    return Ok(ParseOutcome::Exit(format!(
                        "mredis-server {}",
                        crate::VERSION
                    )));
                }
                "--host" => cfg.host = take_value(args, &mut i, "--host")?,
                "--port" => {
                    let raw = take_value(args, &mut i, "--port")?;
                    cfg.port = raw
                        .parse::<u16>()
                        .map_err(|_| format!("端口必须是 0..65535 的整数，收到 '{raw}'"))?;
                }
                "--appendonly" => {
                    let raw = take_value(args, &mut i, "--appendonly")?;
                    cfg.appendonly = match raw.as_str() {
                        "yes" | "true" | "1" | "on" => true,
                        "no" | "false" | "0" | "off" => false,
                        other => {
                            return Err(format!("--appendonly 只接受 yes|no，收到 '{other}'"));
                        }
                    };
                }
                "--appendfsync" => {
                    let raw = take_value(args, &mut i, "--appendfsync")?;
                    cfg.appendfsync = FsyncPolicy::parse(&raw).ok_or_else(|| {
                        format!("--appendfsync 只接受 always|everysec|no，收到 '{raw}'")
                    })?;
                }
                "--aof-path" => cfg.aof_path = take_value(args, &mut i, "--aof-path")?,
                "--seed" => cfg.seed = Some(take_value(args, &mut i, "--seed")?),
                "--flush-on-start" => cfg.flush_on_start = true,
                "--verbose" => cfg.verbose = true,
                other => return Err(format!("未知选项 '{other}'（用 --help 查看用法）")),
            }
            i += 1;
        }
        Ok(ParseOutcome::Run(cfg))
    }

    /// `--help` 文本。
    pub fn help() -> &'static str {
        "mredis-server 0.1.0 — 校园二手交易 mini-redis

用法: mredis-server [选项]

选项:
  --host <ip>            监听地址（默认 127.0.0.1）
  --port <n>             监听端口（默认 6380；0 表示由系统分配）
  --appendonly <yes|no>  开启 AOF 持久化（默认 yes；no = 纯内存演示模式）
  --appendfsync <policy> always | everysec | no（默认 everysec）
  --aof-path <path>      AOF 文件路径（默认 data/runtime/appendonly.aof）
  --seed <path>          首次初始化持久化存储时导入种子 JSON（之后不再导入）
  --flush-on-start       清空键空间与 AOF 后重新初始化
  --verbose              打印每条命令
  -h, --help             显示本帮助
  -v, --version          显示版本"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<ParseOutcome, String> {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        Config::parse_args(&args)
    }

    fn run(argv: &[&str]) -> Config {
        match parse(argv) {
            Ok(ParseOutcome::Run(c)) => c,
            Ok(ParseOutcome::Exit(s)) => panic!("期望 Run，得到 Exit({s})"),
            Err(e) => panic!("期望 Run，得到 Err({e})"),
        }
    }

    #[test]
    fn empty_args_yield_defaults() {
        let c = run(&[]);
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 6380);
        assert!(c.appendonly, "默认可持久化：裸启动也不应是纯内存 demo");
        assert_eq!(c.appendfsync, FsyncPolicy::EverySec);
        assert!(c.seed.is_none());
        assert!(!c.flush_on_start);
        assert!(!c.verbose);
    }

    #[test]
    fn parses_every_documented_option() {
        let c = run(&[
            "--host",
            "0.0.0.0",
            "--port",
            "0",
            "--appendonly",
            "yes",
            "--appendfsync",
            "always",
            "--aof-path",
            "tmp/x.aof",
            "--seed",
            "data/seed.json",
            "--flush-on-start",
            "--verbose",
        ]);
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.port, 0);
        assert!(c.appendonly);
        assert_eq!(c.appendfsync, FsyncPolicy::Always);
        assert_eq!(c.aof_path, "tmp/x.aof");
        assert_eq!(c.seed.as_deref(), Some("data/seed.json"));
        assert!(c.flush_on_start);
        assert!(c.verbose);
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(parse(&["--help"]), Ok(ParseOutcome::Exit(_))));
        assert!(matches!(parse(&["-h"]), Ok(ParseOutcome::Exit(_))));
        assert!(matches!(parse(&["--version"]), Ok(ParseOutcome::Exit(_))));
        // help 里必须列出脚本依赖的全部选项名。
        let text = Config::help();
        for opt in [
            "--host",
            "--port",
            "--appendonly",
            "--appendfsync",
            "--aof-path",
            "--seed",
            "--flush-on-start",
            "--verbose",
        ] {
            assert!(text.contains(opt), "help 缺少 {opt}");
        }
    }

    #[test]
    fn rejects_bad_values() {
        assert!(parse(&["--port", "70000"]).is_err());
        assert!(parse(&["--port", "abc"]).is_err());
        assert!(parse(&["--appendfsync", "sometimes"]).is_err());
        assert!(parse(&["--appendonly", "maybe"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        // 缺值
        assert!(parse(&["--port"]).is_err());
        assert!(parse(&["--seed"]).is_err());
    }

    #[test]
    fn fsync_policy_roundtrip() {
        for p in [FsyncPolicy::Always, FsyncPolicy::EverySec, FsyncPolicy::No] {
            assert_eq!(FsyncPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(FsyncPolicy::parse("ALWAYS"), None);
    }
}
