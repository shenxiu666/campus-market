//! 热度打分：Hacker News 式重力衰减。
//!
//! 直觉：一个商品越受欢迎（浏览/点击/收藏越多）分越高，但**时间会不断拉低它**。
//! 用幂函数 `(age + 2)^1.5` 做分母而不是指数衰减 `e^{-λt}`：
//!
//! * 幂律衰减在「新」的时候下降快、久了之后下降慢，符合二手交易的实际规律
//!   （刚发布 1 小时和 2 小时差别很大，30 天和 31 天差别不大）；
//! * 不需要选半衰期参数，也没有下溢问题；
//! * 常数 `+2` 保证 `age=0` 时不会除零，同时给刚发布的商品一个「冷启动缓冲」。

use crate::model::Product;

/// 重力指数：越大衰减越快。
pub const GRAVITY: f64 = 1.5;

/// 浏览权重。
pub const W_VIEW: f64 = 1.0;
/// 点击权重（比浏览更强烈的兴趣信号）。
pub const W_CLICK: f64 = 3.0;
/// 收藏权重（最强的正向信号）。
pub const W_FAV: f64 = 5.0;

/// 分母的时间缓冲常数（小时），防止 `age=0` 时除零。
pub const TIME_OFFSET: f64 = 2.0;

/// 热度分 `(views + 3*clicks + 5*favs) / (age_hours + 2)^GRAVITY`。
///
/// `age_hours <= 0`（未来时间戳/时钟回拨）按 0 处理，避免除零与负指数。
///
/// 时间 `O(1)`，空间 `O(1)`。
pub fn hot_score(product: &Product, now: i64) -> f64 {
    let numerator = W_VIEW * product.views as f64
        + W_CLICK * product.clicks as f64
        + W_FAV * product.favs as f64;
    let age_secs = (now - product.created_at).max(0) as f64;
    let age_hours = age_secs / 3600.0;
    numerator / (age_hours + TIME_OFFSET).powf(GRAVITY)
}

/// 把一组分数线性归一到 [0,1]；全为 0 时返回全 0。
///
/// 采用 min-max 归一化 `(x - min) / (max - min)`。注意：
///
/// * `max == min`（全部相同）时无法拉伸，统一给 0——否则会出现「全都满分」，
///   混合打分里热度的 0.15 权重就失去意义；
/// * 负数分数也能正确处理（推荐场景里不会出现，但保持函数通用）。
pub fn normalize_scores(scores: &mut [f64]) {
    if scores.is_empty() {
        return;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for &s in scores.iter() {
        if s < min {
            min = s;
        }
        if s > max {
            max = s;
        }
    }
    if !min.is_finite() || !max.is_finite() {
        // 含 NaN/无穷时不做归一化，避免把脏数据扩散到所有元素
        return;
    }
    let span = max - min;
    if span <= f64::EPSILON {
        for s in scores.iter_mut() {
            *s = 0.0;
        }
        return;
    }
    for s in scores.iter_mut() {
        *s = (*s - min) / span;
    }
}

/// 单点归一化：把 `value` 按 `[lo, hi]` 线性映射到 `[0,1]` 并 clamp。
///
/// 用于「候选集之外没有参照系」的场合（例如只有一个候选商品时，
/// [`normalize_scores`] 的 min-max 会退化成全 0，此时用绝对区间更稳）。
pub fn normalize_one(value: f64, lo: f64, hi: f64) -> f64 {
    if hi <= lo {
        return 0.0;
    }
    ((value - lo) / (hi - lo)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::product;

    const NOW: i64 = 1_700_000_000;

    fn with_stats(id: u64, views: u64, clicks: u64, favs: u64, age_secs: i64) -> Product {
        let mut p = product(id, 1000, NOW - age_secs, 1, &[]);
        p.views = views;
        p.clicks = clicks;
        p.favs = favs;
        p
    }

    #[test]
    fn zero_interaction_is_zero() {
        let p = with_stats(1, 0, 0, 0, 0);
        assert_eq!(hot_score(&p, NOW), 0.0);
    }

    #[test]
    fn weights_are_applied() {
        // 同样 1 次行为、同样时间：fav 分 > click 分 > view 分
        let v = hot_score(&with_stats(1, 1, 0, 0, 3600), NOW);
        let c = hot_score(&with_stats(1, 0, 1, 0, 3600), NOW);
        let f = hot_score(&with_stats(1, 0, 0, 1, 3600), NOW);
        assert!(f > c && c > v, "f={f} c={c} v={v}");
        // 权重比例精确为 1:3:5
        assert!((c / v - 3.0).abs() < 1e-12);
        assert!((f / v - 5.0).abs() < 1e-12);
    }

    #[test]
    fn exact_formula() {
        // (0 + 3*2 + 5*1) / (3h + 2)^1.5
        let p = with_stats(1, 0, 2, 1, 3 * 3600);
        let expect = (3.0 * 2.0 + 5.0) / 5.0_f64.powf(1.5);
        assert!((hot_score(&p, NOW) - expect).abs() < 1e-12);
    }

    #[test]
    fn older_is_lower() {
        let fresh = with_stats(1, 10, 0, 0, 60);
        let old = with_stats(1, 10, 0, 0, 3600 * 24 * 10);
        assert!(hot_score(&fresh, NOW) > hot_score(&old, NOW));
    }

    #[test]
    fn future_timestamp_is_clamped() {
        // created_at 在未来：age 按 0 处理，结果有限且等于 age=0 的分
        let future = with_stats(1, 10, 0, 0, -3600 * 24);
        let now_p = with_stats(1, 10, 0, 0, 0);
        let a = hot_score(&future, NOW);
        let b = hot_score(&now_p, NOW);
        assert!(a.is_finite() && b.is_finite());
        assert!((a - b).abs() < 1e-12);
    }

    #[test]
    fn monotonic_in_interactions() {
        let mut prev = -1.0;
        for views in [0u64, 1, 10, 100, 1000] {
            let s = hot_score(&with_stats(1, views, 0, 0, 3600), NOW);
            assert!(s > prev, "views={views} 时热度未单调递增");
            prev = s;
        }
    }

    #[test]
    fn normalize_empty_and_all_zero() {
        let mut v: Vec<f64> = Vec::new();
        normalize_scores(&mut v);
        assert!(v.is_empty());

        let mut zeros = vec![0.0, 0.0, 0.0];
        normalize_scores(&mut zeros);
        assert_eq!(zeros, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn normalize_basic() {
        let mut v = vec![1.0, 2.0, 3.0, 5.0];
        normalize_scores(&mut v);
        assert!((v[0] - 0.0).abs() < 1e-12);
        assert!((v[1] - 0.25).abs() < 1e-12);
        assert!((v[2] - 0.5).abs() < 1e-12);
        assert!((v[3] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn normalize_all_equal_gives_zero() {
        let mut v = vec![7.0, 7.0, 7.0];
        normalize_scores(&mut v);
        assert_eq!(v, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn normalize_single_element() {
        let mut v = vec![42.0];
        normalize_scores(&mut v);
        assert_eq!(v, vec![0.0]);
    }

    #[test]
    fn normalize_negative_values() {
        let mut v = vec![-2.0, 0.0, 2.0];
        normalize_scores(&mut v);
        assert!((v[0] - 0.0).abs() < 1e-12);
        assert!((v[1] - 0.5).abs() < 1e-12);
        assert!((v[2] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn normalize_result_in_range() {
        let mut v: Vec<f64> = (0..100).map(|i| (i as f64).sin() * 1000.0).collect();
        normalize_scores(&mut v);
        for (i, s) in v.iter().enumerate() {
            assert!((0.0..=1.0).contains(s), "idx={i} val={s}");
        }
    }

    #[test]
    fn normalize_one_clamps() {
        assert_eq!(normalize_one(5.0, 0.0, 10.0), 0.5);
        assert_eq!(normalize_one(-1.0, 0.0, 10.0), 0.0);
        assert_eq!(normalize_one(99.0, 0.0, 10.0), 1.0);
        // 区间非法 → 0
        assert_eq!(normalize_one(1.0, 5.0, 5.0), 0.0);
        assert_eq!(normalize_one(1.0, 5.0, 1.0), 0.0);
    }
}
