//! 路由与处理器。
//!
//! 接口契约（JSON，UTF-8）见 `docs/SPEC.md` 4.1：
//!
//! * `GET  /`                              单页应用
//! * `GET  /app.js` `/style.css`           静态资源
//! * `GET  /api/health`                    健康检查
//! * `GET  /api/products`                  列表/筛选/排序
//! * `GET  /api/products/{id}`             详情
//! * `POST /api/products`                  发布
//! * `POST /api/products/{id}/event`       记录行为
//! * `GET  /api/recommend?user_id=&limit=` 为你推荐
//! * `GET  /api/similar/{id}`              相似商品
//! * `GET  /api/hot`                       热榜
//! * `GET  /api/users`                     用户列表
//! * `GET  /api/stats`                     演示面板数据
//! * `GET  /api/bench/sort?n=`             排序算法现场对比
//!
//! 所有数据访问都经过 `state::Backend`（生产是 RESP 客户端），处理器本身不持有状态。

use crate::error::ApiError;
use crate::filter::{self, ProductFilter};
use crate::model::{ProductJson, join_tags};
use crate::state::AppState;
use crate::stats::Info;
use crate::store;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mredis_proto::Resp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 默认分页大小。
const DEFAULT_LIMIT: usize = 20;
/// 分页上限。
const MAX_LIMIT: usize = 200;
/// 推荐结果默认条数。
const DEFAULT_REC_LIMIT: usize = 10;
/// 推荐结果上限。
const MAX_REC_LIMIT: usize = 50;
/// `/api/bench/sort` 默认规模。
const DEFAULT_BENCH_N: usize = 20_000;
/// `/api/bench/sort` 规模上限。
const MAX_BENCH_N: usize = 2_000_000;

/// 组装路由。
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route("/api/health", get(handlers::health))
        .route(
            "/api/products",
            get(handlers::list_products).post(handlers::create_product),
        )
        .route(
            "/api/products/{id}",
            get(handlers::get_product)
                .patch(handlers::update_product)
                .delete(handlers::delete_product),
        )
        .route("/api/products/{id}/event", post(handlers::record_event))
        .route("/api/recommend", get(handlers::recommend))
        .route("/api/similar/{id}", get(handlers::similar))
        .route("/api/hot", get(handlers::hot))
        .route("/api/users", get(handlers::users))
        .route("/api/stats", get(handlers::stats))
        .route("/api/bench/sort", get(handlers::bench_sort))
        .route("/app.js", get(handlers::app_js))
        .route("/style.css", get(handlers::style_css))
        .with_state(state)
}

/// 构造一个 JSON 响应（统一 `content-type: application/json; charset=utf-8`）。
pub fn json_response(status: StatusCode, body: &Value) -> Response {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    (
        status,
        [("content-type", "application/json; charset=utf-8")],
        text,
    )
        .into_response()
}

/// 200 + JSON。
fn ok_json(body: Value) -> Response {
    json_response(StatusCode::OK, &body)
}

/// 把 axum 的 `Json` 提取失败转成 SPEC 4.2 要求的 `400 + {"error":..,"field":..}`。
///
/// 默认的 `JsonRejection` 会返回 422 + 纯文本，破坏「所有错误都是 JSON」的契约，
/// 前端 `app.js` 也拿不到可展示的错误文案。
fn body_json<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    match body {
        Ok(Json(v)) => Ok(v),
        Err(rej) => Err(ApiError::bad_request("body", rej.body_text())),
    }
}

/// 当前 Unix 秒。
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub mod handlers {
    use super::*;

    /// 首页：编译期内嵌的 HTML。
    pub async fn index() -> Html<&'static str> {
        Html(crate::static_assets::INDEX_HTML)
    }

    /// `app.js`。
    pub async fn app_js() -> impl IntoResponse {
        (
            [("content-type", "application/javascript; charset=utf-8")],
            crate::static_assets::APP_JS,
        )
    }

    /// `style.css`。
    pub async fn style_css() -> impl IntoResponse {
        (
            [("content-type", "text/css; charset=utf-8")],
            crate::static_assets::STYLE_CSS,
        )
    }

    /// `GET /api/health` → `{"ok":true,"redis":"...","dbsize":42}`
    pub async fn health(State(state): State<AppState>) -> Response {
        let dbsize = match state.call_str(&["DBSIZE"]).await {
            Ok(resp) => resp.as_int().unwrap_or(0),
            Err(_) => return ApiError::unavailable().into_response(),
        };
        ok_json(json!({
            "ok": true,
            "redis": state.redis_addr.as_str(),
            "dbsize": dbsize,
        }))
    }

    /// `GET /api/products` 的查询参数。
    #[derive(Debug, Deserialize)]
    pub struct ListQuery {
        sort: Option<String>,
        tags: Option<String>,
        min: Option<String>,
        max: Option<String>,
        q: Option<String>,
        limit: Option<String>,
        offset: Option<String>,
    }

    /// `GET /api/products` → `{"total":42,"items":[ProductJson]}`
    pub async fn list_products(
        State(state): State<AppState>,
        Query(query): Query<ListQuery>,
    ) -> Response {
        match list_products_inner(&state, query).await {
            Ok(body) => ok_json(body),
            Err(e) => e.into_response(),
        }
    }

    async fn list_products_inner(state: &AppState, query: ListQuery) -> Result<Value, ApiError> {
        let order = filter::parse_sort(query.sort.as_deref())?;
        let min_price = filter::parse_price(query.min.as_deref(), "min")?;
        let max_price = filter::parse_price(query.max.as_deref(), "max")?;
        if let (Some(min), Some(max)) = (min_price, max_price) {
            if min > max {
                return Err(ApiError::bad_request("min", "min 不能大于 max"));
            }
        }
        let limit =
            filter::parse_usize(query.limit.as_deref(), "limit", DEFAULT_LIMIT, 1, MAX_LIMIT)?;
        let offset = filter::parse_usize(query.offset.as_deref(), "offset", 0, 0, usize::MAX / 2)?;
        let tags = filter::parse_tags(query.tags.as_deref());
        let keyword = query
            .q
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let filter = ProductFilter {
            tags,
            min_price,
            max_price,
            keyword,
            order,
        };
        let products = store::all_products(state).await?;
        let matched = filter.apply(&products);
        let total = matched.len();
        let items: Vec<Value> = matched
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|p| p.to_json())
            .collect();
        Ok(json!({ "total": total, "items": items, "limit": limit, "offset": offset }))
    }

    /// `GET /api/products/{id}` → `ProductJson`
    pub async fn get_product(State(state): State<AppState>, Path(id): Path<String>) -> Response {
        let id = match parse_id(&id, "id") {
            Ok(id) => id,
            Err(e) => return e.into_response(),
        };
        match store::require_product(&state, id).await {
            Ok(mut product) => {
                if let Ok(hot) = store::hot_scores(&state).await {
                    product.hot = hot.get(&product.id).copied().unwrap_or(0.0);
                }
                ok_json(product.to_json())
            }
            Err(e) => e.into_response(),
        }
    }

    /// `POST /api/products` 的请求体。
    #[derive(Debug, Deserialize)]
    pub struct CreateProduct {
        title: Option<String>,
        #[serde(default)]
        desc: Option<String>,
        /// 价格（分）。也接受 `price` 字段（元）。
        price_cents: Option<i64>,
        price: Option<f64>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        seller_id: Option<u64>,
        #[serde(default)]
        condition: Option<u8>,
        #[serde(default)]
        campus: Option<String>,
    }

    /// `POST /api/products` → 201 + `{"id":43}`
    pub async fn create_product(
        State(state): State<AppState>,
        body: Result<Json<CreateProduct>, JsonRejection>,
    ) -> Response {
        let body = match body_json(body) {
            Ok(body) => body,
            Err(e) => return e.into_response(),
        };
        match create_product_inner(&state, body).await {
            Ok(id) => json_response(StatusCode::CREATED, &json!({ "id": id })),
            Err(e) => e.into_response(),
        }
    }

    async fn create_product_inner(state: &AppState, body: CreateProduct) -> Result<u64, ApiError> {
        let title = body
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ApiError::bad_request("title", "title 不能为空"))?
            .to_string();

        // 价格：优先 price_cents（分），否则 price（元）四舍五入到分
        let price_cents = match (body.price_cents, body.price) {
            (Some(cents), _) => cents,
            (None, Some(yuan)) if yuan.is_finite() => (yuan * 100.0).round() as i64,
            _ => 0,
        };
        if price_cents <= 0 {
            return Err(ApiError::bad_request("price", "价格必须大于 0"));
        }
        if price_cents > 100_000_000 {
            return Err(ApiError::bad_request("price", "价格不能超过 100 万元"));
        }

        let tags: Vec<String> = body
            .tags
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if tags.is_empty() {
            return Err(ApiError::bad_request("tags", "至少需要一个标签"));
        }
        if tags.len() > 8 {
            return Err(ApiError::bad_request("tags", "标签最多 8 个"));
        }

        let seller_id = body.seller_id.unwrap_or(1);
        if seller_id == 0 {
            return Err(ApiError::bad_request("seller_id", "seller_id 必须大于 0"));
        }
        let condition = body.condition.unwrap_or(8);
        if !(1..=10).contains(&condition) {
            return Err(ApiError::bad_request("condition", "成色必须在 1..10 之间"));
        }
        let campus = body
            .campus
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("前湖校区")
            .to_string();
        let desc = body.desc.unwrap_or_default();
        let created_at = now_secs();

        // 自增 id：INCR 是原子的，天然避免并发发布撞号
        let id = match state.call_str(&["INCR", "meta:next_product_id"]).await? {
            Resp::Int(id) => id.max(1) as u64,
            Resp::Error(msg) => {
                return Err(ApiError::bad_request_plain(msg));
            }
            _ => return Err(ApiError::unavailable()),
        };

        let product = ProductJson {
            id,
            title,
            desc,
            price_cents,
            tags: tags.clone(),
            seller_id,
            condition,
            campus,
            created_at,
            status: "onsale".to_string(),
            views: 0,
            clicks: 0,
            favs: 0,
            hot: 0.0,
        };

        // 一次 pipeline 写完：商品 HASH + 标签集合 + 两个索引 ZSET
        let mut cmds: Vec<Vec<Vec<u8>>> = Vec::with_capacity(3 + tags.len());
        cmds.push(hset_product(&product));
        for tag in &tags {
            cmds.push(bytes_cmd(&["SADD", &format!("tag:{tag}"), &id.to_string()]));
        }
        cmds.push(bytes_cmd(&[
            "ZADD",
            "product:index:created",
            &created_at.to_string(),
            &id.to_string(),
        ]));
        cmds.push(bytes_cmd(&["ZADD", "zset:hot", "0", &id.to_string()]));

        let replies = state.pipeline(cmds).await?;
        for reply in &replies {
            if let Resp::Error(msg) = reply {
                return Err(ApiError::bad_request_plain(msg.clone()));
            }
        }

        // 键空间写完后必须再通知推荐引擎一次：`recommend_for_user` 的候选集来自
        // `Recommender::products`，只发 HSET/SADD 的话新商品永远不会被推荐到。
        // 这条命令要放在 pipeline **之后**——`REC.UPSERT` 读的是 `product:<id>` 哈希，
        // 哈希还没写进去时调用会拿到 not found。
        //
        // 失败不算致命：商品已经落库，用户刷新列表能看到；只是推荐索引慢一拍，
        // 下次重启 `rebuild_rec_from_keyspace` 也会补上。所以这里只记日志不报错。
        if let Err(e) = state.call_str(&["REC.UPSERT", &id.to_string()]).await {
            eprintln!("mredis-web: 商品 {id} 已保存，但通知推荐引擎失败: {e}");
        }
        Ok(id)
    }

    /// `PATCH /api/products/{id}` 的请求体：所有字段可选，只改传了的。
    #[derive(Debug, Deserialize)]
    pub struct UpdateProduct {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        desc: Option<String>,
        /// 价格（分）。也接受 `price` 字段（元）。
        price_cents: Option<i64>,
        price: Option<f64>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        seller_id: Option<u64>,
        #[serde(default)]
        condition: Option<u8>,
        #[serde(default)]
        campus: Option<String>,
        #[serde(default)]
        status: Option<String>,
    }

    /// `PATCH /api/products/{id}` → `{"ok":true,"product":ProductJson}`
    pub async fn update_product(
        State(state): State<AppState>,
        Path(id): Path<String>,
        body: Result<Json<UpdateProduct>, JsonRejection>,
    ) -> Response {
        let body = match body_json(body) {
            Ok(body) => body,
            Err(e) => return e.into_response(),
        };
        match update_product_inner(&state, &id, body).await {
            Ok(body) => ok_json(body),
            Err(e) => e.into_response(),
        }
    }

    async fn update_product_inner(
        state: &AppState,
        raw_id: &str,
        body: UpdateProduct,
    ) -> Result<Value, ApiError> {
        let id = parse_id(raw_id, "id")?;
        let mut product = store::require_product(state, id).await?;
        let old_tags = product.tags.clone();

        if let Some(title) = body.title.as_deref() {
            let title = title.trim();
            if title.is_empty() {
                return Err(ApiError::bad_request("title", "title 不能为空"));
            }
            product.title = title.to_string();
        }
        if let Some(desc) = body.desc {
            product.desc = desc;
        }
        if body.price_cents.is_some() || body.price.is_some() {
            let cents = match (body.price_cents, body.price) {
                (Some(cents), _) => cents,
                (None, Some(yuan)) if yuan.is_finite() => (yuan * 100.0).round() as i64,
                _ => 0,
            };
            if cents <= 0 {
                return Err(ApiError::bad_request("price", "价格必须大于 0"));
            }
            if cents > 100_000_000 {
                return Err(ApiError::bad_request("price", "价格不能超过 100 万元"));
            }
            product.price_cents = cents;
        }
        if let Some(tags) = body.tags {
            let tags: Vec<String> = tags
                .into_iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if tags.is_empty() {
                return Err(ApiError::bad_request("tags", "至少需要一个标签"));
            }
            if tags.len() > 8 {
                return Err(ApiError::bad_request("tags", "标签最多 8 个"));
            }
            product.tags = tags;
        }
        if let Some(seller_id) = body.seller_id {
            if seller_id == 0 {
                return Err(ApiError::bad_request("seller_id", "seller_id 必须大于 0"));
            }
            product.seller_id = seller_id;
        }
        if let Some(condition) = body.condition {
            if !(1..=10).contains(&condition) {
                return Err(ApiError::bad_request("condition", "成色必须在 1..10 之间"));
            }
            product.condition = condition;
        }
        if let Some(campus) = body.campus.as_deref() {
            let campus = campus.trim();
            if campus.is_empty() {
                return Err(ApiError::bad_request("campus", "campus 不能为空"));
            }
            product.campus = campus.to_string();
        }
        if let Some(status) = body.status.as_deref() {
            let status = status.trim().to_ascii_lowercase();
            if !matches!(status.as_str(), "onsale" | "reserved" | "sold") {
                return Err(ApiError::bad_request(
                    "status",
                    "status 只能是 onsale / reserved / sold",
                ));
            }
            product.status = status;
        }

        // 商品 HASH 整体重写（字段值取自刚读出来的哈希，不会丢计数器），
        // 再按「新旧标签差集」维护倒排索引。
        let mut cmds: Vec<Vec<Vec<u8>>> = vec![hset_product(&product)];
        for tag in &old_tags {
            if !product.tags.contains(tag) {
                cmds.push(bytes_cmd(&["SREM", &format!("tag:{tag}"), &id.to_string()]));
            }
        }
        for tag in &product.tags {
            if !old_tags.contains(tag) {
                cmds.push(bytes_cmd(&["SADD", &format!("tag:{tag}"), &id.to_string()]));
            }
        }
        let replies = state.pipeline(cmds).await?;
        for reply in &replies {
            if let Resp::Error(msg) = reply {
                return Err(ApiError::bad_request_plain(msg.clone()));
            }
        }

        if let Err(e) = state.call_str(&["REC.UPSERT", &id.to_string()]).await {
            eprintln!("mredis-web: 商品 {id} 已更新，但通知推荐引擎失败: {e}");
        }

        let mut product = store::require_product(state, id).await?;
        if let Ok(hot) = store::hot_scores(state).await {
            product.hot = hot.get(&id).copied().unwrap_or(0.0);
        }
        Ok(json!({ "ok": true, "product": product.to_json() }))
    }

    /// `DELETE /api/products/{id}` → `{"ok":true,"deleted":43}`
    pub async fn delete_product(State(state): State<AppState>, Path(id): Path<String>) -> Response {
        match delete_product_inner(&state, &id).await {
            Ok(body) => ok_json(body),
            Err(e) => e.into_response(),
        }
    }

    async fn delete_product_inner(state: &AppState, raw_id: &str) -> Result<Value, ApiError> {
        let id = parse_id(raw_id, "id")?;
        let product = store::require_product(state, id).await?;

        // 删商品哈希 + 所有倒排索引 + 两个 ZSET。历史行为事件保留：
        // 它是「谁看过什么」的流水，不该因为商品下架而被篡改。
        let mut cmds: Vec<Vec<Vec<u8>>> = vec![bytes_cmd(&["DEL", &format!("product:{}", id)])];
        for tag in &product.tags {
            cmds.push(bytes_cmd(&["SREM", &format!("tag:{tag}"), &id.to_string()]));
        }
        cmds.push(bytes_cmd(&[
            "ZREM",
            "product:index:created",
            &id.to_string(),
        ]));
        cmds.push(bytes_cmd(&["ZREM", "zset:hot", &id.to_string()]));
        let replies = state.pipeline(cmds).await?;
        for reply in &replies {
            if let Resp::Error(msg) = reply {
                return Err(ApiError::bad_request_plain(msg.clone()));
            }
        }

        // 推荐索引是键空间的派生视图：删完键整体重建一次，避免残留幽灵商品。
        if let Err(e) = state.call_str(&["REC.REBUILD"]).await {
            eprintln!("mredis-web: 商品 {id} 已删除，但重建推荐索引失败: {e}");
        }
        Ok(json!({ "ok": true, "deleted": id }))
    }

    /// `POST /api/products/{id}/event` 的请求体。
    #[derive(Debug, Deserialize)]
    pub struct EventBody {
        user_id: Option<u64>,
        kind: Option<String>,
    }

    /// `POST /api/products/{id}/event` → `{"ok":true,"product":ProductJson}`
    pub async fn record_event(
        State(state): State<AppState>,
        Path(id): Path<String>,
        body: Result<Json<EventBody>, JsonRejection>,
    ) -> Response {
        let body = match body_json(body) {
            Ok(body) => body,
            Err(e) => return e.into_response(),
        };
        match record_event_inner(&state, &id, body).await {
            Ok(body) => ok_json(body),
            Err(e) => e.into_response(),
        }
    }

    async fn record_event_inner(
        state: &AppState,
        raw_id: &str,
        body: EventBody,
    ) -> Result<Value, ApiError> {
        let id = parse_id(raw_id, "id")?;
        let user_id = body.user_id.unwrap_or(1);
        if user_id == 0 {
            return Err(ApiError::bad_request("user_id", "user_id 必须大于 0"));
        }
        let kind = body
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ApiError::bad_request("kind", "kind 不能为空"))?
            .to_ascii_lowercase();
        if !matches!(kind.as_str(), "view" | "click" | "fav") {
            return Err(ApiError::bad_request(
                "kind",
                "kind 只能是 view / click / fav",
            ));
        }
        // 先确认商品存在，避免给幽灵商品刷热度
        let _ = store::require_product(state, id).await?;

        let uid = user_id.to_string();
        let pid = id.to_string();
        match state.call_str(&["REC.TRACK", &uid, &pid, &kind]).await? {
            Resp::Int(_) | Resp::Simple(_) => {}
            Resp::Error(msg) => return Err(ApiError::bad_request_plain(msg)),
            _ => return Err(ApiError::unavailable()),
        }

        let mut product = store::require_product(state, id).await?;
        if let Ok(hot) = store::hot_scores(state).await {
            product.hot = hot.get(&id).copied().unwrap_or(0.0);
        }
        Ok(json!({ "ok": true, "product": product.to_json() }))
    }

    /// `GET /api/recommend` 的查询参数。
    #[derive(Debug, Deserialize)]
    pub struct RecQuery {
        user_id: Option<String>,
        limit: Option<String>,
    }

    /// `GET /api/recommend?user_id=1&limit=10`
    pub async fn recommend(
        State(state): State<AppState>,
        Query(query): Query<RecQuery>,
    ) -> Response {
        match recommend_inner(&state, query).await {
            Ok(body) => ok_json(body),
            Err(e) => e.into_response(),
        }
    }

    async fn recommend_inner(state: &AppState, query: RecQuery) -> Result<Value, ApiError> {
        let user_id = match query.user_id.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => raw
                .parse::<u64>()
                .map_err(|_| ApiError::bad_request("user_id", "user_id 必须是正整数"))?,
            _ => return Err(ApiError::bad_request("user_id", "缺少 user_id 参数")),
        };
        if user_id == 0 {
            return Err(ApiError::bad_request("user_id", "user_id 必须大于 0"));
        }
        let limit = filter::parse_usize(
            query.limit.as_deref(),
            "limit",
            DEFAULT_REC_LIMIT,
            1,
            MAX_REC_LIMIT,
        )?;

        let uid = user_id.to_string();
        let limit_text = limit.to_string();
        let reply = state.call_str(&["REC.FOR_YOU", &uid, &limit_text]).await?;
        let items = store::parse_rec_array(&reply)?;
        let items = store::enrich_items(state, items).await?;
        Ok(json!({ "user_id": user_id, "items": items }))
    }

    /// `GET /api/similar/{id}?limit=6`
    pub async fn similar(
        State(state): State<AppState>,
        Path(id): Path<String>,
        Query(query): Query<RecQuery>,
    ) -> Response {
        match similar_inner(&state, &id, query).await {
            Ok(body) => ok_json(body),
            Err(e) => e.into_response(),
        }
    }

    async fn similar_inner(
        state: &AppState,
        raw_id: &str,
        query: RecQuery,
    ) -> Result<Value, ApiError> {
        let id = parse_id(raw_id, "id")?;
        let limit = filter::parse_usize(query.limit.as_deref(), "limit", 6, 1, MAX_REC_LIMIT)?;
        let pid = id.to_string();
        let limit_text = limit.to_string();
        let reply = state.call_str(&["REC.SIMILAR", &pid, &limit_text]).await?;
        let items = store::parse_rec_array(&reply)?;
        let items = store::enrich_items(state, items).await?;
        Ok(json!({ "product_id": id, "items": items }))
    }

    /// `GET /api/hot?limit=10`
    pub async fn hot(State(state): State<AppState>, Query(query): Query<RecQuery>) -> Response {
        match hot_inner(&state, query).await {
            Ok(body) => ok_json(body),
            Err(e) => e.into_response(),
        }
    }

    async fn hot_inner(state: &AppState, query: RecQuery) -> Result<Value, ApiError> {
        let limit = filter::parse_usize(
            query.limit.as_deref(),
            "limit",
            DEFAULT_REC_LIMIT,
            1,
            MAX_REC_LIMIT,
        )?;
        let limit_text = limit.to_string();
        let reply = state.call_str(&["REC.HOT", &limit_text]).await?;
        let items = store::parse_rec_array(&reply)?;
        let items = store::enrich_items(state, items).await?;
        Ok(json!({ "items": items }))
    }

    /// `GET /api/users` → `{"items":[UserJson]}`
    pub async fn users(State(state): State<AppState>) -> Response {
        match store::all_users(&state).await {
            Ok(users) => {
                let items: Vec<Value> = users.iter().map(|u| u.to_json()).collect();
                ok_json(json!({ "items": items }))
            }
            Err(e) => e.into_response(),
        }
    }

    /// `GET /api/stats`
    pub async fn stats(State(state): State<AppState>) -> Response {
        match stats_inner(&state).await {
            Ok(body) => ok_json(body),
            Err(e) => e.into_response(),
        }
    }

    async fn stats_inner(state: &AppState) -> Result<Value, ApiError> {
        let text = match state.call_str(&["INFO"]).await? {
            Resp::Bulk(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Resp::Simple(s) => s,
            Resp::Error(msg) => return Err(ApiError::bad_request_plain(msg)),
            _ => return Err(ApiError::unavailable()),
        };
        let info = Info::parse(&text);
        // DBSIZE 单独取，INFO 的 db0:keys 在极端情况下可能滞后
        let dbsize = match state.call_str(&["DBSIZE"]).await {
            Ok(resp) => resp.as_int().map(|n| n.max(0) as u64),
            Err(_) => None,
        };
        let mut body = info.to_stats_json(dbsize);
        // REC.STATS 更精确，优先用它覆盖 INFO 里的 rec_* 数字
        if let Ok(reply) = state.call_str(&["REC.STATS"]).await {
            if let Ok(rec) = store::parse_rec_object(&reply) {
                body["rec"] = rec;
            }
        }
        Ok(body)
    }

    /// `GET /api/bench/sort` 的查询参数。
    #[derive(Debug, Deserialize)]
    pub struct BenchQuery {
        n: Option<String>,
    }

    /// `GET /api/bench/sort?n=20000`
    pub async fn bench_sort(
        State(_state): State<AppState>,
        Query(query): Query<BenchQuery>,
    ) -> Response {
        let n = match filter::parse_usize(query.n.as_deref(), "n", DEFAULT_BENCH_N, 1, MAX_BENCH_N)
        {
            Ok(n) => n,
            Err(e) => return e.into_response(),
        };
        ok_json(crate::bench::run_sort_bench(n))
    }

    /// 把路径参数解析成商品 id。
    fn parse_id(raw: &str, field: &str) -> Result<u64, ApiError> {
        let raw = raw.trim();
        let id: u64 = raw
            .parse()
            .map_err(|_| ApiError::bad_request(field, format!("{field} 必须是正整数")))?;
        if id == 0 {
            return Err(ApiError::bad_request(field, "id 必须大于 0"));
        }
        Ok(id)
    }

    /// 拼一条 `HSET product:<id> ...` 命令。
    fn hset_product(product: &ProductJson) -> Vec<Vec<u8>> {
        bytes_cmd(&[
            "HSET",
            &format!("product:{}", product.id),
            "id",
            &product.id.to_string(),
            "title",
            &product.title,
            "desc",
            &product.desc,
            "price_cents",
            &product.price_cents.to_string(),
            "tags",
            &join_tags(&product.tags),
            "seller_id",
            &product.seller_id.to_string(),
            "condition",
            &product.condition.to_string(),
            "campus",
            &product.campus,
            "created_at",
            &product.created_at.to_string(),
            "status",
            &product.status,
            "views",
            &product.views.to_string(),
            "clicks",
            &product.clicks.to_string(),
            "favs",
            &product.favs.to_string(),
        ])
    }

    /// 字符串切片 → RESP 命令的字节参数。
    fn bytes_cmd(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|s| s.as_bytes().to_vec()).collect()
    }
}

/// 一个 `HashMap` 别名，供其他模块（如 `bench`）使用，避免重复引入。
pub type Map = HashMap<String, Value>;

/// 把 `Instant` 差值转成毫秒（保留 3 位小数）。
pub fn elapsed_ms(start: Instant) -> f64 {
    let micros = start.elapsed().as_micros() as f64;
    (micros / 1000.0 * 1000.0).round() / 1000.0
}
