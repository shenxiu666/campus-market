//! `mredis-web` —— 把 mini-redis 暴露成 HTTP + 单页界面的网关。
//!
//! ```text
//! mredis-web --http-port 8080 --redis-addr 127.0.0.1:6380
//! ```

use mredis_web::{AppState, DEFAULT_HTTP_PORT, build_router};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// 版本号。
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 默认监听地址。
const DEFAULT_HTTP_HOST: &str = "127.0.0.1";
/// 默认 RESP 地址。
const DEFAULT_REDIS_ADDR: &str = "127.0.0.1:6380";

/// `--help` 文本。
const HELP_TEXT: &str = "\
用法: mredis-web [选项]

启动 HTTP 网关与 Web 演示界面（数据全部经由 RESP 协议读取）。

选项:
      --http-port <PORT>      HTTP 监听端口（默认 8080）
      --http-host <HOST>      HTTP 监听地址（默认 127.0.0.1）
      --redis-addr <ADDR>     RESP 服务端地址（默认 127.0.0.1:6380）
      --help                  显示本帮助并退出
      --version               显示版本号并退出

示例:
  mredis-web
  mredis-web --http-port 9090 --redis-addr 127.0.0.1:6399
";

/// 运行参数。
struct Options {
    http_host: String,
    http_port: u16,
    redis_addr: String,
}

enum Parsed {
    Run(Options),
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut opts = Options {
        http_host: DEFAULT_HTTP_HOST.to_string(),
        http_port: DEFAULT_HTTP_PORT,
        redis_addr: DEFAULT_REDIS_ADDR.to_string(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" => return Ok(Parsed::Help),
            "--version" => return Ok(Parsed::Version),
            "--http-port" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "--http-port 需要一个端口参数".to_string())?;
                opts.http_port = value
                    .parse::<u16>()
                    .map_err(|_| format!("端口号非法: {value}"))?;
            }
            "--http-host" => {
                i += 1;
                opts.http_host = args
                    .get(i)
                    .ok_or_else(|| "--http-host 需要一个地址参数".to_string())?
                    .clone();
            }
            "--redis-addr" => {
                i += 1;
                opts.redis_addr = args
                    .get(i)
                    .ok_or_else(|| "--redis-addr 需要一个地址参数".to_string())?
                    .clone();
            }
            other => return Err(format!("未知选项: {other}")),
        }
        i += 1;
    }
    Ok(Parsed::Run(opts))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse_args(&args) {
        Ok(Parsed::Help) => {
            print!("{HELP_TEXT}");
            0
        }
        Ok(Parsed::Version) => {
            println!("mredis-web {VERSION}");
            0
        }
        Ok(Parsed::Run(opts)) => run(opts),
        Err(msg) => {
            eprintln!("mredis-web: {msg}");
            eprintln!("试试 'mredis-web --help'。");
            2
        }
    };
    std::process::exit(code);
}

fn run(opts: Options) -> i32 {
    let ip: IpAddr = match opts.http_host.parse() {
        Ok(ip) => ip,
        Err(_) => {
            // 允许传 localhost 之类的名字
            if opts.http_host.eq_ignore_ascii_case("localhost") {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                eprintln!("HTTP 监听地址非法: {}", opts.http_host);
                return 2;
            }
        }
    };
    let addr = SocketAddr::new(ip, opts.http_port);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("创建 tokio 运行时失败: {e}");
            return 1;
        }
    };

    runtime.block_on(async move {
        let state = AppState::new(opts.redis_addr.clone());
        let router = build_router(state);
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("无法监听 {addr}: {e}");
                return 1;
            }
        };
        println!("mredis-web {VERSION}");
        println!("RESP 服务端: {}", opts.redis_addr);
        println!("Web UI: http://{addr}");
        println!("按 Ctrl+C 退出。");
        if let Err(e) = axum::serve(listener, router).await {
            eprintln!("HTTP 服务异常退出: {e}");
            return 1;
        }
        0
    })
}
