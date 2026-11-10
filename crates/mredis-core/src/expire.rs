//! TTL 相关的辅助函数（时间与随机采样）。

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 当前 Unix 秒时间戳。
pub fn now_secs() -> i64 {
    now_ms() / 1000
}
