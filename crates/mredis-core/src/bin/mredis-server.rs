//! `mredis-server` 可执行入口。

use mredis_core::config::{Config, ParseOutcome};
use mredis_core::server::Server;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Config::parse_args(&args) {
        Ok(ParseOutcome::Exit(text)) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(ParseOutcome::Run(config)) => match Server::run(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mredis-server: {e}");
                ExitCode::FAILURE
            }
        },
        Err(msg) => {
            eprintln!("mredis-server: {msg}");
            eprintln!("使用 --help 查看用法");
            ExitCode::FAILURE
        }
    }
}
