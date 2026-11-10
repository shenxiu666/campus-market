//! 排序算法：三路快速排序与二叉堆 Top-K。
//!
//! 课程选题要求「自研排序」，因此本模块**不调用** `slice::sort_by` /
//! `slice::sort_unstable_by` 完成商品排序（仅在单测里用标准库结果做对照）。
//! 实现细节、复杂度推导与实测耗时见 `docs/ALGORITHMS.md`。

use crate::hot::hot_score;
use crate::model::Product;
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

/// 小区间阈值：长度不超过该值的区间改用插入排序。
///
/// 三路快排划分到 16 个元素左右时，函数调用与划分本身的开销已经超过比较开销，
/// 而插入排序在小数组上常数因子极小、局部性极好。
pub const INSERTION_THRESHOLD: usize = 16;

/// 排序键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// 价格升序
    PriceAsc,
    /// 价格降序
    PriceDesc,
    /// 最新发布在前
    Newest,
    /// 热度分从高到低
    Hot,
}

impl SortKey {
    /// 解析 Web/CLI 传入的排序名（大小写不敏感），无法识别返回 `None`。
    pub fn parse(s: &str) -> Option<SortKey> {
        match s.trim().to_ascii_lowercase().as_str() {
            "price_asc" | "price" | "priceasc" => Some(SortKey::PriceAsc),
            "price_desc" | "pricedesc" => Some(SortKey::PriceDesc),
            "newest" | "time" | "latest" => Some(SortKey::Newest),
            "hot" | "trending" => Some(SortKey::Hot),
            _ => None,
        }
    }

    /// 排序名的规范写法，用于回显给前端。
    pub fn as_str(&self) -> &'static str {
        match self {
            SortKey::PriceAsc => "price_asc",
            SortKey::PriceDesc => "price_desc",
            SortKey::Newest => "newest",
            SortKey::Hot => "hot",
        }
    }
}

/// 当前 Unix 秒。
///
/// `sort_products` 的签名里没有 `now` 参数，`SortKey::Hot` 只能取系统当前时间；
/// 需要固定时间基准（例如单测、回放）时请改用 [`compare_for`] + [`top_k`]。
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 按 `key` 原地排序。同键时按「创建时间降序 → id 升序」做确定性 tie-break。
pub fn sort_products(items: &mut [Product], key: SortKey) {
    let cmp = compare_for(key, unix_now());
    three_way_quick_sort(items, &cmp);
}

/// 三路快速排序（median-of-3 选枢轴 + 三路划分 + 小区间插入排序 + 尾递归优化）。
///
/// * **median-of-3**：取首、中、尾三个元素的中位数当枢轴，避免「已排序 / 逆序」
///   这类常见输入退化成 O(n²)（取首元素当枢轴时的经典退化）。
/// * **三路划分**（Dutch National Flag）：一次扫描把区间切成 `<`、`==`、`>` 三段。
///   重复键很多时（例如「按状态排序」这种低基数键）中间段直接跳过，
///   复杂度从 `O(n log n)` 退化为接近 `O(n)`。
/// * **插入排序**：长度 ≤ [`INSERTION_THRESHOLD`] 的区间不再递归。
/// * **尾递归优化**：只把「较小的一侧」压栈，较大的一侧用循环继续处理，
///   于是栈深 ≤ log2(n)，即使遇到极端划分也不会爆栈。
///
/// 时间复杂度：平均 / 最好 `O(n log n)`，最坏 `O(n²)`（概率极低，且 median-of-3
/// 已规避有序输入退化）；空间 `O(log n)`（显式栈）。
pub fn three_way_quick_sort<T, F>(items: &mut [T], cmp: &F)
where
    F: Fn(&T, &T) -> Ordering,
{
    if items.len() < 2 {
        return;
    }

    // 显式栈模拟递归，存放待处理区间的左闭右开下标。
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut lo = 0usize;
    let mut hi = items.len();

    loop {
        if hi > lo + 1 {
            if hi - lo <= INSERTION_THRESHOLD {
                insertion_sort(&mut items[lo..hi], cmp);
            } else {
                let (eq_lo, eq_hi) = partition_three_way(&mut items[lo..hi], cmp);
                let eq_lo = lo + eq_lo;
                let eq_hi = lo + eq_hi;
                // 划分后：[lo, eq_lo) < 枢轴 ; [eq_lo, eq_hi) == 枢轴 ; [eq_hi, hi) > 枢轴
                if eq_lo - lo < hi - eq_hi {
                    // 左段更小：左段压栈，循环处理右段
                    if eq_lo > lo {
                        stack.push((lo, eq_lo));
                    }
                    lo = eq_hi;
                } else {
                    // 右段更小：右段压栈，循环处理左段
                    if eq_hi < hi {
                        stack.push((eq_hi, hi));
                    }
                    hi = eq_lo;
                }
                continue;
            }
        }
        match stack.pop() {
            Some((next_lo, next_hi)) => {
                lo = next_lo;
                hi = next_hi;
            }
            None => break,
        }
    }
}

/// 插入排序：稳定，适合小数组。
fn insertion_sort<T, F>(items: &mut [T], cmp: &F)
where
    F: Fn(&T, &T) -> Ordering,
{
    for i in 1..items.len() {
        let mut j = i;
        // 相邻交换实现「后移」，保持相等元素的相对顺序（稳定）
        while j > 0 && cmp(&items[j], &items[j - 1]) == Ordering::Less {
            items.swap(j, j - 1);
            j -= 1;
        }
    }
}

/// 三路划分，返回「等于枢轴」区间的左闭右开下标（相对 `items`）。
fn partition_three_way<T, F>(items: &mut [T], cmp: &F) -> (usize, usize)
where
    F: Fn(&T, &T) -> Ordering,
{
    let n = items.len();
    let mid = n / 2;

    // median-of-3：把首/中/尾排序，中位数落在 mid
    if cmp(&items[mid], &items[0]) == Ordering::Less {
        items.swap(0, mid);
    }
    if cmp(&items[n - 1], &items[mid]) == Ordering::Less {
        items.swap(mid, n - 1);
    }
    if cmp(&items[mid], &items[0]) == Ordering::Less {
        items.swap(0, mid);
    }
    // 枢轴搬到 0 号位，这样划分时只要和 items[0] 比较
    items.swap(0, mid);

    let mut lt = 1; // [1, lt) 是「小于枢轴」
    let mut i = 1; // 扫描指针
    let mut gt = n; // [gt, n) 是「大于枢轴」
    while i < gt {
        match cmp(&items[i], &items[0]) {
            Ordering::Less => {
                items.swap(lt, i);
                lt += 1;
                i += 1;
            }
            Ordering::Greater => {
                gt -= 1;
                items.swap(i, gt);
            }
            Ordering::Equal => i += 1,
        }
    }
    // 枢轴归位到「等于段」的左端
    items.swap(0, lt - 1);
    (lt - 1, gt)
}

/// 返回前 `k` 个元素（按 `cmp` 升序语义，即 `cmp(a,b) == Less` 的元素排前面）。
///
/// 用**大小为 k 的二叉堆**实现，时间复杂度 `O(n log k)`、额外空间 `O(k)`；
/// 相比之下「全排序再截断」是 `O(n log n)`。k 远小于 n 时（推荐场景常见：
/// n=10⁵、k=10）堆方案快一个数量级。
///
/// 堆的语义：堆顶是当前保留集合里**最差**（`cmp` 最大）的那个元素，
/// 新元素只要比堆顶好就替换堆顶并下沉。结果再按 `cmp` 升序输出。
pub fn top_k<F>(items: &[Product], k: usize, cmp: F) -> Vec<&Product>
where
    F: Fn(&Product, &Product) -> Ordering,
{
    top_k_by(items, k, cmp)
}

/// [`top_k`] 的泛型版本：任意元素类型 + 任意比较函数。
///
/// 抽出来是为了让「按分数取前 k 个 [`crate::recommender::Scored`]」也能复用同一套堆，
/// 不必为每种类型复制一遍堆代码。
pub fn top_k_by<T, F>(items: &[T], k: usize, cmp: F) -> Vec<&T>
where
    F: Fn(&T, &T) -> Ordering,
{
    if k == 0 || items.is_empty() {
        return Vec::new();
    }
    let k = k.min(items.len());

    // 堆里存下标而不是引用：这样 `cmp` 只需要不可变借用 `items`，不会和堆的可变借用打架。
    let mut heap: Vec<usize> = Vec::with_capacity(k);
    let by_index = |x: usize, y: usize| cmp(&items[x], &items[y]);

    for i in 0..items.len() {
        if heap.len() < k {
            heap.push(i);
            if heap.len() == k {
                // 一次性建堆（Floyd 建堆）：O(k) 而不是 k 次 O(log k) 插入
                for j in (0..k / 2).rev() {
                    sift_down(&mut heap, j, &by_index);
                }
            }
        } else if by_index(i, heap[0]) == Ordering::Less {
            // 比堆顶（保留集合中最差的元素）更好 → 替换并下沉
            heap[0] = i;
            sift_down(&mut heap, 0, &by_index);
        }
    }

    // 堆排序取出升序结果：反复把堆顶（当前最大）换到末尾
    let mut end = heap.len();
    while end > 1 {
        end -= 1;
        heap.swap(0, end);
        sift_down(&mut heap[..end], 0, &by_index);
    }
    heap.into_iter().map(|i| &items[i]).collect()
}

/// 大顶堆下沉（「大」由 `cmp` 决定，即堆顶是 `cmp` 最大的元素）。
fn sift_down<G>(heap: &mut [usize], mut i: usize, cmp: &G)
where
    G: Fn(usize, usize) -> Ordering,
{
    let n = heap.len();
    loop {
        let left = 2 * i + 1;
        let right = left + 1;
        let mut largest = i;
        if left < n && cmp(heap[left], heap[largest]) == Ordering::Greater {
            largest = left;
        }
        if right < n && cmp(heap[right], heap[largest]) == Ordering::Greater {
            largest = right;
        }
        if largest == i {
            break;
        }
        heap.swap(i, largest);
        i = largest;
    }
}

/// 该排序键对应的比较函数（供 `top_k` 与 Web 层复用）。
///
/// 三级 tie-break：**主键 → `created_at` 降序 → `id` 升序**。
/// 这样比较函数构成全序（id 唯一），排序结果与输入顺序无关、完全确定，
/// 前端多次刷新不会看到列表抖动，单测也能逐元素断言。
pub fn compare_for(key: SortKey, now: i64) -> impl Fn(&Product, &Product) -> Ordering {
    move |a: &Product, b: &Product| {
        let primary = match key {
            SortKey::PriceAsc => a.price_cents.cmp(&b.price_cents),
            SortKey::PriceDesc => b.price_cents.cmp(&a.price_cents),
            SortKey::Newest => b.created_at.cmp(&a.created_at),
            // 热度是浮点，用 total_cmp 保证全序（NaN 也不会破坏排序）
            SortKey::Hot => hot_score(b, now).total_cmp(&hot_score(a, now)),
        };
        primary
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Rng, product};

    const NOW: i64 = 1_700_000_000;

    fn ids(items: &[Product]) -> Vec<u64> {
        items.iter().map(|p| p.id).collect()
    }

    /// 用标准库排一遍，作为「正确答案」。
    fn std_sorted(mut v: Vec<Product>, key: SortKey) -> Vec<Product> {
        let cmp = compare_for(key, NOW);
        v.sort_by(cmp);
        v
    }

    fn sample(n: usize, rng: &mut Rng) -> Vec<Product> {
        (0..n)
            .map(|i| {
                let price = rng.below(5) as i64 * 1000; // 少量重复价格，制造大量相等键
                let created = 1_600_000_000 + rng.below(1000) as i64;
                product(i as u64 + 1, price, created, 1, &["自行车"])
            })
            .collect()
    }

    #[test]
    fn empty_slice() {
        let mut v: Vec<Product> = Vec::new();
        three_way_quick_sort(&mut v, &|a: &Product, b: &Product| a.id.cmp(&b.id));
        assert!(v.is_empty());
        assert_eq!(sort_products(&mut v, SortKey::PriceAsc), ());
        assert!(top_k(&v, 3, compare_for(SortKey::PriceAsc, NOW)).is_empty());
    }

    #[test]
    fn single_element() {
        let mut v = vec![product(7, 100, 10, 1, &[])];
        sort_products(&mut v, SortKey::PriceDesc);
        assert_eq!(ids(&v), vec![7]);
        assert_eq!(top_k(&v, 5, compare_for(SortKey::Hot, NOW)).len(), 1);
    }

    #[test]
    fn two_elements() {
        let mut v = vec![product(1, 500, 10, 1, &[]), product(2, 100, 10, 1, &[])];
        sort_products(&mut v, SortKey::PriceAsc);
        assert_eq!(ids(&v), vec![2, 1]);

        let mut v = vec![product(1, 500, 10, 1, &[]), product(2, 100, 10, 1, &[])];
        sort_products(&mut v, SortKey::PriceDesc);
        assert_eq!(ids(&v), vec![1, 2]);
    }

    #[test]
    fn all_equal_keys() {
        // 三路划分的关键场景：全部键相等 → 中间段吞掉整个区间
        let mut v: Vec<Product> = (1..=200).map(|i| product(i, 100, 50, 1, &[])).collect();
        let expect = std_sorted(v.clone(), SortKey::PriceAsc);
        three_way_quick_sort(&mut v, &compare_for(SortKey::PriceAsc, NOW));
        assert_eq!(ids(&v), ids(&expect));
    }

    #[test]
    fn already_sorted() {
        let mut v: Vec<Product> = (1..=100)
            .map(|i| product(i, i as i64, 50, 1, &[]))
            .collect();
        let expect = std_sorted(v.clone(), SortKey::PriceAsc);
        three_way_quick_sort(&mut v, &compare_for(SortKey::PriceAsc, NOW));
        assert_eq!(ids(&v), ids(&expect));
    }

    #[test]
    fn reverse_sorted() {
        let mut v: Vec<Product> = (1..=100)
            .map(|i| product(i, (100 - i) as i64, 50, 1, &[]))
            .collect();
        let expect = std_sorted(v.clone(), SortKey::PriceAsc);
        three_way_quick_sort(&mut v, &compare_for(SortKey::PriceAsc, NOW));
        assert_eq!(ids(&v), ids(&expect));
    }

    #[test]
    fn random_1000_groups_match_std_sort() {
        // 1000 组随机数据 × 4 种排序键，与 slice::sort_by 逐元素比对
        let mut rng = Rng::new(0x5EED_1234);
        for round in 0..1000 {
            let n = rng.below(65) as usize;
            let base = sample(n, &mut rng);
            for key in [
                SortKey::PriceAsc,
                SortKey::PriceDesc,
                SortKey::Newest,
                SortKey::Hot,
            ] {
                let mut mine = base.clone();
                let expect = std_sorted(base.clone(), key);
                three_way_quick_sort(&mut mine, &compare_for(key, NOW));
                assert_eq!(
                    ids(&mine),
                    ids(&expect),
                    "round={round} key={key:?} n={n} 与标准库结果不一致"
                );
            }
        }
    }

    #[test]
    fn random_generic_matches_std() {
        // 泛型版本也要正确：直接排 u64
        let mut rng = Rng::new(7);
        for _ in 0..200 {
            let n = rng.below(80) as usize;
            let mut mine: Vec<u64> = (0..n).map(|_| rng.below(30)).collect();
            let mut expect = mine.clone();
            three_way_quick_sort(&mut mine, &|a: &u64, b: &u64| a.cmp(b));
            expect.sort();
            assert_eq!(mine, expect);
        }
    }

    #[test]
    fn insertion_threshold_boundary() {
        for n in [INSERTION_THRESHOLD, INSERTION_THRESHOLD + 1] {
            let mut rng = Rng::new(n as u64);
            let mut v = sample(n, &mut rng);
            let expect = std_sorted(v.clone(), SortKey::PriceAsc);
            three_way_quick_sort(&mut v, &compare_for(SortKey::PriceAsc, NOW));
            assert_eq!(ids(&v), ids(&expect), "n={n}");
        }
    }

    #[test]
    fn tie_break_created_at_desc_then_id_asc() {
        // 同价：created_at 大的在前；created_at 也相同则 id 小的在前
        let mut v = vec![
            product(9, 100, 10, 1, &[]),
            product(3, 100, 20, 1, &[]),
            product(5, 100, 20, 1, &[]),
            product(1, 100, 5, 1, &[]),
        ];
        sort_products(&mut v, SortKey::PriceAsc);
        assert_eq!(ids(&v), vec![3, 5, 9, 1]);
    }

    #[test]
    fn compare_for_newest_and_price() {
        let a = product(1, 500, 100, 1, &[]);
        let b = product(2, 300, 200, 1, &[]);

        let price_asc = compare_for(SortKey::PriceAsc, NOW);
        assert_eq!(price_asc(&b, &a), Ordering::Less);
        let price_desc = compare_for(SortKey::PriceDesc, NOW);
        assert_eq!(price_desc(&a, &b), Ordering::Less);
        let newest = compare_for(SortKey::Newest, NOW);
        assert_eq!(newest(&a, &b), Ordering::Greater);
    }

    #[test]
    fn compare_for_hot_uses_gravity() {
        // 同样 10 次浏览：新商品热度高于老商品
        let fresh = product(1, 100, NOW - 3600, 1, &[]);
        let old = product(2, 100, NOW - 3600 * 24 * 30, 1, &[]);
        let mut fresh = fresh;
        let mut old = old;
        fresh.views = 10;
        old.views = 10;
        let hot = compare_for(SortKey::Hot, NOW);
        assert_eq!(hot(&fresh, &old), Ordering::Less);
        // 完全相同时退化为 id 升序，保证全序
        let same_a = product(4, 100, NOW - 100, 1, &[]);
        let same_b = product(5, 100, NOW - 100, 1, &[]);
        assert_eq!(hot(&same_a, &same_b), Ordering::Less);
    }

    #[test]
    fn sort_products_hot_does_not_panic() {
        let mut v: Vec<Product> = (1..=50)
            .map(|i| {
                let mut p = product(i, i as i64 * 100, 1_700_000_000 - i as i64 * 60, 1, &[]);
                p.views = i;
                p
            })
            .collect();
        sort_products(&mut v, SortKey::Hot);
        assert_eq!(v.len(), 50);
    }

    #[test]
    fn top_k_zero() {
        let v = sample(20, &mut Rng::new(1));
        assert!(top_k(&v, 0, compare_for(SortKey::PriceAsc, NOW)).is_empty());
    }

    #[test]
    fn top_k_greater_than_len() {
        let v = sample(5, &mut Rng::new(2));
        let got = top_k(&v, 99, compare_for(SortKey::PriceAsc, NOW));
        assert_eq!(got.len(), 5);
        let expect = std_sorted(v.clone(), SortKey::PriceAsc);
        assert_eq!(got.iter().map(|p| p.id).collect::<Vec<_>>(), ids(&expect));
    }

    #[test]
    fn top_k_equals_len() {
        let v = sample(8, &mut Rng::new(3));
        let got = top_k(&v, 8, compare_for(SortKey::PriceDesc, NOW));
        let expect = std_sorted(v.clone(), SortKey::PriceDesc);
        assert_eq!(got.iter().map(|p| p.id).collect::<Vec<_>>(), ids(&expect));
    }

    #[test]
    fn top_k_matches_full_sort_prefix() {
        // 随机 300 组：top_k 必须等于「全排序后取前 k」
        let mut rng = Rng::new(0xABCD);
        for round in 0..300 {
            let n = rng.below(120) as usize;
            let v = sample(n, &mut rng);
            let k = rng.below(n as u64 + 3) as usize;
            let got: Vec<u64> = top_k(&v, k, compare_for(SortKey::Hot, NOW))
                .iter()
                .map(|p| p.id)
                .collect();
            let expect: Vec<u64> = std_sorted(v.clone(), SortKey::Hot)
                .iter()
                .take(k)
                .map(|p| p.id)
                .collect();
            assert_eq!(got, expect, "round={round} n={n} k={k}");
        }
    }

    #[test]
    fn top_k_is_deterministic() {
        let v = sample(200, &mut Rng::new(11));
        let a: Vec<u64> = top_k(&v, 10, compare_for(SortKey::PriceAsc, NOW))
            .iter()
            .map(|p| p.id)
            .collect();
        let b: Vec<u64> = top_k(&v, 10, compare_for(SortKey::PriceAsc, NOW))
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn top_k_does_not_mutate_input() {
        let v = sample(30, &mut Rng::new(12));
        let before = ids(&v);
        let _ = top_k(&v, 5, compare_for(SortKey::PriceAsc, NOW));
        assert_eq!(ids(&v), before);
    }

    #[test]
    fn sort_key_parse_roundtrip() {
        for key in [
            SortKey::PriceAsc,
            SortKey::PriceDesc,
            SortKey::Newest,
            SortKey::Hot,
        ] {
            assert_eq!(SortKey::parse(key.as_str()), Some(key));
        }
        assert_eq!(SortKey::parse(" PRICE_ASC "), Some(SortKey::PriceAsc));
        assert_eq!(SortKey::parse("unknown"), None);
    }
}
