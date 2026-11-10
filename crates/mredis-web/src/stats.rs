//! `INFO` 解析：把 core 输出的分节文本转成 `/api/stats` 的 JSON。
//!
//! 约定见 `docs/SPEC.md` 5.2：
//!
//! ```text
//! # Server
//! uptime_in_seconds:120
//! # Stats
//! total_commands_processed:1024
//! keyspace_hits:900
//! keyspace_misses:70
//! # Keyspace
//! db0:keys=42,expires=0
//! # Dict
//! dict_buckets:128
//! dict_rehashing:0
//! dict_distribution:0,1,0,2
//! # Rec
//! rec_products:42
//! # AOF
//! aof_enabled:1
//! # Recent
//! recent_cmd:HSET product:12 title ...
//! ```

use serde_json::{Value, json};
use std::collections::HashMap;

/// 解析后的 INFO。
#[derive(Debug, Default, Clone)]
pub struct Info {
    /// 平铺的 `key:value`（同名键保留第一次出现的值）
    fields: HashMap<String, String>,
    /// `recent_cmd:` 行，保持服务端给的顺序（最新在前）
    recent: Vec<String>,
    /// `db0:keys=42,expires=0` 里的 keys 数
    pub dbsize: u64,
}

impl Info {
    /// 解析 INFO 文本。
    pub fn parse(text: &str) -> Info {
        let mut info = Info::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if key == "recent_cmd" {
                info.recent.push(value.to_string());
                continue;
            }
            if let Some(rest) = key.strip_prefix("db") {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    info.dbsize += parse_keyspace_keys(value);
                    continue;
                }
            }
            info.fields
                .entry(key.to_string())
                .or_insert_with(|| value.to_string());
        }
        info
    }

    /// 取字符串字段。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// 取整数字段。
    pub fn int(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.trim().parse::<i64>().ok())
    }

    /// 取 `0`/`1` 布尔字段。
    pub fn bool_flag(&self, key: &str) -> Option<bool> {
        match self.int(key)? {
            0 => Some(false),
            _ => Some(true),
        }
    }

    /// `dict_distribution:0,1,2` → `[0,1,2]`。
    pub fn distribution(&self) -> Vec<u64> {
        self.get("dict_distribution")
            .map(|raw| {
                raw.split(',')
                    .filter_map(|v| v.trim().parse::<u64>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 最近命令流（最新在前）。
    pub fn recent_commands(&self) -> &[String] {
        &self.recent
    }

    /// 命中率 = hits / (hits + misses)；没有命中也没有未命中时返回 0。
    pub fn hit_rate(&self) -> f64 {
        let hits = self.int("keyspace_hits").unwrap_or(0).max(0) as f64;
        let misses = self.int("keyspace_misses").unwrap_or(0).max(0) as f64;
        let total = hits + misses;
        if total <= 0.0 { 0.0 } else { hits / total }
    }

    /// 组装 `/api/stats` 的 JSON（SPEC 4.3）。
    ///
    /// `dbsize` 由调用方通过 `DBSIZE` 命令拿到时优先用它，`None` 则退回 INFO 的
    /// `db0:keys=`。
    pub fn to_stats_json(&self, dbsize: Option<u64>) -> Value {
        let keys = dbsize.unwrap_or(self.dbsize);
        let round4 = |v: f64| (v * 10000.0).round() / 10000.0;
        json!({
            "dbsize": keys,
            "keys": keys,
            "dict": {
                "buckets": self.int("dict_buckets").unwrap_or(0),
                "rehashing": self.bool_flag("dict_rehashing").unwrap_or(false),
                "distribution": self.distribution(),
            },
            "hit_rate": round4(self.hit_rate()),
            "total_commands": self.int("total_commands_processed").unwrap_or(0),
            "uptime_secs": self.int("uptime_in_seconds").unwrap_or(0),
            "connected_clients": self.int("connected_clients").unwrap_or(0),
            "recent_commands": self.recent_commands(),
            "rec": {
                "products": self.int("rec_products").unwrap_or(0),
                "users": self.int("rec_users").unwrap_or(0),
                "tags": self.int("rec_tags").unwrap_or(0),
                "co_occurrence_pairs": self.int("rec_cooccur").unwrap_or(0),
                "events": self.int("rec_events").unwrap_or(0),
            },
            "aof": {
                "enabled": self.bool_flag("aof_enabled").unwrap_or(false),
                "size": self.int("aof_size").unwrap_or(0),
                "commands": self.int("aof_cmds").unwrap_or(0),
                "policy": self.get("aof_policy").unwrap_or("no"),
            },
        })
    }
}

/// 解析 `keys=42,expires=0` 里的 keys。
fn parse_keyspace_keys(value: &str) -> u64 {
    value
        .split(',')
        .find_map(|part| {
            let (key, val) = part.split_once('=')?;
            if key.trim() == "keys" {
                val.trim().parse::<u64>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Server
mredis_version:0.1.0
uptime_in_seconds:120
# Clients
connected_clients:1
total_connections_received:5
# Stats
total_commands_processed:1024
keyspace_hits:900
keyspace_misses:70
# Keyspace
db0:keys=42,expires=0
# Dict
dict_buckets:128
dict_rehashing:0
dict_distribution:0,1,0,2,3
# Rec
rec_products:42
rec_users:6
rec_tags:37
rec_cooccur:128
rec_events:240
# AOF
aof_enabled:1
aof_size:4096
aof_cmds:88
aof_policy:everysec
# Recent
recent_cmd:HSET product:12 title 捷安特
recent_cmd:REC.FOR_YOU 1 10
";

    #[test]
    fn parse_reads_scalar_fields() {
        let info = Info::parse(SAMPLE);
        assert_eq!(info.int("uptime_in_seconds"), Some(120));
        assert_eq!(info.int("total_commands_processed"), Some(1024));
        assert_eq!(info.get("aof_policy"), Some("everysec"));
    }

    #[test]
    fn parse_collects_recent_commands_in_order() {
        let info = Info::parse(SAMPLE);
        assert_eq!(info.recent_commands().len(), 2);
        assert!(info.recent_commands()[0].starts_with("HSET product:12"));
    }

    #[test]
    fn parse_reads_distribution_and_buckets() {
        let info = Info::parse(SAMPLE);
        assert_eq!(info.int("dict_buckets"), Some(128));
        assert_eq!(info.distribution(), vec![0, 1, 0, 2, 3]);
        assert_eq!(info.bool_flag("dict_rehashing"), Some(false));
    }

    #[test]
    fn parse_reads_keyspace_dbsize() {
        let info = Info::parse(SAMPLE);
        assert_eq!(info.dbsize, 42);
    }

    #[test]
    fn hit_rate_is_hits_over_total() {
        let info = Info::parse(SAMPLE);
        assert!((info.hit_rate() - 900.0 / 970.0).abs() < 1e-9);
        let empty = Info::parse("");
        assert_eq!(empty.hit_rate(), 0.0);
    }

    #[test]
    fn to_stats_json_matches_contract_shape() {
        let info = Info::parse(SAMPLE);
        let json = info.to_stats_json(None);
        assert_eq!(json["dbsize"], json!(42));
        assert_eq!(json["dict"]["buckets"], json!(128));
        assert_eq!(json["dict"]["distribution"], json!([0, 1, 0, 2, 3]));
        assert_eq!(json["rec"]["co_occurrence_pairs"], json!(128));
        assert_eq!(json["aof"]["enabled"], json!(true));
        assert_eq!(json["aof"]["commands"], json!(88));
        assert_eq!(json["recent_commands"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn to_stats_json_prefers_explicit_dbsize() {
        let info = Info::parse(SAMPLE);
        let json = info.to_stats_json(Some(7));
        assert_eq!(json["dbsize"], json!(7));
        assert_eq!(json["keys"], json!(7));
    }

    #[test]
    fn empty_info_yields_zeroed_stats() {
        let info = Info::parse("");
        let json = info.to_stats_json(None);
        assert_eq!(json["dbsize"], json!(0));
        assert_eq!(json["dict"]["distribution"], json!([]));
        assert_eq!(json["hit_rate"], json!(0.0));
    }

    #[test]
    fn ignores_malformed_lines() {
        let info = Info::parse("# Sec\nno-colon-here\nfoo:bar\n");
        assert_eq!(info.get("foo"), Some("bar"));
    }
}
