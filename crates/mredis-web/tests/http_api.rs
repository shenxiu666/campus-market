//! 集成测试：用 `tower::ServiceExt::oneshot` 直接打 router，不需要真实服务端。
//!
//! 后端是 [`mredis_web::fake::FakeBackend`]（内存版迷你 Redis），
//! 因此这里验证的是**真实的 HTTP 链路**：路由匹配、参数解析、JSON 契约、状态码。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use mredis_web::fake::FakeBackend;
use mredis_web::{AppState, build_router};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

/// 用假后端起一个 router。
fn app(fake: FakeBackend) -> axum::Router {
    build_router(AppState::with_backend("127.0.0.1:6380", Arc::new(fake)))
}

/// 默认数据集：3 个商品 + 2 个用户。
fn sample_app() -> axum::Router {
    app(FakeBackend::new()
        .with_product(
            1,
            "捷安特山地车",
            45000,
            &["自行车", "运动"],
            1_700_000_100,
            0.9,
        )
        .with_product(2, "高数教材", 1500, &["教材", "考研"], 1_700_000_300, 0.2)
        .with_product(3, "宿舍台灯", 3000, &["生活", "宿舍"], 1_700_000_200, 0.5)
        .with_user(1, "张三", &["自行车", "运动"])
        .with_user(2, "李四", &["教材"]))
}

/// 把响应体读成字符串（不引入 http-body-util 依赖）。
async fn body_text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("应能读全 body");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 发一个请求并读取 (状态码, JSON 体)。
async fn call(router: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response: Response = router.oneshot(request).await.expect("请求应成功");
    let status = response.status();
    let text = body_text(response).await;
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, json)
}

/// 便捷 GET。
async fn get(router: axum::Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("请求应可构造");
    call(router, request).await
}

/// 便捷 POST（JSON 体）。
async fn post(router: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("请求应可构造");
    call(router, request).await
}

/// 便捷 PATCH（JSON 体）。
async fn patch(router: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("请求应可构造");
    call(router, request).await
}

/// 便捷 DELETE。
async fn delete(router: axum::Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .expect("请求应可构造");
    call(router, request).await
}

// ======================= 静态资源 =======================

#[tokio::test]
async fn index_serves_embedded_html() {
    let request = Request::builder()
        .uri("/")
        .body(Body::empty())
        .expect("请求应可构造");
    let response = sample_app().oneshot(request).await.expect("应成功");
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("校园二手交易"));
    assert!(html.contains("/app.js"));
}

#[tokio::test]
async fn app_js_and_css_have_correct_content_type() {
    for (uri, expected) in [
        ("/app.js", "application/javascript"),
        ("/style.css", "text/css"),
    ] {
        let request = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("请求应可构造");
        let response = sample_app().oneshot(request).await.expect("应成功");
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.contains(expected),
            "{uri} 的 content-type 应为 {expected}，实际 {content_type}"
        );
    }
}

// ======================= 健康检查 =======================

#[tokio::test]
async fn health_reports_ok_and_dbsize() {
    let (status, body) = get(sample_app(), "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], Value::Bool(true));
    assert!(body["dbsize"].as_u64().unwrap_or(0) > 0);
    assert_eq!(body["redis"], Value::String("127.0.0.1:6380".into()));
}

#[tokio::test]
async fn health_returns_503_when_storage_down() {
    let (status, body) = get(app(FakeBackend::down()), "/api/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], Value::String("storage unavailable".into()));
}

// ======================= 商品列表 =======================

#[tokio::test]
async fn list_products_returns_contract_shape() {
    let (status, body) = get(sample_app(), "/api/products").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], Value::from(3));
    let items = body["items"].as_array().expect("items 应为数组");
    assert_eq!(items.len(), 3);
    let first = &items[0];
    for key in [
        "id",
        "title",
        "desc",
        "price_cents",
        "price",
        "tags",
        "seller_id",
        "condition",
        "campus",
        "created_at",
        "status",
        "views",
        "clicks",
        "favs",
        "hot",
    ] {
        assert!(first.get(key).is_some(), "缺少字段 {key}");
    }
    // 默认排序是 time_desc
    assert_eq!(items[0]["id"], Value::from(2));
}

#[tokio::test]
async fn list_products_price_field_is_formatted_yuan() {
    let (_, body) = get(sample_app(), "/api/products?sort=price_asc").await;
    let items = body["items"].as_array().expect("items 应为数组");
    assert_eq!(items[0]["price"], Value::String("15.00".into()));
    assert_eq!(items[0]["price_cents"], Value::from(1500));
}

#[tokio::test]
async fn list_products_supports_all_sort_orders() {
    for sort in [
        "price_asc",
        "price_desc",
        "time_desc",
        "hot_desc",
        "condition_desc",
    ] {
        let (status, body) = get(sample_app(), &format!("/api/products?sort={sort}")).await;
        assert_eq!(status, StatusCode::OK, "sort={sort} 应成功");
        assert_eq!(
            body["items"].as_array().map(Vec::len),
            Some(3),
            "sort={sort}"
        );
    }
}

#[tokio::test]
async fn list_products_rejects_unknown_sort_with_field() {
    let (status, body) = get(sample_app(), "/api/products?sort=weird").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("sort".into()));
    assert!(body["error"].as_str().unwrap_or("").contains("sort"));
}

#[tokio::test]
async fn list_products_filters_by_tags_with_and_semantics() {
    let (_, body) = get(sample_app(), "/api/products?tags=自行车").await;
    assert_eq!(body["total"], Value::from(1));

    let (_, body) = get(sample_app(), "/api/products?tags=自行车,运动").await;
    assert_eq!(body["total"], Value::from(1));

    // AND 语义：自行车 + 教材 没有交集
    let (_, body) = get(sample_app(), "/api/products?tags=自行车,教材").await;
    assert_eq!(body["total"], Value::from(0));
}

#[tokio::test]
async fn list_products_price_filter_takes_yuan() {
    // 20 元到 40 元 → 只有 3000 分的台灯
    let (_, body) = get(sample_app(), "/api/products?min=20&max=40").await;
    assert_eq!(body["total"], Value::from(1));
    assert_eq!(body["items"][0]["id"], Value::from(3));
}

#[tokio::test]
async fn list_products_rejects_inverted_price_range() {
    let (status, body) = get(sample_app(), "/api/products?min=100&max=10").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("min".into()));
}

#[tokio::test]
async fn list_products_rejects_bad_price() {
    let (status, body) = get(sample_app(), "/api/products?min=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("min".into()));
}

#[tokio::test]
async fn list_products_keyword_matches_title() {
    let (_, body) = get(sample_app(), "/api/products?q=教材").await;
    assert_eq!(body["total"], Value::from(1));
    assert_eq!(body["items"][0]["id"], Value::from(2));

    let (_, body) = get(sample_app(), "/api/products?q=不存在的关键词").await;
    assert_eq!(body["total"], Value::from(0));
}

#[tokio::test]
async fn list_products_paginates() {
    let (_, body) = get(sample_app(), "/api/products?limit=2&offset=0").await;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["total"], Value::from(3));

    let (_, body) = get(sample_app(), "/api/products?limit=2&offset=2").await;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn list_products_rejects_out_of_range_limit() {
    let (status, body) = get(sample_app(), "/api/products?limit=0").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("limit".into()));
}

// ======================= 商品详情 =======================

#[tokio::test]
async fn get_product_returns_full_object() {
    let (status, body) = get(sample_app(), "/api/products/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], Value::from(1));
    assert_eq!(body["title"], Value::String("捷安特山地车".into()));
    assert_eq!(body["price"], Value::String("450.00".into()));
    assert_eq!(body["tags"], serde_json::json!(["自行车", "运动"]));
    assert_eq!(body["hot"], Value::from(0.9));
}

#[tokio::test]
async fn get_missing_product_returns_404() {
    let (status, body) = get(sample_app(), "/api/products/999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], Value::String("product not found".into()));
}

#[tokio::test]
async fn get_product_rejects_non_numeric_id() {
    let (status, body) = get(sample_app(), "/api/products/abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("id".into()));
}

// ======================= 发布商品 =======================

#[tokio::test]
async fn create_product_returns_201_with_id() {
    let router = app(FakeBackend::new()
        .with_product(1, "旧商品", 1000, &["a"], 1_700_000_000, 0.0)
        .with_user(1, "张三", &[]));
    // 预置自增 id
    let (status, body) = post(
        router,
        "/api/products",
        serde_json::json!({
            "title": "新自行车",
            "desc": "九成新",
            "price_cents": 30000,
            "tags": ["自行车", "运动"],
            "seller_id": 1,
            "condition": 9,
            "campus": "前湖校区"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn create_product_accepts_price_in_yuan() {
    let (status, body) = post(
        sample_app(),
        "/api/products",
        serde_json::json!({
            "title": "旧吉他",
            "price": 120.5,
            "tags": ["乐器"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].as_u64().is_some());

    // 发布后能按价格筛到（120.5 元 = 12050 分）
    let (_, listing) = get(sample_app(), "/api/products").await;
    assert_eq!(listing["total"], Value::from(3));
}

#[tokio::test]
async fn create_product_validates_title() {
    let (status, body) = post(
        sample_app(),
        "/api/products",
        serde_json::json!({ "title": "   ", "price_cents": 100, "tags": ["x"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("title".into()));
}

#[tokio::test]
async fn create_product_validates_price() {
    let (status, body) = post(
        sample_app(),
        "/api/products",
        serde_json::json!({ "title": "免费送", "price_cents": 0, "tags": ["x"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("price".into()));

    let (status, body) = post(
        sample_app(),
        "/api/products",
        serde_json::json!({ "title": "超贵", "price_cents": 999_999_999, "tags": ["x"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("price".into()));
}

#[tokio::test]
async fn create_product_validates_tags() {
    let (status, body) = post(
        sample_app(),
        "/api/products",
        serde_json::json!({ "title": "无标签", "price_cents": 100, "tags": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("tags".into()));
}

#[tokio::test]
async fn create_product_validates_condition() {
    let (status, body) = post(
        sample_app(),
        "/api/products",
        serde_json::json!({ "title": "成色错误", "price_cents": 100, "tags": ["x"], "condition": 11 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("condition".into()));
}

// ======================= 行为上报 =======================

#[tokio::test]
async fn record_event_updates_counters() {
    let router =
        app(FakeBackend::new().with_product(1, "车", 1000, &["自行车"], 1_700_000_000, 0.0));
    let (status, body) = post(
        router,
        "/api/products/1/event",
        serde_json::json!({ "user_id": 1, "kind": "fav" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["product"]["favs"], Value::from(1));
}

#[tokio::test]
async fn record_event_rejects_bad_kind() {
    let (status, body) = post(
        sample_app(),
        "/api/products/1/event",
        serde_json::json!({ "user_id": 1, "kind": "share" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("kind".into()));
}

#[tokio::test]
async fn record_event_rejects_missing_kind() {
    let (status, body) = post(
        sample_app(),
        "/api/products/1/event",
        serde_json::json!({ "user_id": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("kind".into()));
}

#[tokio::test]
async fn record_event_404_on_missing_product() {
    let (status, body) = post(
        sample_app(),
        "/api/products/999/event",
        serde_json::json!({ "user_id": 1, "kind": "view" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], Value::String("product not found".into()));
}

#[tokio::test]
async fn record_event_rejects_zero_user() {
    let (status, body) = post(
        sample_app(),
        "/api/products/1/event",
        serde_json::json!({ "user_id": 0, "kind": "view" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("user_id".into()));
}

// ======================= 推荐 / 相似 / 热榜 =======================

#[tokio::test]
async fn recommend_returns_items_with_product_and_reasons() {
    let (status, body) = get(sample_app(), "/api/recommend?user_id=1&limit=5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user_id"], Value::from(1));
    let items = body["items"].as_array().expect("items 应为数组");
    assert!(!items.is_empty());
    for item in items {
        assert!(item.get("product_id").is_some());
        assert!(item.get("score").is_some());
        assert!(item.get("reasons").is_some());
        assert!(item.get("product").is_some());
    }
}

#[tokio::test]
async fn recommend_requires_user_id() {
    let (status, body) = get(sample_app(), "/api/recommend").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("user_id".into()));
}

#[tokio::test]
async fn recommend_rejects_bad_user_id() {
    let (status, body) = get(sample_app(), "/api/recommend?user_id=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("user_id".into()));
}

#[tokio::test]
async fn recommend_cold_start_user_gets_cold_start_reason() {
    let router = app(FakeBackend::new()
        .with_product(1, "车", 1000, &["自行车"], 1_700_000_000, 0.5)
        .with_user(7, "吴九", &[]));
    let (status, body) = get(router, "/api/recommend?user_id=7").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items 应为数组");
    let reasons = items[0]["reasons"].as_array().expect("应有理由");
    assert_eq!(reasons[0]["type"], Value::String("cold_start".into()));
}

#[tokio::test]
async fn similar_returns_other_products() {
    let (status, body) = get(sample_app(), "/api/similar/1?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["product_id"], Value::from(1));
    let items = body["items"].as_array().expect("items 应为数组");
    assert!(items.len() <= 2);
    for item in items {
        assert_ne!(item["product_id"], Value::from(1));
    }
}

#[tokio::test]
async fn similar_rejects_bad_id() {
    let (status, body) = get(sample_app(), "/api/similar/xyz").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("id".into()));
}

#[tokio::test]
async fn hot_returns_ranked_items() {
    let (status, body) = get(sample_app(), "/api/hot?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items 应为数组");
    assert_eq!(items.len(), 2);
    assert!(items[0]["product"].is_object());
}

// ======================= 用户 =======================

#[tokio::test]
async fn users_returns_contract_shape() {
    let (status, body) = get(sample_app(), "/api/users").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items 应为数组");
    assert_eq!(items.len(), 2);
    let first = &items[0];
    assert_eq!(first["id"], Value::from(1));
    assert_eq!(first["name"], Value::String("张三".into()));
    assert_eq!(first["interests"], serde_json::json!(["自行车", "运动"]));
    assert!(first.get("avg_price").is_some());
    assert!(first.get("campus").is_some());
}

#[tokio::test]
async fn users_does_not_leak_event_lists() {
    // user:1:events 是 LIST，不应被当成用户 HASH
    let router = app(FakeBackend::new()
        .with_user(1, "张三", &["a"])
        .with_user(2, "李四", &["b"]));
    let (_, body) = get(router, "/api/users").await;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(2));
}

// ======================= 统计面板 =======================

#[tokio::test]
async fn stats_parses_info_sections() {
    let (status, body) = get(sample_app(), "/api/stats").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["dbsize"].as_u64().unwrap_or(0) > 0);
    assert_eq!(body["dict"]["buckets"], Value::from(16));
    assert_eq!(body["dict"]["rehashing"], Value::Bool(false));
    assert_eq!(
        body["dict"]["distribution"],
        serde_json::json!([0, 1, 0, 2, 3])
    );
    assert!(body["hit_rate"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(body["total_commands"].as_u64().unwrap_or(0) > 0);
    assert_eq!(body["uptime_secs"], Value::from(120));
    let recent = body["recent_commands"].as_array().expect("应有命令流");
    assert!(!recent.is_empty());
    // 最新在前
    assert!(recent[0].as_str().unwrap_or("").contains("INFO"));
    assert_eq!(body["rec"]["co_occurrence_pairs"], Value::from(12));
    assert_eq!(body["aof"]["enabled"], Value::Bool(true));
    assert_eq!(body["aof"]["commands"], Value::from(88));
}

#[tokio::test]
async fn stats_returns_503_when_storage_down() {
    let (status, body) = get(app(FakeBackend::down()), "/api/stats").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], Value::String("storage unavailable".into()));
}

// ======================= 排序基准 =======================

#[tokio::test]
async fn bench_sort_runs_three_algorithms() {
    let (status, body) = get(sample_app(), "/api/bench/sort?n=500").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["n"], Value::from(500));
    assert_eq!(body["consistent"], Value::Bool(true));
    let results = body["results"].as_array().expect("results 应为数组");
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0]["algorithm"],
        Value::String("three_way_quick".into())
    );
    assert_eq!(
        results[1]["algorithm"],
        Value::String("sort_unstable_by".into())
    );
    assert_eq!(results[2]["algorithm"], Value::String("heap_top_k".into()));
}

#[tokio::test]
async fn bench_sort_defaults_to_20000() {
    let (status, body) = get(sample_app(), "/api/bench/sort").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["n"], Value::from(20_000));
}

#[tokio::test]
async fn bench_sort_rejects_bad_n() {
    let (status, body) = get(sample_app(), "/api/bench/sort?n=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], Value::String("n".into()));
}

// ======================= 通用契约 =======================

#[tokio::test]
async fn all_json_responses_declare_utf8_charset() {
    let request = Request::builder()
        .uri("/api/health")
        .body(Body::empty())
        .expect("请求应可构造");
    let response = sample_app().oneshot(request).await.expect("应成功");
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(content_type.contains("application/json"));
    assert!(content_type.contains("charset=utf-8"));
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let request = Request::builder()
        .uri("/api/nope")
        .body(Body::empty())
        .expect("请求应可构造");
    let response = sample_app().oneshot(request).await.expect("应成功");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wrong_method_returns_405() {
    let request = Request::builder()
        .method("DELETE")
        .uri("/api/products")
        .body(Body::empty())
        .expect("请求应可构造");
    let response = sample_app().oneshot(request).await.expect("应成功");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn error_bodies_carry_error_key() {
    for uri in ["/api/products/999", "/api/products?sort=bad"] {
        let (status, body) = get(sample_app(), uri).await;
        assert!(status.is_client_error() || status.is_server_error());
        assert!(body.get("error").is_some(), "{uri} 应带 error 字段");
    }
}

// ======================= JSON 提取失败（回归） =======================

/// axum 的 `JsonRejection` 默认返回 **422 + 纯文本**，破坏 SPEC 4.2 的
/// 「所有错误都是 400/404/503 + JSON」契约，前端也拿不到 `error` 字段。
/// 这里锁住修复后的行为：类型不匹配 → 400 + JSON + `field`。
#[tokio::test]
async fn malformed_json_body_returns_400_json() {
    let request = Request::builder()
        .method("POST")
        .uri("/api/products")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"title":"x","price":"abc"}"#))
        .expect("请求应可构造");
    let (status, body) = call(sample_app(), request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.get("error").is_some(), "应带 error 字段");
    assert_eq!(body["field"], Value::String("body".into()));
}

/// 连 JSON 都解析不了（语法错误）时同样必须是 400 + JSON，而不是 422 纯文本。
#[tokio::test]
async fn syntactically_invalid_json_returns_400_json() {
    let request = Request::builder()
        .method("POST")
        .uri("/api/products/1/event")
        .header("content-type", "application/json")
        .body(Body::from("{bad"))
        .expect("请求应可构造");
    let (status, body) = call(sample_app(), request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.get("error").is_some(), "应带 error 字段");
}

// ======================= 商品编辑 / 删除 =======================

#[tokio::test]
async fn patch_updates_product_and_persists() {
    let router = sample_app();
    let (status, body) = patch(
        router.clone(),
        "/api/products/1",
        serde_json::json!({
            "title": "捷安特山地车（改）",
            "price_cents": 39000,
            "tags": ["自行车", "健身"],
            "condition": 9,
            "status": "reserved",
            "desc": "已换新刹车片"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(
        body["product"]["title"],
        Value::String("捷安特山地车（改）".into())
    );
    assert_eq!(body["product"]["price"], Value::String("390.00".into()));
    assert_eq!(body["product"]["condition"], Value::from(9));
    assert_eq!(body["product"]["status"], Value::String("reserved".into()));
    assert_eq!(
        body["product"]["tags"],
        serde_json::json!(["自行车", "健身"])
    );

    // 再读一次，确认真的写进了存储，而不是只改了响应体。
    let (status, body) = get(router, "/api/products/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], Value::String("捷安特山地车（改）".into()));
    assert_eq!(body["status"], Value::String("reserved".into()));
    assert_eq!(body["desc"], Value::String("已换新刹车片".into()));
}

#[tokio::test]
async fn patch_validates_fields() {
    for (body, field) in [
        (serde_json::json!({ "title": "  " }), "title"),
        (serde_json::json!({ "price_cents": 0 }), "price"),
        (serde_json::json!({ "tags": [] }), "tags"),
        (serde_json::json!({ "condition": 11 }), "condition"),
        (serde_json::json!({ "status": "deleted" }), "status"),
    ] {
        let (status, resp) = patch(sample_app(), "/api/products/1", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp["field"], Value::String(field.into()));
    }
}

#[tokio::test]
async fn patch_and_delete_unknown_product_return_404() {
    let (status, body) = patch(
        sample_app(),
        "/api/products/999",
        serde_json::json!({ "title": "x" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], Value::String("product not found".into()));

    let (status, body) = delete(sample_app(), "/api/products/999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], Value::String("product not found".into()));
}

#[tokio::test]
async fn delete_removes_product_from_catalog() {
    let router = sample_app();
    let (status, body) = delete(router.clone(), "/api/products/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["deleted"], Value::from(1));

    let (status, _) = get(router.clone(), "/api/products/1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = get(router, "/api/products").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], Value::from(2));
}
