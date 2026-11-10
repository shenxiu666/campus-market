//! # mredis-web —— mini-redis 的 Web 演示界面
//!
//! 所有数据访问都走 RESP 协议（`mredis-client`），不直接读服务端内存，
//! 保证「协议层是唯一数据入口」。
//!
//! 模块划分：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`routes`] | axum 路由表与全部 handler（JSON 契约见 `docs/SPEC.md` 4.1） |
//! | [`state`] | `AppState` 与 `Backend` 命令通道抽象（生产 RESP / 测试内存） |
//! | [`store`] | 键 → JSON 的读取与 `REC.*` 结果加工 |
//! | [`model`] | `ProductJson` / `UserJson` 视图模型 |
//! | [`filter`] | 列表筛选与排序参数解析 |
//! | [`stats`] | `INFO` 分节解析 |
//! | [`bench`] | 三路快排 / 标准库排序 / Top-K 现场基准 |
//! | [`error`] | 统一的 `{"error","field"}` 错误响应 |
//! | [`static_assets`] | `web/` 三件套的编译期内嵌 |
//! | [`fake`] | 测试用内存版迷你 Redis（`#[cfg(test)]` 之外也可用于本地演示） |

pub mod bench;
pub mod error;
pub mod fake;
pub mod filter;
pub mod model;
pub mod routes;
pub mod state;
pub mod static_assets;
pub mod stats;
pub mod store;

pub use error::ApiError;
pub use routes::build_router;
pub use state::{AppState, Backend};

/// 默认 HTTP 端口。
pub const DEFAULT_HTTP_PORT: u16 = 8080;
/// 默认连接的 RESP 端口。
pub const DEFAULT_REDIS_PORT: u16 = 6380;
