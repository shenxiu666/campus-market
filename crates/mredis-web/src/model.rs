//! JSON 视图模型：把 RESP 的 HASH 拼成 `docs/SPEC.md` 4.1 约定的 JSON 形状。

use serde_json::{Value, json};
use std::collections::HashMap;

/// 商品 JSON（`ProductJson`）。
#[derive(Debug, Clone, Default)]
pub struct ProductJson {
    pub id: u64,
    pub title: String,
    pub desc: String,
    pub price_cents: i64,
    pub tags: Vec<String>,
    pub seller_id: u64,
    pub condition: u8,
    pub campus: String,
    pub created_at: i64,
    pub status: String,
    pub views: u64,
    pub clicks: u64,
    pub favs: u64,
    /// 热度分，来自 `zset:hot`；缺省 0
    pub hot: f64,
}

impl ProductJson {
    /// 价格（元）字符串，如 `450.00`。
    pub fn price_yuan(&self) -> String {
        let cents = self.price_cents.max(0);
        format!("{}.{:02}", cents / 100, cents % 100)
    }

    /// 序列化成契约里的 JSON 对象。
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "desc": self.desc,
            "price_cents": self.price_cents,
            "price": self.price_yuan(),
            "tags": self.tags,
            "seller_id": self.seller_id,
            "condition": self.condition,
            "campus": self.campus,
            "created_at": self.created_at,
            "status": self.status,
            "views": self.views,
            "clicks": self.clicks,
            "favs": self.favs,
            "hot": (self.hot * 10000.0).round() / 10000.0,
        })
    }
}

/// 用户 JSON（`UserJson`）。
#[derive(Debug, Clone, Default)]
pub struct UserJson {
    pub id: u64,
    pub name: String,
    pub campus: String,
    pub interests: Vec<String>,
    pub avg_price: i64,
}

impl UserJson {
    /// 序列化成契约里的 JSON 对象。
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "campus": self.campus,
            "interests": self.interests,
            "avg_price": self.avg_price,
        })
    }
}

/// 把 `HGETALL` 的扁平数组（field1, value1, field2, value2, ...）转成映射。
pub fn hash_map(flat: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::with_capacity(flat.len() / 2);
    let mut iter = flat.iter();
    while let (Some(field), Some(value)) = (iter.next(), iter.next()) {
        map.insert(field.clone(), value.clone());
    }
    map
}

/// 从 `HGETALL` 的映射里取出 `u64` 字段，缺失或非法时用默认值。
fn u64_field(map: &HashMap<String, String>, key: &str) -> u64 {
    map.get(key)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// 从 `HGETALL` 的映射里取出 `i64` 字段，缺失或非法时用默认值。
fn i64_field(map: &HashMap<String, String>, key: &str) -> i64 {
    map.get(key)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
}

/// 把逗号分隔的字符串拆成标签列表（去空白、去空项）。
pub fn split_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// 把标签列表拼成逗号分隔的字符串（存储格式）。
pub fn join_tags(tags: &[String]) -> String {
    tags.iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

/// 由 `product:<id>` 的 HASH 拼出 `ProductJson`；HASH 为空时返回 `None`。
pub fn product_from_hash(flat: &[String]) -> Option<ProductJson> {
    let map = hash_map(flat);
    if map.is_empty() {
        return None;
    }
    Some(ProductJson {
        id: u64_field(&map, "id"),
        title: map.get("title").cloned().unwrap_or_default(),
        desc: map.get("desc").cloned().unwrap_or_default(),
        price_cents: i64_field(&map, "price_cents"),
        tags: split_tags(map.get("tags").map(String::as_str).unwrap_or("")),
        seller_id: u64_field(&map, "seller_id"),
        condition: u64_field(&map, "condition").min(255) as u8,
        campus: map.get("campus").cloned().unwrap_or_default(),
        created_at: i64_field(&map, "created_at"),
        status: map
            .get("status")
            .cloned()
            .unwrap_or_else(|| "onsale".to_string()),
        views: u64_field(&map, "views"),
        clicks: u64_field(&map, "clicks"),
        favs: u64_field(&map, "favs"),
        hot: 0.0,
    })
}

/// 由 `user:<id>` 的 HASH 拼出 `UserJson`；HASH 为空时返回 `None`。
pub fn user_from_hash(flat: &[String]) -> Option<UserJson> {
    let map = hash_map(flat);
    if map.is_empty() {
        return None;
    }
    Some(UserJson {
        id: u64_field(&map, "id"),
        name: map.get("name").cloned().unwrap_or_default(),
        campus: map.get("campus").cloned().unwrap_or_default(),
        interests: split_tags(map.get("interests").map(String::as_str).unwrap_or("")),
        avg_price: i64_field(&map, "avg_price"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_map_pairs_fields() {
        let flat: Vec<String> = ["a", "1", "b", "2"].iter().map(|s| s.to_string()).collect();
        let map = hash_map(&flat);
        assert_eq!(map.get("a").map(String::as_str), Some("1"));
        assert_eq!(map.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn product_from_hash_reads_all_fields() {
        let flat: Vec<String> = [
            "id",
            "12",
            "title",
            "捷安特",
            "desc",
            "九成新",
            "price_cents",
            "45000",
            "tags",
            "自行车,运动",
            "seller_id",
            "2",
            "condition",
            "9",
            "campus",
            "前湖校区",
            "created_at",
            "1700000000",
            "status",
            "onsale",
            "views",
            "12",
            "clicks",
            "3",
            "favs",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let product = product_from_hash(&flat).expect("应解析成功");
        assert_eq!(product.id, 12);
        assert_eq!(product.tags, vec!["自行车", "运动"]);
        assert_eq!(product.price_yuan(), "450.00");
        assert_eq!(product.views, 12);
        let json = product.to_json();
        assert_eq!(json["price"], json!("450.00"));
        assert_eq!(json["condition"], json!(9));
    }

    #[test]
    fn product_from_empty_hash_is_none() {
        assert!(product_from_hash(&[]).is_none());
    }

    #[test]
    fn price_yuan_pads_cents() {
        let mut p = ProductJson {
            price_cents: 1505,
            ..ProductJson::default()
        };
        assert_eq!(p.price_yuan(), "15.05");
        p.price_cents = 100;
        assert_eq!(p.price_yuan(), "1.00");
        p.price_cents = -5;
        assert_eq!(p.price_yuan(), "0.00");
    }

    #[test]
    fn split_and_join_tags_roundtrip() {
        assert_eq!(split_tags(" 自行车 , 运动 ,, "), vec!["自行车", "运动"]);
        assert!(split_tags("").is_empty());
        assert_eq!(join_tags(&["a".into(), " b ".into()]), "a,b");
    }
}
