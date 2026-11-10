//! `INFO` 输出组装。
//!
//! 格式严格按 SPEC 5.2：每个小节以 `# 名称` 开头，之后是若干 `key:value` 行。
//! Web 端（`mredis-web/src/stats.rs`）直接按行 `split_once(':')` 解析，
//! 所以这里既不能多空行、也不能让值里出现换行。

use crate::db::Database;
use crate::stats::Stats;
use std::sync::atomic::Ordering;

/// 组装 `INFO` 文本。
///
/// `aof` 为 `None` 表示未开启 AOF，此时 `aof_enabled:0`、`aof_size:0`。
/// `now_ms` 由调用方传入，避免同一份输出里出现两个不同的「现在」。
pub fn render_info(
    stats: &Stats,
    db: &Database,
    aof: Option<&crate::aof::Aof>,
    now_ms: u64,
) -> String {
    let mut out = String::with_capacity(1024);

    out.push_str("# Server\r\n");
    out.push_str(&format!("mredis_version:{}\r\n", crate::VERSION));
    out.push_str(&format!(
        "uptime_in_seconds:{}\r\n",
        stats.uptime_secs(now_ms)
    ));

    out.push_str("# Clients\r\n");
    out.push_str(&format!(
        "connected_clients:{}\r\n",
        stats.connected_clients.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "total_connections_received:{}\r\n",
        stats.total_connections.load(Ordering::Relaxed)
    ));

    out.push_str("# Stats\r\n");
    out.push_str(&format!(
        "total_commands_processed:{}\r\n",
        stats.total_commands.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "keyspace_hits:{}\r\n",
        stats.keyspace_hits.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "keyspace_misses:{}\r\n",
        stats.keyspace_misses.load(Ordering::Relaxed)
    ));

    out.push_str("# Keyspace\r\n");
    out.push_str(&format!(
        "db0:keys={},expires={}\r\n",
        db.data.len(),
        db.expires.len()
    ));

    // ---- 自研哈希表的内窥数据，演示面板靠它画桶分布柱状图 ----
    out.push_str("# Dict\r\n");
    out.push_str(&format!("dict_buckets:{}\r\n", db.data.bucket_count()));
    out.push_str(&format!(
        "dict_rehashing:{}\r\n",
        u8::from(db.data.is_rehashing())
    ));
    let dist = db
        .data
        .bucket_distribution()
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    out.push_str(&format!("dict_distribution:{dist}\r\n"));

    let rec = db.rec.stats();
    out.push_str("# Rec\r\n");
    out.push_str(&format!("rec_products:{}\r\n", rec.products));
    out.push_str(&format!("rec_users:{}\r\n", rec.users));
    out.push_str(&format!("rec_tags:{}\r\n", rec.tags));
    out.push_str(&format!("rec_cooccur:{}\r\n", rec.co_occurrence_pairs));
    out.push_str(&format!("rec_events:{}\r\n", rec.events));

    out.push_str("# AOF\r\n");
    match aof {
        Some(aof) => {
            out.push_str("aof_enabled:1\r\n");
            out.push_str(&format!("aof_size:{}\r\n", aof.size()));
            out.push_str(&format!("aof_cmds:{}\r\n", aof.cmd_count()));
            out.push_str(&format!("aof_policy:{}\r\n", aof.policy().as_str()));
        }
        None => {
            out.push_str("aof_enabled:0\r\n");
            out.push_str("aof_size:0\r\n");
            out.push_str(&format!(
                "aof_cmds:{}\r\n",
                stats.aof_cmds.load(Ordering::Relaxed)
            ));
            out.push_str("aof_policy:no\r\n");
        }
    }

    // ---- 最近命令：最新在前，最多 50 条 ----
    out.push_str("# Recent\r\n");
    for line in stats.recent_commands() {
        out.push_str("recent_cmd:");
        out.push_str(&line);
        out.push_str("\r\n");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FsyncPolicy;

    /// 把 INFO 文本切成 `(小节名, [(key, value)])`，模拟 Web 端解析。
    fn parse(text: &str) -> Vec<(String, Vec<(String, String)>)> {
        let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for raw in text.lines() {
            let line = raw.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix("# ") {
                sections.push((name.to_string(), Vec::new()));
            } else if let Some((k, v)) = line.split_once(':') {
                if let Some((_, fields)) = sections.last_mut() {
                    fields.push((k.to_string(), v.to_string()));
                }
            }
        }
        sections
    }

    fn field<'a>(
        sections: &'a [(String, Vec<(String, String)>)],
        name: &str,
        key: &str,
    ) -> Option<&'a str> {
        sections
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, fs)| fs.iter().find(|(k, _)| k == key))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn renders_all_required_sections_in_order() {
        let stats = Stats::new(0);
        let db = Database::new();
        let text = render_info(&stats, &db, None, 5_000);
        let sections = parse(&text);
        let names: Vec<&str> = sections.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "Server", "Clients", "Stats", "Keyspace", "Dict", "Rec", "AOF", "Recent"
            ]
        );
        assert_eq!(
            field(&sections, "Server", "mredis_version"),
            Some(crate::VERSION)
        );
        assert_eq!(field(&sections, "Server", "uptime_in_seconds"), Some("5"));
        assert_eq!(
            field(&sections, "Keyspace", "db0"),
            Some("keys=0,expires=0")
        );
        assert_eq!(field(&sections, "Dict", "dict_rehashing"), Some("0"));
        assert_eq!(field(&sections, "AOF", "aof_enabled"), Some("0"));
        assert_eq!(field(&sections, "AOF", "aof_policy"), Some("no"));
    }

    #[test]
    fn dict_distribution_matches_bucket_count() {
        let stats = Stats::new(0);
        let mut db = Database::new();
        for i in 0..40 {
            db.set(
                format!("k{i}").into_bytes(),
                crate::obj::Obj::Str(b"v".to_vec()),
            );
        }
        let text = render_info(&stats, &db, None, 0);
        let sections = parse(&text);
        let buckets: usize = field(&sections, "Dict", "dict_buckets")
            .unwrap()
            .parse()
            .unwrap();
        let dist = field(&sections, "Dict", "dict_distribution").unwrap();
        let counts: Vec<usize> = dist.split(',').map(|v| v.parse().unwrap()).collect();
        assert_eq!(counts.len(), buckets);
        assert_eq!(counts.iter().sum::<usize>(), 40);
    }

    #[test]
    fn recent_commands_are_newest_first_and_single_line() {
        let stats = Stats::new(0);
        let db = Database::new();
        stats.record_command(&[b"set".to_vec(), b"k".to_vec(), b"a\nb".to_vec()]);
        stats.record_command(&[b"REC.FOR_YOU".to_vec(), b"1".to_vec(), b"10".to_vec()]);
        let text = render_info(&stats, &db, None, 0);
        let sections = parse(&text);
        let recent: Vec<&str> = sections
            .iter()
            .find(|(n, _)| n == "Recent")
            .map(|(_, fs)| fs.iter().map(|(_, v)| v.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(recent, ["REC.FOR_YOU 1 10", "SET k a b"]);
    }

    #[test]
    fn aof_section_reflects_enabled_state() {
        let path = crate::testutil::temp_file("info-aof", "info.aof");
        let mut aof = crate::aof::Aof::open(&path, FsyncPolicy::Always).expect("打开 AOF");
        aof.append(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])
            .expect("追加");
        aof.flush().expect("刷盘");
        let stats = Stats::new(0);
        let db = Database::new();
        let text = render_info(&stats, &db, Some(&aof), 0);
        let sections = parse(&text);
        assert_eq!(field(&sections, "AOF", "aof_enabled"), Some("1"));
        assert_eq!(field(&sections, "AOF", "aof_cmds"), Some("1"));
        assert_eq!(field(&sections, "AOF", "aof_policy"), Some("always"));
        assert!(
            field(&sections, "AOF", "aof_size")
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_value_contains_line_breaks() {
        let stats = Stats::new(0);
        let mut db = Database::new();
        // 键和命令参数里都塞进换行符，验证它们不会破坏「一行一个字段」的格式。
        db.set(b"weird\nkey".to_vec(), crate::obj::Obj::Str(b"v".to_vec()));
        stats.record_command(&[b"SET".to_vec(), b"a\r\nb".to_vec()]);
        let text = render_info(&stats, &db, None, 0);
        for line in text.lines() {
            if line.starts_with('#') {
                // 小节标题行形如 `# Server`，本来就没有冒号。
                continue;
            }
            assert!(line.contains(':'), "字段行应含冒号: {line:?}");
        }
        // 每个 `\r\n` 恰好切出一行（`lines()` 会吃掉 `\r`），
        // 说明没有任何值里混进了裸换行。
        assert_eq!(text.lines().count(), text.matches("\r\n").count());
        assert!(!text.contains('\n') || text.matches("\r\n").count() == text.matches('\n').count());
    }
}
