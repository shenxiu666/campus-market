//! HTTP 错误响应：统一的 `{"error": "...", "field": "..."}` JSON 契约。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// 一个 API 错误。
#[derive(Debug, Clone)]
pub struct ApiError {
    /// HTTP 状态码
    pub status: StatusCode,
    /// 错误描述（写入 JSON 的 `error` 字段）
    pub message: String,
    /// 出错字段名（仅 400 参数错误时出现）
    pub field: Option<String>,
}

impl ApiError {
    /// 400：参数非法，带上出错字段名。
    pub fn bad_request(field: &str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            field: Some(field.to_string()),
        }
    }

    /// 400：参数非法但不针对具体字段。
    pub fn bad_request_plain(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            field: None,
        }
    }

    /// 404：资源不存在。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
            field: None,
        }
    }

    /// 503：存储层不可用。
    pub fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "storage unavailable".to_string(),
            field: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.message });
        if let Some(field) = &self.field {
            body["field"] = json!(field);
        }
        crate::routes::json_response(self.status, &body)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.field {
            Some(field) => write!(f, "{} ({field}): {}", self.status, self.message),
            None => write!(f, "{}: {}", self.status, self.message),
        }
    }
}
