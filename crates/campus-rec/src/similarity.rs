//! 相似度度量：Jaccard 与余弦相似度。
//!
//! 两者都是「集合/向量相似度」，但适用场景不同：
//!
//! * **Jaccard** 只看**是否出现**，不看权重与次数，适合标签集合这类
//!   「有/无」语义（一件商品要么带「自行车」标签，要么不带）。
//! * **余弦** 看**权重与强度**，适合行为向量这类「多少次」语义
//!   （用户 A 看了 5 次、用户 B 看了 1 次，强度不同）。
//!
//! 详细推导见 `docs/ALGORITHMS.md`。

use std::collections::HashSet;

/// Jaccard 相似度 `|A ∩ B| / |A ∪ B|`。
///
/// 两个集合都为空时定义为 0（而非 1），避免「零信息」被判为完全相似。
/// 输入允许有重复元素，内部按集合语义去重。
///
/// 时间 `O(|A| + |B|)`，空间 `O(|A| + |B|)`。
pub fn jaccard(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let sa: HashSet<&str> = a.iter().map(|s| s.as_str()).collect();
    let sb: HashSet<&str> = b.iter().map(|s| s.as_str()).collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count();
    if inter == 0 {
        return 0.0;
    }
    // |A ∪ B| = |A| + |B| - |A ∩ B|，不必真的构造并集
    let union = sa.len() + sb.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Jaccard 相似度（字符串切片版，省一次 `Vec<String>` 分配）。
pub fn jaccard_str(a: &[&str], b: &[&str]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let sa: HashSet<&str> = a.iter().copied().collect();
    let sb: HashSet<&str> = b.iter().copied().collect();
    let inter = sa.intersection(&sb).count();
    if inter == 0 {
        return 0.0;
    }
    let union = sa.len() + sb.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Jaccard 相似度（零分配版本，供打分/重排热路径使用）。
///
/// 语义与 [`jaccard`] **完全一致**（含集合去重、含「两边都空返回 0」），
/// 但用 `O(|A|·|B|)` 的两两比较代替两个 `HashSet`。
///
/// **为什么值得单独写一个**：标签集合通常只有 1~5 个元素，此时建两个
/// `HashSet` 的分配 + 哈希开销比直接比较贵一到两个数量级。推荐器的重排
/// 循环里，这个函数要被调用 `|候选| × limit × 已选` 次（本项目约 5000 次），
/// 每次两次堆分配会直接成为性能瓶颈——这是实测出来的，不是猜的
/// （见 `docs/ALGORITHMS.md` §7.5）。
///
/// 时间 `O(|A|·|B|)`，空间 `O(1)`。
pub fn jaccard_no_alloc(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    // 统计去重后的交集大小与两侧去重后的大小。
    let mut inter = 0usize;
    let mut distinct_a = 0usize;
    for (i, x) in a.iter().enumerate() {
        if a[..i].contains(x) {
            continue; // 已统计过这个元素
        }
        distinct_a += 1;
        if b.contains(x) {
            inter += 1;
        }
    }
    let mut distinct_b = 0usize;
    for (i, y) in b.iter().enumerate() {
        if b[..i].contains(y) {
            continue;
        }
        distinct_b += 1;
    }
    if inter == 0 {
        return 0.0;
    }
    let union = distinct_a + distinct_b - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// 余弦相似度，输入为稀疏向量（下标 → 权重）。
///
/// `cos(A,B) = Σ aᵢbᵢ / (‖A‖·‖B‖)`，只遍历非零分量，因此稀疏向量的
/// 复杂度是 `O(nnz(A) + nnz(B))` 而不是稠密的 `O(d)`（d 为维度）。
///
/// 任一向量为零向量（空或全零）时返回 0——零向量的方向没有定义。
/// 输入**无需排序**：内部用哈希表对齐下标，避免调用方维护有序性。
pub fn cosine(a: &[(u64, f64)], b: &[(u64, f64)]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    // 把较短的一侧建表，较长的一侧做探测：空间 O(min)，时间 O(m+n)
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut table: std::collections::HashMap<u64, f64> =
        std::collections::HashMap::with_capacity(short.len());
    for &(idx, w) in short {
        // 同一分量重复出现时累加（稀疏向量表示允许分片）
        *table.entry(idx).or_insert(0.0) += w;
    }

    let mut dot = 0.0f64;
    for &(idx, w) in long {
        if let Some(&x) = table.get(&idx) {
            dot += x * w;
        }
    }

    let norm_a = a.iter().map(|(_, w)| w * w).sum::<f64>().sqrt();
    let norm_b = b.iter().map(|(_, w)| w * w).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    let sim = dot / (norm_a * norm_b);
    // 浮点误差可能让结果略微越界，夹到 [-1, 1]
    sim.clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Rng;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn jaccard_basic() {
        assert!((jaccard(&s(&["a", "b"]), &s(&["a", "b"])) - 1.0).abs() < 1e-12);
        assert!((jaccard(&s(&["a", "b"]), &s(&["a", "b", "c"])) - 2.0 / 3.0).abs() < 1e-12);
        assert!((jaccard(&s(&["a"]), &s(&["b"])) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn jaccard_empty_is_zero() {
        assert_eq!(jaccard(&[], &[]), 0.0);
        assert_eq!(jaccard(&s(&["a"]), &[]), 0.0);
        assert_eq!(jaccard(&[], &s(&["a"])), 0.0);
    }

    #[test]
    fn jaccard_dedups_duplicates() {
        // 重复元素按集合语义处理：{a} vs {a} 仍然是 1.0
        assert!((jaccard(&s(&["a", "a", "a"]), &s(&["a"])) - 1.0).abs() < 1e-12);
        // {a,b} vs {b,b,c} → 1/3
        assert!((jaccard(&s(&["a", "b"]), &s(&["b", "b", "c"])) - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn jaccard_is_symmetric() {
        let a = s(&["自行车", "运动", "书"]);
        let b = s(&["自行车", "书"]);
        assert!((jaccard(&a, &b) - jaccard(&b, &a)).abs() < 1e-12);
    }

    #[test]
    fn jaccard_range_and_self() {
        let cases = [
            (s(&["a"]), s(&["b", "c"])),
            (s(&["a", "b"]), s(&["a", "b", "c", "d"])),
            (s(&["x"]), s(&["x"])),
        ];
        for (a, b) in cases {
            let j = jaccard(&a, &b);
            assert!((0.0..=1.0).contains(&j), "jaccard 越界: {j}");
            assert!((jaccard(&a, &a) - 1.0).abs() < 1e-12, "自相似必须为 1");
        }
    }

    #[test]
    fn jaccard_str_matches_vec_version() {
        let a = s(&["x", "y", "z"]);
        let b = s(&["y", "z", "w"]);
        let ar: Vec<&str> = a.iter().map(|x| x.as_str()).collect();
        let br: Vec<&str> = b.iter().map(|x| x.as_str()).collect();
        assert!((jaccard(&a, &b) - jaccard_str(&ar, &br)).abs() < 1e-12);
        assert_eq!(jaccard_str(&[], &br), 0.0);
    }

    #[test]
    fn cosine_basic() {
        // 同向 → 1
        assert!((cosine(&[(1, 1.0), (2, 2.0)], &[(1, 1.0), (2, 2.0)]) - 1.0).abs() < 1e-12);
        // 正交 → 0
        assert!(cosine(&[(1, 1.0)], &[(2, 1.0)]).abs() < 1e-12);
        // 已知值：a=(1,0) b=(1,1) → 1/√2
        let got = cosine(&[(0, 1.0)], &[(0, 1.0), (1, 1.0)]);
        assert!((got - 1.0 / 2.0_f64.sqrt()).abs() < 1e-12, "got={got}");
    }

    #[test]
    fn cosine_empty_and_zero_vector() {
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[(1, 1.0)], &[]), 0.0);
        assert_eq!(cosine(&[], &[(1, 1.0)]), 0.0);
        // 全零向量：方向未定义 → 0
        assert_eq!(cosine(&[(1, 0.0)], &[(1, 1.0)]), 0.0);
        assert_eq!(cosine(&[(1, 0.0)], &[(2, 0.0)]), 0.0);
    }

    #[test]
    fn cosine_unsorted_input() {
        // 输入顺序不影响结果
        let a = [(3u64, 2.0), (1, 1.0)];
        let b = [(1, 1.0), (3, 2.0)];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn cosine_duplicate_index_accumulates() {
        // (1,1.0) 出现两次等价于 (1,2.0)
        let a = [(1u64, 1.0), (1, 1.0)];
        let b = [(1u64, 2.0)];
        // dot 累加后 = 2*2 = 4；‖a‖ = 2，‖b‖ = 2 → 1.0
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn cosine_is_symmetric_and_bounded() {
        let a = [(1u64, 0.5), (2, 1.5), (7, 3.0)];
        let b = [(2u64, 2.0), (5, 1.0)];
        let x = cosine(&a, &b);
        let y = cosine(&b, &a);
        assert!((x - y).abs() < 1e-12);
        assert!((-1.0..=1.0).contains(&x));
    }

    #[test]
    fn jaccard_no_alloc_matches_jaccard() {
        let cases: Vec<(Vec<String>, Vec<String>)> = vec![
            (vec![], vec![]),
            (vec![], s(&["a"])),
            (s(&["a"]), vec![]),
            (s(&["a"]), s(&["a"])),
            (s(&["a"]), s(&["b"])),
            (s(&["a", "b"]), s(&["b", "c"])),
            (s(&["a", "a", "b"]), s(&["a", "b", "b"])),
            (s(&["x"]), s(&["x", "y", "z"])),
            (s(&["a", "b", "c"]), s(&["a", "b", "c"])),
        ];
        for (a, b) in cases {
            let want = jaccard(&a, &b);
            let got = jaccard_no_alloc(&a, &b);
            assert!(
                (got - want).abs() < 1e-12,
                "a={a:?} b={b:?} got={got} want={want}"
            );
        }
    }

    #[test]
    fn jaccard_no_alloc_matches_jaccard_random() {
        let mut rng = Rng::new(0x5EED_1234);
        let pool = ["a", "b", "c", "d", "e", "f"];
        for _ in 0..300 {
            let na = rng.below(5) as usize;
            let nb = rng.below(5) as usize;
            let mut a: Vec<String> = (0..na)
                .map(|_| pool[rng.below(pool.len() as u64) as usize].to_string())
                .collect();
            let mut b: Vec<String> = (0..nb)
                .map(|_| pool[rng.below(pool.len() as u64) as usize].to_string())
                .collect();
            a.sort();
            b.sort();
            let want = jaccard(&a, &b);
            let got = jaccard_no_alloc(&a, &b);
            assert!((got - want).abs() < 1e-12, "a={a:?} b={b:?}");
        }
    }

    #[test]
    fn cosine_opposite_direction_is_negative() {
        let a = [(1u64, 1.0)];
        let b = [(1u64, -1.0)];
        assert!((cosine(&a, &b) + 1.0).abs() < 1e-12);
    }
}
