//! 标签倒排索引：`HashMap<标签, Vec<商品id>>`。
//!
//! posting list 始终按商品 id 升序维护，因此求交/求并可以用双指针归并，复杂度 O(m+n)。
//!
//! 为什么不用 `HashSet`：`HashSet` 求交要遍历较小集合并逐个哈希查找，
//! 每次查找平均 O(1) 但常数大（哈希 + 探测 + 缓存缺失），而且结果无序、
//! 还需要额外排序才能保证输出的确定性。倒排表天然有序，双指针归并是
//! **顺序访存**，对 CPU 缓存友好，且结果直接就是升序，无需再排。

use crate::model::Product;
use std::collections::HashMap;

/// 查询模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    /// 交集（AND）：商品必须带全部标签
    And,
    /// 并集（OR）：命中任一标签即可
    Or,
}

/// 倒排索引。
#[derive(Debug, Default, Clone)]
pub struct InvertedIndex {
    postings: HashMap<String, Vec<u64>>,
}

impl InvertedIndex {
    /// 构造空索引。
    pub fn new() -> Self {
        Self::default()
    }

    /// 由商品列表整体重建索引。
    ///
    /// 复杂度 `O(N·T + Σ|posting|·log)`，其中 N 为商品数、T 为平均标签数。
    /// 因为一次性构建时商品 id 顺序未知，插入后再各自排序一次，
    /// 比逐个 `upsert` 二分插入更快。
    pub fn build(products: &[Product]) -> Self {
        let mut postings: HashMap<String, Vec<u64>> = HashMap::new();
        for p in products {
            for tag in &p.tags {
                postings.entry(tag.clone()).or_default().push(p.id);
            }
        }
        for list in postings.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        InvertedIndex { postings }
    }

    /// 插入/更新单个商品的标签。
    ///
    /// 用二分查找定位插入点，单次 `O(T·log|posting|)`；幂等（重复 upsert 不会产生重复 id）。
    pub fn upsert(&mut self, product: &Product) {
        for tag in &product.tags {
            let list = self.postings.entry(tag.clone()).or_default();
            // partition_point 返回第一个 > id 的位置；若该位置前一个就是 id 说明已存在
            let pos = list.partition_point(|&x| x < product.id);
            if list.get(pos) != Some(&product.id) {
                list.insert(pos, product.id);
            }
        }
    }

    /// 移除商品的标签。
    ///
    /// posting 变空时把整个键删掉，避免 `len()` 把「没有任何商品的标签」也算进去，
    /// 也避免 `tag_counts()` 输出一堆 0 计数的噪音。
    pub fn remove(&mut self, product_id: u64, tags: &[String]) {
        for tag in tags {
            let mut drop_key = false;
            if let Some(list) = self.postings.get_mut(tag) {
                if let Ok(pos) = list.binary_search(&product_id) {
                    list.remove(pos);
                }
                drop_key = list.is_empty();
            }
            if drop_key {
                self.postings.remove(tag);
            }
        }
    }

    /// 单个标签的 posting list；标签不存在时返回空切片。
    pub fn get(&self, tag: &str) -> &[u64] {
        match self.postings.get(tag) {
            Some(list) => list.as_slice(),
            None => &[],
        }
    }

    /// 标签数量。
    pub fn len(&self) -> usize {
        self.postings.len()
    }

    /// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }

    /// 所有标签及其商品数，按商品数降序。
    ///
    /// 商品数相同则按标签字典序升序，保证输出确定（HashMap 迭代顺序是随机的）。
    pub fn tag_counts(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = self
            .postings
            .iter()
            .map(|(tag, list)| (tag.clone(), list.len()))
            .collect();
        out.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// 多标签查询，结果按商品 id 升序。
    ///
    /// * `QueryMode::And`：从最短的 posting list 开始两两求交（见 [`intersect_all`]）。
    /// * `QueryMode::Or`：两两求并。
    ///
    /// 空标签列表返回空结果——「没有任何筛选条件」由调用方（`Recommender::query`）
    /// 解释为「全部商品」，而不是在这里返回全部 id（索引并不知道商品总数）。
    pub fn search(&self, tags: &[String], mode: QueryMode) -> Vec<u64> {
        if tags.is_empty() {
            return Vec::new();
        }
        match mode {
            QueryMode::And => intersect_all(&tags.iter().map(|t| self.get(t)).collect::<Vec<_>>()),
            QueryMode::Or => union_all(&tags.iter().map(|t| self.get(t)).collect::<Vec<_>>()),
        }
    }
}

/// 多个升序列表求交。
///
/// **按长度升序排序后再两两归并**：交集大小不会超过最短列表，
/// 先和最长的列表求交会白白扫描大量不可能命中的元素。
pub fn intersect_all(lists: &[&[u64]]) -> Vec<u64> {
    let mut non_empty: Vec<&[u64]> = lists.iter().copied().filter(|l| !l.is_empty()).collect();
    if non_empty.len() != lists.len() {
        // 有任一列表为空 → 交集必然为空
        return Vec::new();
    }
    if non_empty.is_empty() {
        return Vec::new();
    }
    non_empty.sort_unstable_by_key(|l| l.len());
    let mut acc = non_empty[0].to_vec();
    for list in &non_empty[1..] {
        if acc.is_empty() {
            return Vec::new();
        }
        acc = intersect_sorted(&acc, list);
    }
    acc
}

/// 多个升序列表求并。
pub fn union_all(lists: &[&[u64]]) -> Vec<u64> {
    let mut acc: Vec<u64> = Vec::new();
    for list in lists {
        acc = union_sorted(&acc, list);
    }
    acc
}

/// 两个升序 id 列表求交（双指针归并）。
///
/// 时间 `O(m + n)`，空间 `O(min(m, n))`（结果）。
pub fn intersect_sorted(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// 两个升序 id 列表求并（双指针归并去重）。
///
/// 时间 `O(m + n)`，空间 `O(m + n)`（结果）。
pub fn union_sorted(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Rng, product};
    use std::collections::HashSet;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn hash_intersect(a: &[u64], b: &[u64]) -> Vec<u64> {
        let sb: HashSet<u64> = b.iter().copied().collect();
        let mut out: Vec<u64> = a.iter().copied().filter(|x| sb.contains(x)).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn hash_union(a: &[u64], b: &[u64]) -> Vec<u64> {
        let mut set: HashSet<u64> = a.iter().copied().collect();
        set.extend(b.iter().copied());
        let mut out: Vec<u64> = set.into_iter().collect();
        out.sort_unstable();
        out
    }

    fn demo() -> Vec<Product> {
        vec![
            product(1, 100, 10, 1, &["自行车", "运动"]),
            product(2, 200, 20, 1, &["自行车"]),
            product(3, 300, 30, 2, &["书", "运动"]),
            product(4, 400, 40, 2, &[]),
            product(5, 500, 50, 3, &["自行车", "书"]),
        ]
    }

    #[test]
    fn build_and_get() {
        let idx = InvertedIndex::build(&demo());
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.get("自行车"), &[1, 2, 5]);
        assert_eq!(idx.get("运动"), &[1, 3]);
        assert_eq!(idx.get("书"), &[3, 5]);
        // 不存在的标签返回空切片，不 panic
        assert!(idx.get("不存在").is_empty());
        assert!(!idx.is_empty());
    }

    #[test]
    fn empty_build() {
        let idx = InvertedIndex::build(&[]);
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.search(&s(&["任意"]), QueryMode::Or).is_empty());
        assert!(idx.tag_counts().is_empty());
    }

    #[test]
    fn search_and_or() {
        let idx = InvertedIndex::build(&demo());
        // AND：同时有 自行车 与 运动
        assert_eq!(idx.search(&s(&["自行车", "运动"]), QueryMode::And), vec![1]);
        // OR：自行车 或 运动
        assert_eq!(
            idx.search(&s(&["自行车", "运动"]), QueryMode::Or),
            vec![1, 2, 3, 5]
        );
        // 单个标签
        assert_eq!(idx.search(&s(&["书"]), QueryMode::And), vec![3, 5]);
        // 有标签不存在 → AND 为空、OR 退化为另一个标签
        assert!(idx.search(&s(&["自行车", "无"]), QueryMode::And).is_empty());
        assert_eq!(
            idx.search(&s(&["自行车", "无"]), QueryMode::Or),
            vec![1, 2, 5]
        );
    }

    #[test]
    fn search_empty_query() {
        let idx = InvertedIndex::build(&demo());
        assert!(idx.search(&[], QueryMode::And).is_empty());
        assert!(idx.search(&[], QueryMode::Or).is_empty());
    }

    #[test]
    fn search_matches_hashset_random() {
        // 200 组随机标签集合：双指针结果必须与 HashSet 结果一致
        let mut rng = Rng::new(0x1234_5678);
        for _ in 0..200 {
            let mut products = Vec::new();
            for id in 1..=40u64 {
                let mut tags = Vec::new();
                for t in 0..6u64 {
                    if rng.below(3) == 0 {
                        tags.push(format!("t{t}"));
                    }
                }
                products.push(product(id, 100, 0, 1, &[]));
                products.last_mut().unwrap().tags = tags;
            }
            let idx = InvertedIndex::build(&products);

            for mode in [QueryMode::And, QueryMode::Or] {
                let query: Vec<String> = (0..6u64)
                    .filter(|_| rng.below(2) == 0)
                    .map(|t| format!("t{t}"))
                    .collect();
                if query.is_empty() {
                    continue;
                }
                let got = idx.search(&query, mode);

                // 用 HashSet 暴力算正确答案
                let sets: Vec<HashSet<u64>> = query
                    .iter()
                    .map(|q| idx.get(q).iter().copied().collect())
                    .collect();
                let expect: Vec<u64> = (1..=40u64)
                    .filter(|id| match mode {
                        QueryMode::And => sets.iter().all(|st| st.contains(id)),
                        QueryMode::Or => sets.iter().any(|st| st.contains(id)),
                    })
                    .collect();
                assert_eq!(got, expect, "mode={mode:?} query={query:?}");
            }
        }
    }

    #[test]
    fn intersect_union_sorted_correctness() {
        let mut rng = Rng::new(99);
        for _ in 0..300 {
            let mut a: Vec<u64> = (0..rng.below(20)).map(|_| rng.below(30)).collect();
            let mut b: Vec<u64> = (0..rng.below(20)).map(|_| rng.below(30)).collect();
            a.sort_unstable();
            a.dedup();
            b.sort_unstable();
            b.dedup();
            assert_eq!(intersect_sorted(&a, &b), hash_intersect(&a, &b));
            assert_eq!(union_sorted(&a, &b), hash_union(&a, &b));
        }
    }

    #[test]
    fn intersect_union_edge_cases() {
        let empty: &[u64] = &[];
        assert!(intersect_sorted(empty, &[1, 2]).is_empty());
        assert!(intersect_sorted(&[1, 2], empty).is_empty());
        assert_eq!(union_sorted(empty, &[1, 2]), vec![1, 2]);
        assert_eq!(union_sorted(&[1, 2], empty), vec![1, 2]);
        // 完全不相交
        assert!(intersect_sorted(&[1, 3, 5], &[2, 4]).is_empty());
        assert_eq!(union_sorted(&[1, 3, 5], &[2, 4]), vec![1, 2, 3, 4, 5]);
        // 完全相同
        assert_eq!(intersect_sorted(&[1, 2], &[1, 2]), vec![1, 2]);
        assert_eq!(union_sorted(&[1, 2], &[1, 2]), vec![1, 2]);
        // 包含关系
        assert_eq!(intersect_sorted(&[1, 2, 3], &[2]), vec![2]);
        assert_eq!(union_sorted(&[1, 2, 3], &[2]), vec![1, 2, 3]);
    }

    #[test]
    fn upsert_keeps_sorted_and_idempotent() {
        let mut idx = InvertedIndex::new();
        for id in [5u64, 1, 3, 9, 7] {
            let mut p = product(id, 0, 0, 1, &["x"]);
            p.tags = s(&["x"]);
            idx.upsert(&p);
        }
        assert_eq!(idx.get("x"), &[1, 3, 5, 7, 9]);
        // 重复 upsert 不产生重复 id
        let p = product(3, 0, 0, 1, &["x"]);
        idx.upsert(&p);
        idx.upsert(&p);
        assert_eq!(idx.get("x"), &[1, 3, 5, 7, 9]);
    }

    #[test]
    fn upsert_then_search() {
        let mut idx = InvertedIndex::build(&demo());
        idx.upsert(&product(6, 600, 60, 4, &["自行车", "新标签"]));
        assert_eq!(idx.get("自行车"), &[1, 2, 5, 6]);
        assert_eq!(
            idx.search(&s(&["自行车", "新标签"]), QueryMode::And),
            vec![6]
        );
        assert_eq!(idx.len(), 4);
    }

    #[test]
    fn remove_cleans_postings() {
        let mut idx = InvertedIndex::build(&demo());
        idx.remove(2, &s(&["自行车"]));
        assert_eq!(idx.get("自行车"), &[1, 5]);
        // 删到空 → 整个标签键被清理
        idx.remove(1, &s(&["运动"]));
        idx.remove(3, &s(&["运动"]));
        assert!(idx.get("运动").is_empty());
        assert_eq!(idx.len(), 2); // 只剩 自行车 / 书
        assert!(!idx.tag_counts().iter().any(|(t, _)| t == "运动"));
    }

    #[test]
    fn remove_missing_is_noop() {
        let mut idx = InvertedIndex::build(&demo());
        idx.remove(999, &s(&["自行车"]));
        idx.remove(1, &s(&["不存在"]));
        assert_eq!(idx.get("自行车"), &[1, 2, 5]);
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn remove_all_products_makes_index_empty() {
        let mut idx = InvertedIndex::build(&demo());
        for p in demo() {
            idx.remove(p.id, &p.tags);
        }
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn tag_counts_sorted_desc() {
        let idx = InvertedIndex::build(&demo());
        let counts = idx.tag_counts();
        assert_eq!(counts[0], ("自行车".to_string(), 3));
        // 同计数按字典序：书 与 运动 都是 2，但字典序「书」<「运动」
        assert_eq!(counts[1], ("书".to_string(), 2));
        assert_eq!(counts[2], ("运动".to_string(), 2));
    }

    #[test]
    fn duplicate_tags_in_product_dedup() {
        // 商品自己写了重复标签，posting 里不能出现两次
        let mut p = product(1, 0, 0, 1, &[]);
        p.tags = s(&["x", "x", "y"]);
        let idx = InvertedIndex::build(&[p.clone()]);
        assert_eq!(idx.get("x"), &[1]);
        assert_eq!(idx.get("y"), &[1]);

        let mut idx2 = InvertedIndex::new();
        idx2.upsert(&p);
        assert_eq!(idx2.get("x"), &[1]);
    }
}
