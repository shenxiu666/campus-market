//! 排序算法实测：三路快排 vs 标准库 vs 二叉堆 Top-K。
//!
//! 运行（务必 release，debug 的常数因子没有参考意义）：
//!
//! ```bash
//! cargo run -p campus-rec --release --example bench_sort
//! ```
//!
//! 输出表格会被抄进 `docs/ALGORITHMS.md` 的「实测数据」一节。

use campus_rec::index::{InvertedIndex, QueryMode};
use campus_rec::model::{Product, Status};
use campus_rec::sort::{SortKey, compare_for, sort_products, three_way_quick_sort, top_k};
use std::time::{Duration, Instant};

/// 数据规模。
const SIZES: [usize; 3] = [1_000, 10_000, 100_000];
/// Top-K 的 k。
const K: usize = 10;
/// 每个测量点重复次数，取中位数（避免 JIT/调度抖动）。
const REPEAT: usize = 7;

/// 固定种子的 xorshift，保证每次跑的数据完全一样。
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

/// 构造贴近真实的数据：价格集中在若干档位（制造大量相等键）、
/// 标签从 40 个里随机取 1~3 个、创建时间散布在一个月内。
fn gen_data(n: usize, seed: u64) -> Vec<Product> {
    let mut rng = Rng(seed);
    let tags = [
        "自行车",
        "运动",
        "健身",
        "教材",
        "计算机",
        "考研",
        "电子",
        "数码",
        "电脑",
        "家具",
        "宿舍",
        "生活",
        "乐器",
        "音乐",
        "图书",
        "文学",
        "家电",
        "厨具",
        "服饰",
        "美妆",
    ];
    (0..n)
        .map(|i| {
            let nt = 1 + rng.below(3) as usize;
            let mut t = Vec::with_capacity(nt);
            for _ in 0..nt {
                let tag = tags[rng.below(tags.len() as u64) as usize];
                if !t.iter().any(|x: &String| x == tag) {
                    t.push(tag.to_string());
                }
            }
            let price = (rng.below(200) as i64 + 1) * 1000; // 100~20000 分
            Product {
                id: i as u64 + 1,
                title: format!("商品{i}"),
                desc: String::new(),
                price_cents: price,
                tags: t,
                seller_id: rng.below(50),
                condition: 5 + rng.below(5) as u8,
                campus: "前湖校区".to_string(),
                created_at: 1_700_000_000 - rng.below(30 * 24 * 3600) as i64,
                status: Status::OnSale,
                views: rng.below(500),
                clicks: rng.below(80),
                favs: rng.below(20),
            }
        })
        .collect()
}

/// 跑 `f` 若干次取中位数耗时。
fn median_ms<F: FnMut()>(repeat: usize, mut f: F) -> f64 {
    let mut samples: Vec<Duration> = Vec::with_capacity(repeat);
    for _ in 0..repeat {
        let t = Instant::now();
        f();
        samples.push(t.elapsed());
    }
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1000.0
}

fn main() {
    println!("# 排序算法实测（release，单位 ms，取 {} 次中位数）", REPEAT);
    println!("# k = {K}\n");
    println!(
        "| n | three_way_quick | std sort_unstable_by | top_k (堆) | 快排/标准库 | top_k/全排序 |"
    );
    println!("|---|---|---|---|---|---|");

    for n in SIZES {
        let base = gen_data(n, 0xC0FFEE);

        // 1. 自研三路快排（原地，需要克隆一份以免多次排序已有序数据）
        let quick = median_ms(REPEAT, || {
            let mut v = base.clone();
            three_way_quick_sort(&mut v, &compare_for(SortKey::Hot, 1_700_000_000));
            std::hint::black_box(&v);
        });

        // 2. 标准库 sort_unstable_by（对照）
        let std_sort = median_ms(REPEAT, || {
            let mut v = base.clone();
            let cmp = compare_for(SortKey::Hot, 1_700_000_000);
            v.sort_unstable_by(cmp);
            std::hint::black_box(&v);
        });

        // 3. 二叉堆 Top-K
        let topk = median_ms(REPEAT, || {
            let out = top_k(&base, K, compare_for(SortKey::Hot, 1_700_000_000));
            std::hint::black_box(&out);
        });

        println!(
            "| {n} | {quick:.3} | {std_sort:.3} | {topk:.3} | {:.2}x | {:.1}x |",
            std_sort / quick,
            std_sort / topk
        );
    }

    println!("\n## 端到端：完整 sort_products（含 compare_for 闭包）");
    println!("\n| n | sort_products (Hot) |");
    println!("|---|---|");
    for n in SIZES {
        let base = gen_data(n, 0x1234);
        let ms = median_ms(REPEAT, || {
            let mut v = base.clone();
            sort_products(&mut v, SortKey::Hot);
            std::hint::black_box(&v);
        });
        println!("| {n} | {ms:.3} |");
    }

    // ---- 重复键场景：三路划分的主场 ----
    // 把价格压到 5 个档位，制造海量相等主键（tie-break 后仍然大量相等）。
    println!("\n## 重复键场景（价格仅 5 档，大量相等键）");
    println!("\n| n | three_way_quick | std sort_unstable_by | 快排/标准库 |");
    println!("|---|---|---|---|");
    for n in SIZES {
        let mut base = gen_data(n, 0xBEEF);
        for p in base.iter_mut() {
            p.price_cents = (p.id % 5) as i64 * 1000;
        }
        let quick = median_ms(REPEAT, || {
            let mut v = base.clone();
            three_way_quick_sort(&mut v, &compare_for(SortKey::PriceAsc, 1_700_000_000));
            std::hint::black_box(&v);
        });
        let std_sort = median_ms(REPEAT, || {
            let mut v = base.clone();
            v.sort_unstable_by(compare_for(SortKey::PriceAsc, 1_700_000_000));
            std::hint::black_box(&v);
        });
        println!(
            "| {n} | {quick:.3} | {std_sort:.3} | {:.2}x |",
            std_sort / quick
        );
    }

    // ---- 纯整数键：隔离比较器开销，只看排序算法本身 ----
    // 上面的测试里比较器要算 hot_score（含 powf），开销远大于排序本身，
    // 会掩盖算法差异。这里用纯整数比较，才是「排序算法」的真实速度。
    println!("\n## 纯整数键排序（隔离比较器开销，只比排序算法本身）");
    println!("\n| n | three_way_quick | std sort_unstable_by | 快排/标准库 |");
    println!("|---|---|---|---|");
    for n in SIZES {
        let base = gen_data(n, 0x5EED);
        let quick = median_ms(REPEAT, || {
            let mut v = base.clone();
            three_way_quick_sort(&mut v, &|a: &Product, b: &Product| {
                a.price_cents.cmp(&b.price_cents).then(a.id.cmp(&b.id))
            });
            std::hint::black_box(&v);
        });
        let std_sort = median_ms(REPEAT, || {
            let mut v = base.clone();
            v.sort_unstable_by(|a, b| a.price_cents.cmp(&b.price_cents).then(a.id.cmp(&b.id)));
            std::hint::black_box(&v);
        });
        println!(
            "| {n} | {quick:.3} | {std_sort:.3} | {:.2}x |",
            std_sort / quick
        );
    }

    // ---- 倒排索引 vs 线性扫描 ----
    println!("\n## 倒排索引 vs 线性扫描（单标签查询，命中约 5%）");
    println!("\n| n | 倒排索引 | 线性扫描 | 加速比 |");
    println!("|---|---|---|---|");
    for n in SIZES {
        let base = gen_data(n, 0xF00D);
        let idx = InvertedIndex::build(&base);
        // 选一个高频标签做查询，模拟「搜索页」的典型调用。
        let tag = idx
            .tag_counts()
            .first()
            .map(|(t, _)| t.clone())
            .unwrap_or_default();
        let query = vec![tag.clone()];

        let inv = median_ms(REPEAT, || {
            let out = idx.search(&query, QueryMode::Or);
            std::hint::black_box(&out);
        });
        let scan = median_ms(REPEAT, || {
            let out: Vec<u64> = base
                .iter()
                .filter(|p| p.tags.iter().any(|t| t == &tag))
                .map(|p| p.id)
                .collect();
            std::hint::black_box(&out);
        });
        println!("| {n} | {inv:.4} | {scan:.4} | {:.1}x |", scan / inv);
    }
}
