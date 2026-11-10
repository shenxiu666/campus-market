//! 存储访问层：所有读写都通过 RESP 命令完成，不碰服务端内存。
//!
//! 键规范见 `docs/SPEC.md` 5.1。这里的函数把「一批键」变成「一批 JSON 视图对象」，
//! 尽量用 pipeline 把 N 次往返压成 1 次。

use crate::error::ApiError;
use crate::model::{ProductJson, UserJson, product_from_hash, user_from_hash};
use crate::state::AppState;
use mredis_proto::Resp;
use std::collections::HashMap;

/// 一次 pipeline 里最多塞多少个 `HGETALL`，避免单帧过大。
const PIPE_BATCH: usize = 64;

/// 键名是否形如 `product:<纯数字>`（排除 `product:index:created` 这类辅助键）。
fn is_id_key(key: &str, prefix: &str) -> bool {
    match key.strip_prefix(prefix) {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// 从键名 `product:<id>` 里取 id。
fn id_of(key: &str, prefix: &str) -> Option<u64> {
    key.strip_prefix(prefix)?.parse::<u64>().ok()
}

/// 用 `SCAN` 遍历所有匹配 `pattern` 的键；服务端不支持 `SCAN` 时退回 `KEYS`。
pub async fn scan_keys(state: &AppState, pattern: &str) -> Result<Vec<String>, ApiError> {
    let mut keys = Vec::new();
    let mut cursor = "0".to_string();
    loop {
        let reply = state
            .call_str(&["SCAN", &cursor, "MATCH", pattern, "COUNT", "500"])
            .await?;
        match reply {
            Resp::Array(items) if items.len() == 2 => {
                let next = items[0].as_string().unwrap_or_else(|| "0".to_string());
                if let Resp::Array(batch) = &items[1] {
                    keys.extend(batch.iter().filter_map(|k| k.as_string()));
                }
                if next == "0" {
                    break;
                }
                cursor = next;
            }
            Resp::Error(_) => {
                // SCAN 未实现：退回 KEYS
                let reply = state.call_str(&["KEYS", pattern]).await?;
                if let Resp::Array(items) = reply {
                    keys.extend(items.iter().filter_map(|k| k.as_string()));
                }
                break;
            }
            _ => break,
        }
    }
    Ok(keys)
}

/// 所有商品 id（升序）。
pub async fn product_ids(state: &AppState) -> Result<Vec<u64>, ApiError> {
    let keys = scan_keys(state, "product:*").await?;
    let mut ids: Vec<u64> = keys
        .iter()
        .filter(|k| is_id_key(k, "product:"))
        .filter_map(|k| id_of(k, "product:"))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// 读取 `zset:hot` 的全部成员分数。
pub async fn hot_scores(state: &AppState) -> Result<HashMap<u64, f64>, ApiError> {
    let reply = state
        .call_str(&["ZRANGE", "zset:hot", "0", "-1", "WITHSCORES"])
        .await?;
    let mut out = HashMap::new();
    match reply {
        Resp::Array(items) => {
            // 服务端可能返回 [member, score, member, score, ...]
            let mut iter = items.iter();
            while let (Some(member), Some(score)) = (iter.next(), iter.next()) {
                if let (Some(id), Some(score)) = (
                    member.as_string().and_then(|s| s.parse::<u64>().ok()),
                    score.as_string().and_then(|s| s.parse::<f64>().ok()),
                ) {
                    out.insert(id, score);
                }
            }
        }
        // 没有 zset:hot（空库）或服务端返回错误都按「无热度数据」处理
        Resp::Null | Resp::Error(_) => {}
        _ => {}
    }
    Ok(out)
}

/// 批量读取商品 HASH，返回与 `ids` 顺序一致的列表（不存在的键对应 `None`）。
pub async fn fetch_products(
    state: &AppState,
    ids: &[u64],
) -> Result<Vec<Option<ProductJson>>, ApiError> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(PIPE_BATCH) {
        let cmds: Vec<Vec<Vec<u8>>> = chunk
            .iter()
            .map(|id| vec![b"HGETALL".to_vec(), format!("product:{id}").into_bytes()])
            .collect();
        let replies = state.pipeline(cmds).await?;
        for reply in replies {
            match reply {
                Resp::Array(items) => {
                    let flat: Vec<String> = items.iter().filter_map(|v| v.as_string()).collect();
                    out.push(product_from_hash(&flat));
                }
                _ => out.push(None),
            }
        }
    }
    Ok(out)
}

/// 读取单个商品。
pub async fn fetch_product(state: &AppState, id: u64) -> Result<Option<ProductJson>, ApiError> {
    let mut products = fetch_products(state, &[id]).await?;
    Ok(products.pop().flatten())
}

/// 读取单个商品，不存在则 404。
pub async fn require_product(state: &AppState, id: u64) -> Result<ProductJson, ApiError> {
    fetch_product(state, id)
        .await?
        .ok_or_else(|| ApiError::not_found("product not found"))
}

/// 全部商品，`hot` 字段用 `zset:hot` 填充。
pub async fn all_products(state: &AppState) -> Result<Vec<ProductJson>, ApiError> {
    let ids = product_ids(state).await?;
    let mut products: Vec<ProductJson> = fetch_products(state, &ids)
        .await?
        .into_iter()
        .flatten()
        .collect();
    let hot = hot_scores(state).await?;
    for product in &mut products {
        product.hot = hot.get(&product.id).copied().unwrap_or(0.0);
    }
    Ok(products)
}

/// 全部用户（按 id 升序）。
pub async fn all_users(state: &AppState) -> Result<Vec<UserJson>, ApiError> {
    let keys = scan_keys(state, "user:*").await?;
    let mut ids: Vec<u64> = keys.iter().filter_map(|k| id_of(k, "user:")).collect();
    ids.sort_unstable();
    ids.dedup();

    let mut users = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(PIPE_BATCH) {
        let cmds: Vec<Vec<Vec<u8>>> = chunk
            .iter()
            .map(|id| vec![b"HGETALL".to_vec(), format!("user:{id}").into_bytes()])
            .collect();
        let replies = state.pipeline(cmds).await?;
        for reply in replies {
            if let Resp::Array(items) = reply {
                let flat: Vec<String> = items.iter().filter_map(|v| v.as_string()).collect();
                users.extend(user_from_hash(&flat));
            }
        }
    }
    Ok(users)
}

/// 把 `REC.*` 返回的 JSON 数组（Bulk 承载）解析成 `Vec<serde_json::Value>`。
pub fn parse_rec_array(reply: &Resp) -> Result<Vec<serde_json::Value>, ApiError> {
    let text = match reply {
        Resp::Bulk(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Resp::Simple(s) => s.clone(),
        Resp::Error(msg) => return Err(ApiError::bad_request_plain(msg.clone())),
        Resp::Null => return Ok(Vec::new()),
        _ => {
            return Err(ApiError::unavailable());
        }
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| ApiError::unavailable())?;
    match value {
        serde_json::Value::Array(items) => Ok(items),
        serde_json::Value::Null => Ok(Vec::new()),
        _ => Err(ApiError::unavailable()),
    }
}

/// 把 `REC.*` 的单条 JSON 解析成对象。
pub fn parse_rec_object(reply: &Resp) -> Result<serde_json::Value, ApiError> {
    let text = match reply {
        Resp::Bulk(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Resp::Simple(s) => s.clone(),
        Resp::Error(msg) => return Err(ApiError::bad_request_plain(msg.clone())),
        _ => return Err(ApiError::unavailable()),
    };
    serde_json::from_str(&text).map_err(|_| ApiError::unavailable())
}

/// 把 `REC.*` 的一条结果补上完整商品信息，变成 `RecommendItem`。
pub async fn enrich_items(
    state: &AppState,
    items: Vec<serde_json::Value>,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let ids: Vec<u64> = items
        .iter()
        .filter_map(|item| item.get("product_id").and_then(|v| v.as_u64()))
        .collect();
    let mut products = fetch_products(state, &ids).await?;
    let hot = hot_scores(state).await?;
    let mut by_id: HashMap<u64, ProductJson> = HashMap::with_capacity(ids.len());
    for product in products.drain(..).flatten() {
        by_id.insert(product.id, product);
    }

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("product_id")
            .and_then(|v| v.as_u64())
            .unwrap_or_default();
        let product = by_id.get_mut(&id).map(|p| {
            p.hot = hot.get(&id).copied().unwrap_or(0.0);
            p.to_json()
        });
        let score = item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let reasons = item
            .get("reasons")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        out.push(serde_json::json!({
            "product_id": id,
            "score": (score * 10000.0).round() / 10000.0,
            "reasons": reasons,
            "product": product,
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_id_key_accepts_only_numeric_suffix() {
        assert!(is_id_key("product:12", "product:"));
        assert!(!is_id_key("product:index:created", "product:"));
        assert!(!is_id_key("product:", "product:"));
        assert!(!is_id_key("user:1", "product:"));
    }

    #[test]
    fn id_of_parses_suffix() {
        assert_eq!(id_of("product:12", "product:"), Some(12));
        assert_eq!(id_of("product:abc", "product:"), None);
        assert_eq!(id_of("nope", "product:"), None);
    }

    #[test]
    fn parse_rec_array_reads_bulk_json() {
        let reply = Resp::Bulk(br#"[{"product_id":1,"score":0.5,"reasons":[]}]"#.to_vec());
        let items = parse_rec_array(&reply).expect("应解析成功");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["product_id"], serde_json::json!(1));
    }

    #[test]
    fn parse_rec_array_maps_server_error_to_400() {
        let reply = Resp::Error("ERR bad user".into());
        let err = parse_rec_array(&reply).expect_err("应报错");
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_rec_array_rejects_garbage() {
        let reply = Resp::Bulk(b"not json".to_vec());
        let err = parse_rec_array(&reply).expect_err("应报错");
        assert_eq!(err.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
}
