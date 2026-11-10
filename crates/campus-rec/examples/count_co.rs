//! 临时统计脚本：用真实种子数据报告共现矩阵规模（供 docs/ALGORITHMS.md 引用）。
//!
//! ```bash
//! cargo run -p campus-rec --release --example count_co
//! ```

use campus_rec::{CoOccurrence, Event, InvertedIndex, Product};

fn main() {
    let raw = std::fs::read_to_string("data/seed.json").expect("读 seed.json 失败");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("解析 JSON 失败");

    let products: Vec<Product> = serde_json::from_value(v["products"].clone()).unwrap();
    let events: Vec<Event> = serde_json::from_value(v["events"].clone()).unwrap();

    let idx = InvertedIndex::build(&products);
    let co = CoOccurrence::build(&events);

    println!("商品数           = {}", products.len());
    println!("事件数           = {}", events.len());
    println!("标签数           = {}", idx.len());
    println!("共现对数         = {}", co.len());
    println!("最大共现次数     = {}", co.max_count());
    println!(
        "稠密矩阵格子数   = {} (N×N)",
        products.len() * products.len()
    );
    println!(
        "理论最大对子数   = {} (N(N-1)/2)",
        products.len() * (products.len() - 1) / 2
    );

    // 分布：多少对是 1 次、2 次、3 次以上
    let mut hist = std::collections::BTreeMap::<u32, usize>::new();
    for a in 0..products.len() as u64 {
        for b in (a + 1)..products.len() as u64 {
            let c = co.get(a + 1, b + 1);
            if c > 0 {
                *hist.entry(c).or_insert(0) += 1;
            }
        }
    }
    println!("共现次数分布     = {hist:?}");
}
