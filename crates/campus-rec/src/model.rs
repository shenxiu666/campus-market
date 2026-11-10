//! 数据模型：商品、用户、行为事件。
//!
//! 所有结构体都实现 `serde`，因此既能从 `data/seed.json` 载入，也能直接序列化给 Web 层。

use serde::{Deserialize, Serialize};

/// 商品状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// 在售
    OnSale,
    /// 已预订
    Reserved,
    /// 已售出
    Sold,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::OnSale => "onsale",
            Status::Reserved => "reserved",
            Status::Sold => "sold",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        match s {
            "onsale" => Some(Status::OnSale),
            "reserved" => Some(Status::Reserved),
            "sold" => Some(Status::Sold),
            _ => None,
        }
    }
}

/// 商品。价格一律用「分」为单位的整数，避免浮点误差。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Product {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub desc: String,
    /// 价格（分）
    pub price_cents: i64,
    #[serde(default)]
    pub tags: Vec<String>,
    pub seller_id: u64,
    #[serde(default)]
    pub condition: u8,
    #[serde(default)]
    pub campus: String,
    /// 发布时间（Unix 秒）
    #[serde(default)]
    pub created_at: i64,
    #[serde(default = "Status::onsale")]
    pub status: Status,
    #[serde(default)]
    pub views: u64,
    #[serde(default)]
    pub clicks: u64,
    #[serde(default)]
    pub favs: u64,
}

impl Status {
    fn onsale() -> Status {
        Status::OnSale
    }
}

impl Product {
    /// 价格（元）字符串，用于展示。
    pub fn price_yuan(&self) -> String {
        format!("{}.{:02}", self.price_cents / 100, self.price_cents % 100)
    }
}

/// 用户。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub campus: String,
    /// 兴趣标签
    #[serde(default)]
    pub interests: Vec<String>,
    /// 期望价位（分）
    #[serde(default)]
    pub avg_price: i64,
}

/// 行为类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// 浏览，权重 1.0
    View,
    /// 点击，权重 2.0
    Click,
    /// 收藏，权重 3.0
    Fav,
}

impl EventKind {
    /// 隐式反馈权重。
    pub fn weight(&self) -> f64 {
        match self {
            EventKind::View => 1.0,
            EventKind::Click => 2.0,
            EventKind::Fav => 3.0,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::View => "view",
            EventKind::Click => "click",
            EventKind::Fav => "fav",
        }
    }

    pub fn parse(s: &str) -> Option<EventKind> {
        match s {
            "view" => Some(EventKind::View),
            "click" => Some(EventKind::Click),
            "fav" => Some(EventKind::Fav),
            _ => None,
        }
    }
}

/// 一次用户行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub user_id: u64,
    pub product_id: u64,
    pub kind: EventKind,
    /// Unix 秒
    pub ts: i64,
}
