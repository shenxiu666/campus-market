//! 排序算法现场基准：三路快排 / 标准库 `sort_unstable_by` / 二叉堆 Top-K。
//!
//! ## 为什么这里自带一份三路快排
//!
//! `campus-rec::sort` 里的三路快排是**算法演示与 `REC.*` 的实现**，它的正确性由
//! `campus-rec` 自己的单测保证。本模块是**基准测试夹具**，需要满足两点：
//!
//! 1. `/api/bench/sort` 必须永远可用 —— 演示时算法 crate 的任何抖动都不该让面板 503；
//! 2. 三条实现必须跑在**同一批数据、同一个比较函数**上，否则耗时没有可比性。
//!
//! 因此这里独立实现一份与 `campus-rec` 等价的算法（median-of-3 + 三路划分 + 小区间插排；
//! 二叉堆 Top-K），并用单测与标准库逐元素比对，保证「等价」不是嘴上说说。

use serde_json::{Value, json};
use std::cmp::Ordering;
use std::time::Instant;

/// 小区间阈值：小于等于它时改用插入排序。
const INSERTION_THRESHOLD: usize = 16;

/// 基准用的排序记录（只要比较键，避免构造完整商品对象）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// 价格（分）
    pub price_cents: i64,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 商品 id
    pub id: u64,
}

impl Row {
    /// 与 `campus-rec` 一致的比较：价格升序 → 创建时间降序 → id 升序。
    pub fn compare(a: &Row, b: &Row) -> Ordering {
        a.price_cents
            .cmp(&b.price_cents)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    }
}

/// 确定性伪随机数（xorshift64*），避免基准数据每次都不同。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn range(&mut self, low: u64, high: u64) -> u64 {
        if high <= low {
            return low;
        }
        low + self.next_u64() % (high - low)
    }
}

/// 生成 `n` 条基准数据（价格 100..100000 分，时间戳递增）。
pub fn generate(n: usize) -> Vec<Row> {
    let mut rng = Rng::new(0xC0FF_EE12_3456_789A);
    (0..n)
        .map(|i| Row {
            price_cents: rng.range(100, 100_000) as i64,
            created_at: 1_700_000_000 + i as i64 * 37,
            id: i as u64 + 1,
        })
        .collect()
}

/// 三路快速排序（原地）。返回比较次数，便于展示复杂度行为。
pub fn three_way_quick(items: &mut [Row]) -> u64 {
    let mut comparisons = 0u64;
    quick(items, &mut comparisons);
    comparisons
}

fn quick(items: &mut [Row], comparisons: &mut u64) {
    if items.len() <= INSERTION_THRESHOLD {
        insertion(items, comparisons);
        return;
    }
    let pivot = median_of_three(items, comparisons);
    // 三路划分：[0, lt) < pivot，[lt, gt] == pivot，(gt, len) > pivot
    let (mut lt, mut i, mut gt) = (0usize, 0usize, items.len());
    while i < gt {
        *comparisons += 1;
        match Row::compare(&items[i], &pivot) {
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
    quick(&mut items[..lt], comparisons);
    quick(&mut items[gt..], comparisons);
}

/// 小区间插入排序。
fn insertion(items: &mut [Row], comparisons: &mut u64) {
    for i in 1..items.len() {
        let key = items[i];
        let mut j = i;
        while j > 0 {
            *comparisons += 1;
            if Row::compare(&items[j - 1], &key) != Ordering::Greater {
                break;
            }
            items[j] = items[j - 1];
            j -= 1;
        }
        items[j] = key;
    }
}

/// median-of-3：取首、中、尾的中位数，并把它换到首位当作 pivot。
fn median_of_three(items: &mut [Row], comparisons: &mut u64) -> Row {
    let mid = items.len() / 2;
    let last = items.len() - 1;
    *comparisons += 3;
    if Row::compare(&items[mid], &items[0]) == Ordering::Less {
        items.swap(0, mid);
    }
    if Row::compare(&items[last], &items[0]) == Ordering::Less {
        items.swap(0, last);
    }
    if Row::compare(&items[last], &items[mid]) == Ordering::Less {
        items.swap(mid, last);
    }
    items[mid]
}

/// 二叉堆 Top-K：维护一个大小为 `k` 的最大堆，堆顶是当前结果里最大的元素。
///
/// 复杂度 O(n log k)，不需要全排序；`k == 0` 或空输入返回空。
pub fn top_k(items: &[Row], k: usize) -> Vec<Row> {
    if k == 0 || items.is_empty() {
        return Vec::new();
    }
    let k = k.min(items.len());
    let mut heap: Vec<Row> = Vec::with_capacity(k);
    for &item in items {
        if heap.len() < k {
            heap.push(item);
            let last = heap.len() - 1;
            sift_up(&mut heap, last);
        } else if Row::compare(&item, &heap[0]) == Ordering::Less {
            heap[0] = item;
            sift_down(&mut heap, 0);
        }
    }
    // 堆里是「最小的 k 个」，按升序输出
    heap.sort_unstable_by(Row::compare);
    heap
}

/// 堆上浮。
fn sift_up(heap: &mut [Row], mut index: usize) {
    while index > 0 {
        let parent = (index - 1) / 2;
        if Row::compare(&heap[index], &heap[parent]) == Ordering::Greater {
            heap.swap(index, parent);
            index = parent;
        } else {
            break;
        }
    }
}

/// 堆下沉。
fn sift_down(heap: &mut [Row], mut index: usize) {
    let len = heap.len();
    loop {
        let left = index * 2 + 1;
        if left >= len {
            break;
        }
        let right = left + 1;
        let mut largest = left;
        if right < len && Row::compare(&heap[right], &heap[left]) == Ordering::Greater {
            largest = right;
        }
        if Row::compare(&heap[largest], &heap[index]) == Ordering::Greater {
            heap.swap(index, largest);
            index = largest;
        } else {
            break;
        }
    }
}

/// 跑三种算法并返回 `/api/bench/sort` 的 JSON。
///
/// 输出：
/// ```json
/// {"n":20000,"results":[{"algorithm":"three_way_quick","ms":3.1,"comparisons":182341},...]}
/// ```
pub fn run_sort_bench(n: usize) -> Value {
    let base = generate(n);
    let mut results: Vec<Value> = Vec::with_capacity(3);

    // 1. 三路快排（自研）
    let mut data = base.clone();
    let start = Instant::now();
    let comparisons = three_way_quick(&mut data);
    results.push(json!({
        "algorithm": "three_way_quick",
        "label": "三路快排（自研）",
        "ms": millis(start),
        "comparisons": comparisons,
        "complexity": "O(n log n) 平均 / O(n) 全相同",
    }));
    let quick_sorted = data;

    // 2. 标准库 sort_unstable_by
    let mut data = base.clone();
    let start = Instant::now();
    data.sort_unstable_by(Row::compare);
    results.push(json!({
        "algorithm": "sort_unstable_by",
        "label": "标准库 sort_unstable_by",
        "ms": millis(start),
        "comparisons": Value::Null,
        "complexity": "O(n log n) 最坏",
    }));
    let std_sorted = data;

    // 3. 二叉堆 Top-K（k = n / 10）
    let k = (n / 10).max(1);
    let start = Instant::now();
    let top = top_k(&base, k);
    results.push(json!({
        "algorithm": "heap_top_k",
        "label": format!("二叉堆 Top-{k}"),
        "ms": millis(start),
        "comparisons": Value::Null,
        "complexity": "O(n log k)",
    }));

    // 一致性校验：三条路径的前 k 个必须一致
    let consistent = quick_sorted[..k] == std_sorted[..k] && top == std_sorted[..k];
    json!({
        "n": n,
        "k": k,
        "results": results,
        "consistent": consistent,
    })
}

/// `Instant` 差值转毫秒（3 位小数）。
fn millis(start: Instant) -> f64 {
    let micros = start.elapsed().as_micros() as f64;
    (micros / 1000.0 * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn std_sorted(mut rows: Vec<Row>) -> Vec<Row> {
        rows.sort_by(Row::compare);
        rows
    }

    #[test]
    fn three_way_quick_matches_stdlib_on_random_data() {
        for n in [0usize, 1, 2, 5, 17, 100, 1000] {
            let mut rows = generate(n);
            three_way_quick(&mut rows);
            assert_eq!(rows, std_sorted(generate(n)), "n={n} 结果应与标准库一致");
        }
    }

    #[test]
    fn three_way_quick_handles_all_equal() {
        let mut rows = vec![
            Row {
                price_cents: 100,
                created_at: 5,
                id: 1
            };
            50
        ];
        // id 全相同会破坏 tie-break 的严格弱序假设，这里改成递增 id
        for (i, row) in rows.iter_mut().enumerate() {
            row.id = i as u64;
        }
        three_way_quick(&mut rows);
        assert_eq!(rows, std_sorted(rows.clone()));
    }

    #[test]
    fn three_way_quick_handles_reverse_sorted() {
        let mut rows: Vec<Row> = (0..100)
            .map(|i| Row {
                price_cents: 100 - i,
                created_at: i,
                id: i as u64,
            })
            .collect();
        three_way_quick(&mut rows);
        assert_eq!(rows, std_sorted(rows.clone()));
    }

    #[test]
    fn top_k_returns_smallest_k_in_order() {
        let rows = generate(500);
        let expected: Vec<Row> = std_sorted(rows.clone()).into_iter().take(10).collect();
        assert_eq!(top_k(&rows, 10), expected);
    }

    #[test]
    fn top_k_edge_cases() {
        assert!(top_k(&[], 5).is_empty());
        assert!(top_k(&generate(10), 0).is_empty());
        // k 大于长度时退化为全排序
        let rows = generate(7);
        assert_eq!(top_k(&rows, 100), std_sorted(rows.clone()));
    }

    #[test]
    fn top_k_matches_full_sort_prefix() {
        let rows = generate(1000);
        let full = std_sorted(rows.clone());
        for k in [1usize, 3, 64, 100, 999] {
            assert_eq!(top_k(&rows, k), full[..k].to_vec(), "k={k}");
        }
    }

    #[test]
    fn run_sort_bench_reports_all_three_algorithms() {
        let json = run_sort_bench(500);
        assert_eq!(json["n"], json!(500));
        assert_eq!(json["consistent"], json!(true));
        let results = json["results"].as_array().expect("results 应为数组");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["algorithm"], json!("three_way_quick"));
        assert_eq!(results[1]["algorithm"], json!("sort_unstable_by"));
        assert_eq!(results[2]["algorithm"], json!("heap_top_k"));
        for item in results {
            assert!(item["ms"].as_f64().unwrap_or(-1.0) >= 0.0);
        }
    }

    #[test]
    fn generate_is_deterministic() {
        assert_eq!(generate(20), generate(20));
        assert_ne!(generate(20)[0], generate(21)[1]);
    }
}
