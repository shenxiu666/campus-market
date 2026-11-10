//! 行为共现矩阵：`HashMap<(商品id, 商品id), 次数>`，item-based CF 的数据基础。
//!
//! **共现**的定义：同一用户在 [`CO_WINDOW_SECS`]（5 分钟）的时间窗口内先后交互过的
//! 两个商品，视为「一起被看过」。时间窗口很关键——没有窗口约束的话，
//! 一个用户一周内看过的所有商品都会两两配对，共现矩阵会退化成「全连接」，
//! 失去区分度（这也是本项目相比「全量用户-商品矩阵」的稀疏化手段）。
//!
//! **对称存储**：`(a,b)` 与 `(b,a)` 是同一件事，统一按 `(min,max)` 存储，
//! 空间直接减半，查询时也不用考虑顺序。

use crate::model::Event;
use std::collections::HashMap;

/// 共现窗口：同一用户在 `CO_WINDOW_SECS` 内交互过的两个商品视为共现。
pub const CO_WINDOW_SECS: i64 = 300;

/// 每个用户保留的最近行为条数上限。
///
/// `record` 是增量接口，必须记住用户最近看过什么才能和「下一个行为」配对。
/// 不设上限的话长期运行内存会无界增长，这里按「窗口内最多 256 条」截断。
pub const RECENT_CAP: usize = 256;

/// 共现统计。
#[derive(Debug, Default, Clone)]
pub struct CoOccurrence {
    /// 商品对 → 共现次数，键恒为 `(min, max)`。
    counts: HashMap<(u64, u64), u32>,
    /// 用户最近的行为 `(ts, product_id)`，升序，用于增量配对。
    recent: HashMap<u64, Vec<(i64, u64)>>,
    /// 当前最大共现次数，`score` 归一化时直接读，避免每次扫描全表。
    max_count: u32,
}

impl CoOccurrence {
    /// 构造空矩阵。
    pub fn new() -> Self {
        Self::default()
    }

    /// 用全量事件重建共现矩阵。
    ///
    /// 先按用户分组、按时间升序，再用滑动窗口对每个用户的事件做两两配对：
    /// 对第 `i` 条事件，只和它前面仍在窗口内的 `j` 配对，一旦 `ts[i]-ts[j] > 窗口`
    /// 就收缩窗口左端。复杂度 `O(Σ Eᵤ·w)`，`w` 为窗口内平均事件数（本项目数据里
    /// 通常是个位数），远好于朴素 `O(E²)`。
    pub fn build(events: &[Event]) -> Self {
        let mut co = CoOccurrence::new();

        let mut by_user: HashMap<u64, Vec<&Event>> = HashMap::new();
        for e in events {
            by_user.entry(e.user_id).or_default().push(e);
        }

        for (user_id, mut evs) in by_user {
            // 稳定排序：同一秒的事件保持输入顺序，保证结果可复现
            evs.sort_by_key(|e| e.ts);
            let mut left = 0usize;
            for right in 0..evs.len() {
                while evs[right].ts - evs[left].ts > CO_WINDOW_SECS {
                    left += 1;
                }
                for j in left..right {
                    co.bump(evs[j].product_id, evs[right].product_id);
                }
            }
            // 记录「窗口内最近的行为」，让后续 record 能接着配对
            let last_ts = evs.last().map(|e| e.ts).unwrap_or(0);
            let kept: Vec<(i64, u64)> = evs
                .iter()
                .filter(|e| last_ts - e.ts <= CO_WINDOW_SECS)
                .map(|e| (e.ts, e.product_id))
                .collect();
            if !kept.is_empty() {
                co.recent.insert(user_id, kept);
            }
        }
        co
    }

    /// 增量记录一次行为。
    ///
    /// 与「该用户窗口内已记录的行为」两两配对。若事件时间戳早于已记录的行为
    /// （乱序到达），仍然会配对，只是不会反向清理更早的记录。
    pub fn record(&mut self, event: &Event) {
        let uid = event.user_id;
        let ts = event.ts;
        let pid = event.product_id;

        // 先把要配对的商品收集出来，再统一更新计数：
        // 避免「持有 recent 的可变借用」和「可变借用 counts」同时存在。
        let partners: Vec<u64> = {
            let recent = self.recent.entry(uid).or_default();
            // 丢弃滑出窗口的记录（按 ts 升序，从头部弹出）
            let cutoff = ts - CO_WINDOW_SECS;
            let drop_n = recent.iter().take_while(|(t, _)| *t < cutoff).count();
            if drop_n > 0 {
                recent.drain(0..drop_n);
            }
            let partners: Vec<u64> = recent
                .iter()
                .filter(|(t, _)| (t - ts).abs() <= CO_WINDOW_SECS)
                .map(|(_, p)| *p)
                .collect();
            recent.push((ts, pid));
            if recent.len() > RECENT_CAP {
                let overflow = recent.len() - RECENT_CAP;
                recent.drain(0..overflow);
            }
            partners
        };

        for other in partners {
            self.bump(other, pid);
        }
    }

    /// 内部：规范化键 `(min,max)` 后自增，并同步维护 `max_count`。
    fn bump(&mut self, a: u64, b: u64) {
        if a == b {
            // 同一个商品和自己不算共现
            return;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        let c = self.counts.entry(key).or_insert(0);
        *c = c.saturating_add(1);
        if *c > self.max_count {
            self.max_count = *c;
        }
    }

    /// 与 `product_id` 共现最多的商品（降序）。
    ///
    /// 返回 `(商品 id, 共现次数)`；次数相同按 id 升序，保证输出确定。
    /// 复杂度 `O(P)`，`P` 为共现对数量——本项目 `P` 在数百量级，
    /// 直接用扁平 `HashMap` 扫一遍比维护邻接表更省内存（见 `docs/ALGORITHMS.md`）。
    pub fn neighbors(&self, product_id: u64, limit: usize) -> Vec<(u64, u32)> {
        if limit == 0 {
            return Vec::new();
        }
        let mut out: Vec<(u64, u32)> = Vec::new();
        for (&(a, b), &c) in &self.counts {
            if a == product_id {
                out.push((b, c));
            } else if b == product_id {
                out.push((a, c));
            }
        }
        out.sort_unstable_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
        out.truncate(limit);
        out
    }

    /// 两个商品的共现次数。
    pub fn get(&self, a: u64, b: u64) -> u32 {
        if a == b {
            return 0;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        self.counts.get(&key).copied().unwrap_or(0)
    }

    /// 归一化共现强度 `count / max_count ∈ [0,1]`。
    ///
    /// 用「全局最大次数」做分母而不是 `count/(count+1)`：后者在次数分布很集中时
    /// （本项目常见 1~3 次）几乎所有值都挤在 0.5 附近，区分度极差；
    /// 除以最大值能让最强的共现对拿到 1.0，便于和 `tag_sim` 等指标线性加权。
    pub fn score(&self, a: u64, b: u64) -> f64 {
        if self.max_count == 0 {
            return 0.0;
        }
        self.get(a, b) as f64 / self.max_count as f64
    }

    /// 全矩阵最大共现次数。
    pub fn max_count(&self) -> u32 {
        self.max_count
    }

    /// 共现对数量。
    pub fn len(&self) -> usize {
        self.counts.len()
    }

    /// 矩阵是否为空。
    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
}

/// 用户之间的相似度：按**共同交互过的商品数**降序。
///
/// `user_items` 是 `用户 id → 交互过的商品 id（升序去重）`。
/// 返回 `(用户 id, 共同商品数)`，不含 `uid` 自己，也不含共同商品为 0 的用户。
/// 共同商品数相同时按用户 id 升序，保证输出确定。
///
/// 复杂度 `O(U·m)`，`U` 为用户数、`m` 为该用户的商品数（用哈希表做 membership 测试）。
pub fn similar_users(user_items: &HashMap<u64, Vec<u64>>, uid: u64, k: usize) -> Vec<(u64, usize)> {
    if k == 0 {
        return Vec::new();
    }
    let mine = match user_items.get(&uid) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let my_set: std::collections::HashSet<u64> = mine.iter().copied().collect();

    let mut out: Vec<(u64, usize)> = Vec::new();
    for (&other, items) in user_items {
        if other == uid {
            continue;
        }
        let shared = items.iter().filter(|p| my_set.contains(p)).count();
        if shared > 0 {
            out.push((other, shared));
        }
    }
    out.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EventKind;

    fn ev(user_id: u64, product_id: u64, kind: EventKind, ts: i64) -> Event {
        Event {
            user_id,
            product_id,
            kind,
            ts,
        }
    }

    #[test]
    fn empty_build() {
        let co = CoOccurrence::build(&[]);
        assert!(co.is_empty());
        assert_eq!(co.len(), 0);
        assert_eq!(co.get(1, 2), 0);
        assert_eq!(co.score(1, 2), 0.0);
        assert!(co.neighbors(1, 5).is_empty());
    }

    #[test]
    fn within_window_co_occurs() {
        let events = vec![
            ev(1, 10, EventKind::View, 1000),
            ev(1, 20, EventKind::Click, 1000 + CO_WINDOW_SECS),
        ];
        let co = CoOccurrence::build(&events);
        assert_eq!(co.get(10, 20), 1);
        // 对称
        assert_eq!(co.get(20, 10), 1);
        assert_eq!(co.len(), 1);
    }

    #[test]
    fn outside_window_does_not_co_occur() {
        let events = vec![
            ev(1, 10, EventKind::View, 1000),
            ev(1, 20, EventKind::Click, 1000 + CO_WINDOW_SECS + 1),
        ];
        let co = CoOccurrence::build(&events);
        assert_eq!(co.get(10, 20), 0);
        assert!(co.is_empty());
    }

    #[test]
    fn different_users_do_not_co_occur() {
        let events = vec![
            ev(1, 10, EventKind::View, 1000),
            ev(2, 20, EventKind::View, 1001),
        ];
        let co = CoOccurrence::build(&events);
        assert!(co.is_empty());
    }

    #[test]
    fn same_product_twice_is_ignored() {
        let events = vec![
            ev(1, 10, EventKind::View, 1000),
            ev(1, 10, EventKind::Click, 1010),
        ];
        let co = CoOccurrence::build(&events);
        assert_eq!(co.get(10, 10), 0);
        assert!(co.is_empty());
    }

    #[test]
    fn three_events_in_window_make_three_pairs() {
        let events = vec![
            ev(1, 1, EventKind::View, 1000),
            ev(1, 2, EventKind::View, 1010),
            ev(1, 3, EventKind::View, 1020),
        ];
        let co = CoOccurrence::build(&events);
        assert_eq!(co.len(), 3);
        assert_eq!(co.get(1, 2), 1);
        assert_eq!(co.get(1, 3), 1);
        assert_eq!(co.get(2, 3), 1);
    }

    #[test]
    fn sliding_window_shrinks() {
        // 1、2 在窗口内；3 距离 1 超过窗口，只与 2 配对
        let events = vec![
            ev(1, 1, EventKind::View, 0),
            ev(1, 2, EventKind::View, CO_WINDOW_SECS),
            ev(1, 3, EventKind::View, CO_WINDOW_SECS * 2),
        ];
        let co = CoOccurrence::build(&events);
        assert_eq!(co.get(1, 2), 1);
        assert_eq!(co.get(2, 3), 1);
        assert_eq!(co.get(1, 3), 0);
    }

    #[test]
    fn repeated_pairs_accumulate() {
        // 5 个独立的时间窗口，每个窗口内 10、20 各交互一次 → 共现 5 次
        let mut co = CoOccurrence::new();
        for k in 0..5 {
            let base = 100_000 * (k + 1);
            co.record(&ev(1, 10, EventKind::View, base));
            co.record(&ev(1, 20, EventKind::View, base + 10));
        }
        assert_eq!(co.get(10, 20), 5);
        assert_eq!(co.max_count(), 5);
        assert!((co.score(10, 20) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn same_window_each_pair_counted_once() {
        // 窗口内 5 条事件 → 两两配对共 5*4/2 = 10 对，每对计 1 次
        let mut co = CoOccurrence::new();
        for i in 0..5u64 {
            co.record(&ev(1, 10 + i, EventKind::View, 1000 + i as i64));
        }
        assert_eq!(co.len(), 10);
        for a in 10..15u64 {
            for b in (a + 1)..15 {
                assert_eq!(co.get(a, b), 1, "pair ({a},{b})");
                assert_eq!(co.get(b, a), 1, "pair 对称 ({b},{a})");
            }
        }
    }

    #[test]
    fn record_matches_build() {
        // 增量 record 与批量 build 必须得到同一个矩阵
        let events = vec![
            ev(1, 10, EventKind::View, 1000),
            ev(1, 20, EventKind::Click, 1100),
            ev(1, 30, EventKind::Fav, 1200),
            ev(2, 10, EventKind::View, 2000),
            ev(2, 40, EventKind::View, 2100),
        ];
        let built = CoOccurrence::build(&events);
        let mut recorded = CoOccurrence::new();
        for e in &events {
            recorded.record(e);
        }
        assert_eq!(built.len(), recorded.len());
        for (a, b, c) in [(10u64, 20u64, 1u32), (10, 30, 1), (20, 30, 1), (10, 40, 1)] {
            assert_eq!(built.get(a, b), c, "build ({a},{b})");
            assert_eq!(recorded.get(a, b), c, "record ({a},{b})");
        }
        assert_eq!(recorded.get(20, 40), 0);
    }

    #[test]
    fn record_after_build_continues_pairing() {
        let mut co = CoOccurrence::build(&[ev(1, 10, EventKind::View, 1000)]);
        assert!(co.is_empty());
        co.record(&ev(1, 20, EventKind::View, 1010));
        assert_eq!(co.get(10, 20), 1);
    }

    #[test]
    fn record_prunes_old_recent() {
        let mut co = CoOccurrence::new();
        co.record(&ev(1, 10, EventKind::View, 0));
        // 时间跳到窗口之外，旧记录应被清理，不再与新事件配对
        co.record(&ev(1, 20, EventKind::View, CO_WINDOW_SECS * 10));
        assert_eq!(co.get(10, 20), 0);
    }

    #[test]
    fn record_caps_recent_len() {
        let mut co = CoOccurrence::new();
        for i in 0..(RECENT_CAP as i64 + 50) {
            co.record(&ev(1, i as u64 + 1, EventKind::View, i));
        }
        assert!(co.recent.get(&1).map(|r| r.len()).unwrap_or(0) <= RECENT_CAP);
    }

    #[test]
    fn neighbors_sorted_desc() {
        let events = vec![
            ev(1, 1, EventKind::View, 1000),
            ev(1, 2, EventKind::View, 1001),
            ev(1, 3, EventKind::View, 1002),
            ev(2, 1, EventKind::View, 3000),
            ev(2, 2, EventKind::View, 3001),
            ev(3, 1, EventKind::View, 5000),
            ev(3, 3, EventKind::View, 5001),
        ];
        let co = CoOccurrence::build(&events);
        let nb = co.neighbors(1, 10);
        assert_eq!(nb[0], (2, 2)); // 1-2 共现 2 次
        assert_eq!(nb[1], (3, 2)); // 1-3 共现 2 次，次数相同按 id 升序
        assert_eq!(co.neighbors(1, 1).len(), 1);
        assert!(co.neighbors(999, 5).is_empty());
        assert!(co.neighbors(1, 0).is_empty());
    }

    #[test]
    fn score_is_relative_to_max() {
        // 两个独立窗口：窗口 A 里 1-2 配 1 次；窗口 B 里 1-3 配 3 次
        let mut co = CoOccurrence::new();
        co.record(&ev(1, 1, EventKind::View, 1000));
        co.record(&ev(1, 2, EventKind::View, 1001));
        for k in 0..3 {
            let base = 100_000 * (k + 1);
            co.record(&ev(2, 1, EventKind::View, base));
            co.record(&ev(2, 3, EventKind::View, base + 1));
        }
        assert_eq!(co.get(1, 2), 1);
        assert_eq!(co.get(1, 3), 3);
        assert!((co.score(1, 3) - 1.0).abs() < 1e-12);
        assert!((co.score(1, 2) - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(co.score(1, 1), 0.0);
    }
}
