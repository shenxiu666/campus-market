//! `mredis-bench` —— 排序算法与推荐接口的现场基准。
//!
//! 两类测量：
//!
//! 1. **排序算法**：n = 1000 / 10000 / 100000，分别跑三路快排、标准库
//!    `sort_unstable_by`、二叉堆 Top-K，输出耗时对照表（纯本地计算，不依赖服务）。
//! 2. **推荐接口**：对 `GET /api/recommend` 打至少 200 次请求，统计 P50 / P99。
//!    这里手写最小 HTTP/1.1 客户端（`std::net::TcpStream`），不引入额外依赖。
//!
//! 服务没起时不会失败退出：排序部分照常输出，接口部分标注「跳过」。

use mredis_web::bench::{Row, generate, three_way_quick, top_k};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// 版本号。
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 默认 HTTP 地址。
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8080";
/// 推荐接口的默认请求次数。
const DEFAULT_ROUNDS: usize = 200;
/// 排序基准的规模档位。
const SIZES: [usize; 3] = [1_000, 10_000, 100_000];

const HELP_TEXT: &str = "\
用法: mredis-bench [选项]

测量三路快排 / 标准库排序 / Top-K 的耗时，以及推荐接口的 P50 / P99 延迟。

选项:
      --http-addr <ADDR>    HTTP 网关地址（默认 127.0.0.1:8080）
      --rounds <N>          推荐接口请求次数（默认 200，至少 200）
      --user-id <ID>        推荐接口用的用户 id（默认 1）
      --help                显示本帮助并退出
      --version             显示版本号并退出
";

struct Options {
    http_addr: String,
    rounds: usize,
    user_id: u64,
}

enum Parsed {
    Run(Options),
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut opts = Options {
        http_addr: DEFAULT_HTTP_ADDR.to_string(),
        rounds: DEFAULT_ROUNDS,
        user_id: 1,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" => return Ok(Parsed::Help),
            "--version" => return Ok(Parsed::Version),
            "--http-addr" => {
                i += 1;
                opts.http_addr = args
                    .get(i)
                    .ok_or_else(|| "--http-addr 需要一个地址参数".to_string())?
                    .clone();
            }
            "--rounds" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| "--rounds 需要一个数字参数".to_string())?;
                let rounds = raw
                    .parse::<usize>()
                    .map_err(|_| format!("--rounds 非法: {raw}"))?;
                if rounds == 0 {
                    return Err("--rounds 必须大于 0".to_string());
                }
                opts.rounds = rounds.max(DEFAULT_ROUNDS);
            }
            "--user-id" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| "--user-id 需要一个数字参数".to_string())?;
                opts.user_id = raw
                    .parse::<u64>()
                    .map_err(|_| format!("--user-id 非法: {raw}"))?;
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
            println!("mredis-bench {VERSION}");
            0
        }
        Ok(Parsed::Run(opts)) => run(&opts),
        Err(msg) => {
            eprintln!("mredis-bench: {msg}");
            eprintln!("试试 'mredis-bench --help'。");
            2
        }
    };
    std::process::exit(code);
}

fn run(opts: &Options) -> i32 {
    println!("mredis-bench {VERSION}");
    println!();
    sort_section();
    println!();
    let http_ok = recommend_section(opts);
    if !http_ok {
        println!();
        println!("提示：接口基准需要先启动 mredis-server 与 mredis-web。");
    }
    0
}

/// 三种排序算法的耗时对照表。
fn sort_section() {
    println!("=== 排序算法基准（本地，确定性随机数据） ===");
    println!(
        "{:<10} {:<20} {:>12} {:>12}",
        "n", "算法", "耗时(ms)", "相对最快"
    );
    println!("{}", "-".repeat(60));
    for n in SIZES {
        let base = generate(n);
        let k = (n / 10).max(1);

        let mut data = base.clone();
        let start = Instant::now();
        three_way_quick(&mut data);
        let quick = millis(start);

        let mut data = base.clone();
        let start = Instant::now();
        data.sort_unstable_by(Row::compare);
        let std_sort = millis(start);

        let start = Instant::now();
        let top = top_k(&base, k);
        let heap = millis(start);
        // 防止优化器把结果丢掉
        debug_assert_eq!(top.len(), k.min(n));

        let fastest = quick.min(std_sort).min(heap).max(f64::MIN_POSITIVE);
        let rows: [(&str, f64); 3] = [
            ("三路快排(自研)", quick),
            ("sort_unstable_by", std_sort),
            ("二叉堆 Top-K", heap),
        ];
        for (name, ms) in rows {
            println!(
                "{:<10} {:<20} {:>12.3} {:>11.2}x",
                n,
                format!("{name}(k={k})"),
                ms,
                ms / fastest
            );
        }
        println!("{}", "-".repeat(60));
    }
}

/// 推荐接口延迟分布。
fn recommend_section(opts: &Options) -> bool {
    println!(
        "=== 推荐接口基准（GET /api/recommend，{} 次） ===",
        opts.rounds
    );
    let path = format!("/api/recommend?user_id={}&limit=10", opts.user_id);
    // 预热一次，确认服务可达
    let warmup = match http_get(&opts.http_addr, &path) {
        Ok(resp) => resp,
        Err(e) => {
            println!("跳过：无法连接 {}（{e}）", opts.http_addr);
            return false;
        }
    };
    if warmup.status != 200 {
        println!("跳过：接口返回 HTTP {}", warmup.status);
        return false;
    }

    let mut samples: Vec<f64> = Vec::with_capacity(opts.rounds);
    let mut failures = 0usize;
    for _ in 0..opts.rounds {
        let start = Instant::now();
        match http_get(&opts.http_addr, &path) {
            Ok(resp) if resp.status == 200 => samples.push(millis(start)),
            _ => failures += 1,
        }
    }
    if samples.is_empty() {
        println!("跳过：全部请求失败");
        return false;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = percentile(&samples, 50.0);
    let p90 = percentile(&samples, 90.0);
    let p99 = percentile(&samples, 99.0);
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    println!("{:<14} {:>12}", "指标", "耗时(ms)");
    println!("{}", "-".repeat(28));
    println!("{:<14} {:>12}", "样本数", samples.len());
    println!("{:<14} {:>12.3}", "平均", mean);
    println!("{:<14} {:>12.3}", "P50", p50);
    println!("{:<14} {:>12.3}", "P90", p90);
    println!("{:<14} {:>12.3}", "P99", p99);
    println!("{:<14} {:>12.3}", "最快", samples[0]);
    println!("{:<14} {:>12.3}", "最慢", samples[samples.len() - 1]);
    if failures > 0 {
        println!("失败请求: {failures}");
    }
    true
}

/// 线性插值求分位数（`pct` 取 0..=100）。
fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (pct / 100.0) * (sorted.len() - 1) as f64;
    let low = rank.floor() as usize;
    let high = rank.ceil() as usize;
    if low == high {
        sorted[low]
    } else {
        let weight = rank - low as f64;
        sorted[low] * (1.0 - weight) + sorted[high] * weight
    }
}

/// `Instant` 差值转毫秒。
fn millis(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// 一个 HTTP 响应（只需要状态码与是否读全）。
struct HttpResponse {
    status: u16,
}

/// 手写最小 HTTP/1.1 GET：连接 → 请求 → 读到 EOF → 解析状态行。
///
/// 只为基准服务，不处理 chunked / keep-alive / 重定向；`Connection: close`
/// 让服务端在响应后关闭连接，于是「读到 EOF」就是读完了整个响应。
fn http_get(addr: &str, path: &str) -> Result<HttpResponse, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("连接失败: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nUser-Agent: mredis-bench/{VERSION}\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("写请求失败: {e}"))?;
    let mut buf = Vec::with_capacity(8192);
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("读响应失败: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "响应状态行非法".to_string())?;
    Ok(HttpResponse { status })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_interpolates() {
        let sorted = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((percentile(&sorted, 0.0) - 1.0).abs() < 1e-9);
        assert!((percentile(&sorted, 100.0) - 5.0).abs() < 1e-9);
        assert!((percentile(&sorted, 50.0) - 3.0).abs() < 1e-9);
        assert!((percentile(&sorted, 25.0) - 2.0).abs() < 1e-9);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn parse_args_reads_overrides() {
        let args: Vec<String> = [
            "--http-addr",
            "1.2.3.4:9",
            "--rounds",
            "300",
            "--user-id",
            "7",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        match parse_args(&args).expect("应解析成功") {
            Parsed::Run(opts) => {
                assert_eq!(opts.http_addr, "1.2.3.4:9");
                assert_eq!(opts.rounds, 300);
                assert_eq!(opts.user_id, 7);
            }
            _ => panic!("应为 Run"),
        }
    }

    #[test]
    fn rounds_never_below_200() {
        let args: Vec<String> = ["--rounds", "10"].iter().map(|s| s.to_string()).collect();
        match parse_args(&args).expect("应解析成功") {
            Parsed::Run(opts) => assert_eq!(opts.rounds, DEFAULT_ROUNDS),
            _ => panic!("应为 Run"),
        }
    }

    #[test]
    fn parse_args_rejects_unknown() {
        assert!(parse_args(&["--nope".to_string()]).is_err());
        assert!(parse_args(&["--rounds".to_string()]).is_err());
    }
}
