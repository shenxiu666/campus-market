//! 运行期统计：命中率、命令计数、连接数。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// 最近命令流保留的条数上限（SPEC 5.2：最多 50 条）。
pub const RECENT_CAP: usize = 50;

/// 全局统计计数器。
#[derive(Debug, Default)]
pub struct Stats {
    pub total_commands: AtomicU64,
    pub keyspace_hits: AtomicU64,
    pub keyspace_misses: AtomicU64,
    pub connected_clients: AtomicU64,
    pub total_connections: AtomicU64,
    pub expired_keys: AtomicU64,
    pub aof_cmds: AtomicU64,
    pub started_at_ms: AtomicU64,
    /// 最近命令流（环形，最新在前）。用 `Mutex` 而不是原子：它是一条完整命令
    /// 的一行文本，需要按顺序整体读写，用锁比无锁队列简单得多。
    recent: Mutex<VecDeque<String>>,
}

impl Stats {
    pub fn new(now_ms: u64) -> Self {
        let s = Stats::default();
        s.started_at_ms.store(now_ms, Ordering::Relaxed);
        s
    }

    pub fn incr_command(&self) {
        self.total_commands.fetch_add(1, Ordering::Relaxed);
    }

    pub fn incr_hit(&self) {
        self.keyspace_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn incr_miss(&self) {
        self.keyspace_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn client_connected(&self) {
        self.connected_clients.fetch_add(1, Ordering::Relaxed);
        self.total_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn client_disconnected(&self) {
        self.connected_clients.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn uptime_secs(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.started_at_ms.load(Ordering::Relaxed)) / 1000
    }

    /// 命中率（0..=1）；无查询时为 0。
    pub fn hit_rate(&self) -> f64 {
        let hit = self.keyspace_hits.load(Ordering::Relaxed) as f64;
        let miss = self.keyspace_misses.load(Ordering::Relaxed) as f64;
        if hit + miss == 0.0 {
            0.0
        } else {
            hit / (hit + miss)
        }
    }

    /// 记录一条最近命令（供 `INFO` 的 `# Recent` 小节）。
    ///
    /// 只保留命令名与前几个参数：`INFO` 是给演示面板看的，整条 `HSET` 的长
    /// 参数列表会把屏幕刷满，而且可能把二进制值写进文本输出。
    pub fn record_command(&self, args: &[Vec<u8>]) {
        let line = format_command(args);
        if line.is_empty() {
            return;
        }
        if let Ok(mut recent) = self.recent.lock() {
            // 最新在前：从队首插入，超出上限从队尾丢。
            recent.push_front(line);
            while recent.len() > RECENT_CAP {
                recent.pop_back();
            }
        }
    }

    /// 最近命令（最新在前）。
    pub fn recent_commands(&self) -> Vec<String> {
        match self.recent.lock() {
            Ok(recent) => recent.iter().cloned().collect(),
            // 锁中毒说明另一个线程 panic 了，此时命令流不重要，返回空即可，
            // 不能让 INFO 因此失败。
            Err(_) => Vec::new(),
        }
    }
}

/// 把一条命令格式化成单行文本：命令名大写，参数原样，最多取前 4 个参数。
///
/// 换行/回车/制表符一律替换成空格——`INFO` 是 `key:value` 的行协议，
/// 值里出现换行会破坏整个小节结构（SPEC 5.2 明确要求值内不得含换行）。
pub fn format_command(args: &[Vec<u8>]) -> String {
    const MAX_ARGS: usize = 5;
    let mut out = String::new();
    for (i, arg) in args.iter().take(MAX_ARGS).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let text = String::from_utf8_lossy(arg);
        let text = if i == 0 {
            text.to_ascii_uppercase()
        } else {
            text.into_owned()
        };
        for ch in text.chars() {
            out.push(if ch == '\n' || ch == '\r' || ch == '\t' {
                ' '
            } else {
                ch
            });
        }
    }
    if args.len() > MAX_ARGS {
        out.push_str(" ...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn counters_and_uptime() {
        let s = Stats::new(1_000);
        assert_eq!(s.uptime_secs(4_500), 3);
        s.incr_hit();
        s.incr_hit();
        s.incr_miss();
        assert!((s.hit_rate() - 2.0 / 3.0).abs() < 1e-9);
        s.client_connected();
        s.client_connected();
        s.client_disconnected();
        assert_eq!(s.connected_clients.load(Ordering::Relaxed), 1);
        assert_eq!(s.total_connections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn hit_rate_is_zero_without_queries() {
        assert_eq!(Stats::new(0).hit_rate(), 0.0);
    }

    #[test]
    fn recent_commands_are_newest_first_and_capped() {
        let s = Stats::new(0);
        for i in 0..(RECENT_CAP + 7) {
            s.record_command(&args(&["set", &format!("k{i}"), "v"]));
        }
        let recent = s.recent_commands();
        assert_eq!(recent.len(), RECENT_CAP);
        assert!(recent[0].starts_with("SET k56"));
        assert!(recent[RECENT_CAP - 1].starts_with("SET k7"));
    }

    #[test]
    fn format_command_is_single_line_and_truncated() {
        let line = format_command(&args(&["hset", "k", "a\nb", "c\r\td", "e", "f", "g"]));
        assert!(!line.contains('\n') && !line.contains('\r') && !line.contains('\t'));
        assert!(line.starts_with("HSET k a b"));
        assert!(line.ends_with("..."));
        // 空命令不产生记录。
        assert_eq!(format_command(&[]), "");
    }
}
