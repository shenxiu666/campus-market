//! `mredis-cli` —— mini-redis 的交互式命令行客户端。
//!
//! 两种工作模式：
//!
//! * **交互模式**：直接运行 `mredis-cli`，提示符 `127.0.0.1:6380> `，
//!   支持 `help` / `clear` / `quit` / `exit` 等本地命令。
//! * **一次性模式**：`mredis-cli -h 127.0.0.1 -p 6380 GET foo`，
//!   发一条命令、打印回包、退出。
//!
//! 回包按 RESP 类型着色（`Simple` 绿、`Error` 红、`Int` 青、`Bulk` 白、
//! `Array` 逐行编号、`Null` 灰）。只有在 stdout 是终端且未设置 `NO_COLOR`
//! 时才输出 ANSI 转义序列，重定向到文件或管道时自动降级为纯文本。

use mredis_client::{Client, ClientError};
use mredis_proto::Resp;
use std::io::{self, BufRead, IsTerminal, Write};

/// 默认主机。
const DEFAULT_HOST: &str = "127.0.0.1";
/// 默认端口。
const DEFAULT_PORT: u16 = 6380;
/// 版本号（来自 Cargo.toml）。
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `--help` 文本。
const HELP_TEXT: &str = "\
用法: mredis-cli [选项] [命令 [参数...]]

选项:
  -h, --host <HOST>   服务端地址（默认 127.0.0.1）
  -p, --port <PORT>   服务端端口（默认 6380）
      --help          显示本帮助并退出
      --version       显示版本号并退出

交互模式内置命令:
  help                显示本帮助
  clear               清屏
  quit | exit         退出

示例:
  mredis-cli
  mredis-cli GET foo
  mredis-cli -p 6399 REC.FOR_YOU 1 10
";

/// 解析后的运行模式。
enum Mode {
    /// 打印帮助后退出
    Help,
    /// 打印版本后退出
    Version,
    /// 正常执行
    Run {
        host: String,
        port: u16,
        /// 一次性模式的命令词；为空表示进入交互模式
        command: Vec<String>,
    },
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse_args(&args) {
        Ok(Mode::Help) => {
            print!("{HELP_TEXT}");
            0
        }
        Ok(Mode::Version) => {
            println!("mredis-cli {VERSION}");
            0
        }
        Ok(Mode::Run {
            host,
            port,
            command,
        }) => {
            let addr = format!("{host}:{port}");
            if command.is_empty() {
                run_repl(&addr)
            } else {
                run_once(&addr, &command)
            }
        }
        Err(msg) => {
            eprintln!("mredis-cli: {msg}");
            eprintln!("试试 'mredis-cli --help'。");
            2
        }
    };
    std::process::exit(code);
}

/// 解析命令行参数。参数非法时返回错误文本（调用方以退出码 2 结束）。
fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut host = DEFAULT_HOST.to_string();
    let mut port = DEFAULT_PORT;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--help" => return Ok(Mode::Help),
            "--version" => return Ok(Mode::Version),
            "-h" | "--host" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| format!("选项 {arg} 需要一个主机参数"))?;
                host = value.clone();
            }
            "-p" | "--port" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| format!("选项 {arg} 需要一个端口参数"))?;
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("端口号非法: {value}"))?;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("未知选项: {other}"));
            }
            _ => {
                // 第一个非选项参数起，全部当作命令与命令参数
                return Ok(Mode::Run {
                    host,
                    port,
                    command: args[i..].to_vec(),
                });
            }
        }
        i += 1;
    }
    Ok(Mode::Run {
        host,
        port,
        command: Vec::new(),
    })
}

/// 一次性模式：连接、发一条命令、打印回包。
fn run_once(addr: &str, words: &[String]) -> i32 {
    let mut client = match Client::connect(addr) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("无法连接到 {addr}: {e}");
            return 1;
        }
    };
    let palette = Palette::detect();
    let refs: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
    match client.call(&refs) {
        Ok(resp) => {
            let failed = resp.is_error();
            print_resp(&palette, &resp);
            i32::from(failed)
        }
        Err(e) => {
            eprintln!("命令执行失败: {e}");
            1
        }
    }
}

/// 交互模式。
fn run_repl(addr: &str) -> i32 {
    let mut client = match Client::connect(addr) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("无法连接到 {addr}: {e}");
            return 1;
        }
    };
    let palette = Palette::detect();
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        println!("mredis-cli {VERSION} 已连接 {addr}");
        println!("输入 help 查看帮助，quit 退出。");
    }
    let mut line = String::new();
    loop {
        if interactive {
            print!("{addr}> ");
            let _ = io::stdout().flush();
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("读取输入失败: {e}");
                return 1;
            }
        }
        let words = tokenize(line.trim());
        let Some(first) = words.first() else {
            continue;
        };
        match first.to_ascii_lowercase().as_str() {
            "quit" | "exit" => break,
            "help" => {
                print!("{HELP_TEXT}");
                continue;
            }
            "clear" => {
                if interactive {
                    // ANSI 清屏 + 光标归位；非终端下退化为空行
                    print!("\x1b[2J\x1b[H");
                    let _ = io::stdout().flush();
                }
                continue;
            }
            _ => {}
        }
        let refs: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
        match client.call(&refs) {
            Ok(resp) => print_resp(&palette, &resp),
            Err(ClientError::Io(e)) => {
                eprintln!("连接已断开: {e}");
                return 1;
            }
            Err(e) => eprintln!("错误: {e}"),
        }
    }
    if interactive {
        println!("再见。");
    }
    0
}

/// 把一行输入切成命令词，支持双引号、单引号与双引号内的 `\n` `\t` `\r` `\\` 转义。
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if in_double && c == '\\' {
            match chars.next() {
                Some('n') => cur.push('\n'),
                Some('t') => cur.push('\t'),
                Some('r') => cur.push('\r'),
                Some(other) => cur.push(other),
                None => cur.push('\\'),
            }
            started = true;
            continue;
        }
        match c {
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// 终端配色。`enabled == false` 时所有方法原样返回文本（Windows 老控制台/重定向）。
struct Palette {
    enabled: bool,
}

impl Palette {
    fn detect() -> Self {
        let is_terminal = io::stdout().is_terminal();
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            enabled: is_terminal && !no_color,
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn green(&self, text: &str) -> String {
        self.paint("32", text)
    }
    fn red(&self, text: &str) -> String {
        self.paint("31", text)
    }
    fn cyan(&self, text: &str) -> String {
        self.paint("36", text)
    }
    fn white(&self, text: &str) -> String {
        self.paint("37", text)
    }
    fn gray(&self, text: &str) -> String {
        self.paint("90", text)
    }
}

/// 按 RESP 类型着色打印一个回包。
fn print_resp(palette: &Palette, resp: &Resp) {
    match resp {
        Resp::Simple(s) => println!("{}", palette.green(s)),
        Resp::Error(s) => println!("{}", palette.red(&format!("(error) {s}"))),
        Resp::Int(n) => println!("{}", palette.cyan(&format!("(integer) {n}"))),
        Resp::Bulk(bytes) => println!("{}", palette.white(&quote(&String::from_utf8_lossy(bytes)))),
        Resp::Null => println!("{}", palette.gray("(nil)")),
        Resp::Array(items) => {
            if items.is_empty() {
                println!("{}", palette.gray("(empty array)"));
            } else {
                print_array(palette, items, 0);
            }
        }
    }
}

/// 逐行编号打印数组，嵌套数组递归缩进。
fn print_array(palette: &Palette, items: &[Resp], indent: usize) {
    let pad = " ".repeat(indent);
    for (i, item) in items.iter().enumerate() {
        let index = palette.gray(&format!("{}) ", i + 1));
        match item {
            Resp::Array(inner) => {
                if inner.is_empty() {
                    println!("{pad}{index}{}", palette.gray("(empty array)"));
                } else {
                    println!(
                        "{pad}{index}{}",
                        palette.gray(&format!("{} 项数组", inner.len()))
                    );
                    print_array(palette, inner, indent + 3);
                }
            }
            other => println!("{pad}{index}{}", scalar_text(palette, other)),
        }
    }
}

/// 数组元素的单行渲染。
fn scalar_text(palette: &Palette, resp: &Resp) -> String {
    match resp {
        Resp::Simple(s) => palette.green(s),
        Resp::Error(s) => palette.red(&format!("(error) {s}")),
        Resp::Int(n) => palette.cyan(&n.to_string()),
        Resp::Bulk(bytes) => palette.white(&quote(&String::from_utf8_lossy(bytes))),
        Resp::Null => palette.gray("(nil)"),
        Resp::Array(items) => palette.gray(&format!("{} 项数组", items.len())),
    }
}

/// 给 bulk 值加双引号（与 redis-cli 风格一致）。
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_plain_words() {
        assert_eq!(tokenize("GET foo"), vec!["GET", "foo"]);
        assert_eq!(tokenize("   "), Vec::<String>::new());
    }

    #[test]
    fn tokenize_keeps_quoted_spaces() {
        assert_eq!(
            tokenize(r#"SET k "hello world""#),
            vec!["SET", "k", "hello world"]
        );
        assert_eq!(tokenize("SET k 'a b'"), vec!["SET", "k", "a b"]);
    }

    #[test]
    fn tokenize_handles_escapes() {
        assert_eq!(tokenize(r#"SET k "a\nb""#), vec!["SET", "k", "a\nb"]);
    }

    #[test]
    fn parse_args_reads_host_and_port() {
        let args: Vec<String> = ["-h", "10.0.0.1", "-p", "6399", "PING"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_args(&args).expect("应解析成功") {
            Mode::Run {
                host,
                port,
                command,
            } => {
                assert_eq!(host, "10.0.0.1");
                assert_eq!(port, 6399);
                assert_eq!(command, vec!["PING"]);
            }
            _ => panic!("应为 Run 模式"),
        }
    }

    #[test]
    fn parse_args_rejects_unknown_option() {
        let args = vec!["--nope".to_string()];
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_args_rejects_bad_port() {
        let args = vec!["-p".to_string(), "abc".to_string()];
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn quote_escapes_specials() {
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("a\nb"), "\"a\\nb\"");
    }
}
