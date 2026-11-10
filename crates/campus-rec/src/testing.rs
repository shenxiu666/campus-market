//! 测试辅助：确定性随机数发生器与商品构造函数。
//!
//! 只在本 crate 的 `#[cfg(test)]` 下编译。用自带的 xorshift 而不是 `rand`，
//! 是为了让「1000 组随机数据与标准库比对」这类测试**完全可复现**：
//! 同一个种子永远生成同一串数据，失败时可以直接重跑定位。

use crate::model::{Product, Status};

/// 极简 xorshift64* 伪随机数发生器。///
/// 周期 2⁶⁴-1，统计质量足够单测使用，且无外部依赖、可复现。
pub struct Rng {
    state: u64,
}

impl Rng {
    /// 用种子构造；种子为 0 时替换成固定非零值（xorshift 要求状态非零）。
    pub fn new(seed: u64) -> Self {
        Rng {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// 下一个 64 位随机数。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `[0, n)` 内的均匀随机数；`n == 0` 时返回 0。
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

/// 构造一个测试商品。`tags` 会自动转成 `Vec<String>`。
pub fn product(
    id: u64,
    price_cents: i64,
    created_at: i64,
    seller_id: u64,
    tags: &[&str],
) -> Product {
    Product {
        id,
        title: format!("商品{id}"),
        desc: String::new(),
        price_cents,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        seller_id,
        condition: 8,
        campus: "前湖校区".to_string(),
        created_at,
        status: Status::OnSale,
        views: 0,
        clicks: 0,
        favs: 0,
    }
}
