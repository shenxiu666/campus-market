//! 用真实种子数据跑一遍推荐引擎，打印索引规模、共现矩阵与推荐结果。
//!
//! 用途：① 演示前确认数据可用；② 把真实数字抄进 `docs/ALGORITHMS.md` / README。
//!
//! ```bash
//! cargo run -p campus-rec --release --example seed_stats
//! ```

use campus_rec::{Event, Product, QueryMode, Recommender, SortKey, User};

fn main() {
    let raw = std::fs::read_to_string("data/seed.json").expect("读 data/seed.json 失败");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("解析 JSON 失败");

    let products: Vec<Product> = serde_json::from_value(v["products"].clone()).unwrap();
    let users: Vec<User> = serde_json::from_value(v["users"].clone()).unwrap();
    let events: Vec<Event> = serde_json::from_value(v["events"].clone()).unwrap();

    let now = events.iter().map(|e| e.ts).max().unwrap_or(0) + 3600;
    let rec = Recommender::new(products.clone(), users.clone(), &events);
    let stats = rec.stats();

    println!("== 索引规模 ==");
    println!("商品数           = {}", stats.products);
    println!("用户数           = {}", stats.users);
    println!("标签数           = {}", stats.tags);
    println!("共现对数         = {}", stats.co_occurrence_pairs);
    println!("累计事件         = {}", stats.events);
    println!("最大共现次数     = {}", rec.co_occurrence.max_count());
    println!(
        "稠密矩阵格子数   = {} (N×N)",
        stats.products * stats.products
    );
    println!(
        "理论最大对子数   = {} (N(N-1)/2)",
        stats.products * (stats.products - 1) / 2
    );

    println!("\n== 热门标签 Top 8 ==");
    for (tag, n) in rec.index.tag_counts().into_iter().take(8) {
        println!("  {tag:<10} {n}");
    }

    println!("\n== 热度榜 Top 5 ==");
    for s in rec.hot_ranking(5, now) {
        let p = rec.product(s.product_id).unwrap();
        println!(
            "  #{:<3} {:<20} {:<8} score={:.4}",
            p.id,
            p.title,
            p.price_yuan(),
            s.score
        );
    }

    println!("\n== 为你推荐：用户 1（有兴趣标签）==");
    if let Some(u) = rec.user(1) {
        println!("  兴趣={:?} 期望价位={}分", u.interests, u.avg_price);
    }
    for s in rec.recommend_for_user(1, 5, &[], now) {
        let p = rec.product(s.product_id).unwrap();
        println!(
            "  #{:<3} {:<20} score={:.4} reasons={:?}",
            p.id, p.title, s.score, s.reasons
        );
    }

    println!("\n== 冷启动：用户 7（interests 为空）==");
    for s in rec.recommend_for_user(7, 3, &[], now) {
        println!(
            "  #{:<3} score={:.4} reasons={:?}",
            s.product_id, s.score, s.reasons
        );
    }

    println!("\n== 相似商品：商品 1 的 Top 3 ==");
    for s in rec.similar_products(1, 3, now) {
        println!(
            "  #{:<3} score={:.4} reasons={:?}",
            s.product_id, s.score, s.reasons
        );
    }

    println!("\n== 打分明细：用户 1 × 商品 2 ==");
    if let Some(e) = rec.explain(1, 2, now) {
        println!(
            "  score={:.4} = 0.45*{:.4} + 0.35*{:.4} + 0.15*{:.4} + 0.05*{:.4}",
            e.score, e.tag_sim, e.cf_sim, e.hot_norm, e.price_fit
        );
        println!(
            "  命中标签={:?} 相似用户={:?}",
            e.matched_tags, e.similar_users
        );
    }

    println!("\n== 搜索：标签「自行车」OR，按价格升序 ==");
    let hit = rec.query(
        &["自行车".to_string()],
        QueryMode::Or,
        None,
        None,
        None,
        SortKey::PriceAsc,
        now,
    );
    println!("  命中 {} 件", hit.len());
    for p in hit.iter().take(3) {
        println!("  #{:<3} {:<20} {}", p.id, p.title, p.price_yuan());
    }

    // 单次推荐调用耗时（含 hot_norms 全量归一化）
    let t = std::time::Instant::now();
    let n = 1000;
    for _ in 0..n {
        std::hint::black_box(rec.recommend_for_user(1, 10, &[], now));
    }
    println!(
        "\n== 性能 ==\nrecommend_for_user(limit=10)：{:.1} µs/次（{} 次平均，N={}）",
        t.elapsed().as_secs_f64() * 1e6 / n as f64,
        n,
        stats.products
    );
}
