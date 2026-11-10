//! # campus-rec —— 校园二手交易推荐算法库
//!
//! 本 crate 不依赖任何网络/存储代码，是纯算法层，包含题目要求的三类算法：
//!
//! * **排序**：三路快速排序（`sort`）、二叉堆 Top-K（`sort::top_k`）、热度打分（`hot`）
//! * **哈希**：标签倒排索引（`index`）、行为共现矩阵（`cf`）
//! * **推荐**：Jaccard 相似度（`similarity`）+ item-based 协同过滤 + 混合打分与重排（`recommender`）
//!
//! 上层（`mredis-core` 的 `REC.*` 命令、`mredis-web` 的 REST 接口）只依赖本 crate 的公开 API。

pub mod cf;
pub mod hot;
pub mod index;
pub mod model;
pub mod recommender;
pub mod similarity;
pub mod sort;

#[cfg(test)]
mod testing;

pub use cf::CoOccurrence;
pub use hot::{hot_score, normalize_scores};
pub use index::{InvertedIndex, QueryMode};
pub use model::{Event, EventKind, Product, Status, User};
pub use recommender::{Reason, Recommender, Scored};
pub use similarity::{cosine, jaccard};
pub use sort::{SortKey, sort_products, top_k};

/// 混合打分权重：标签相似度 / 协同过滤 / 热度 / 价格匹配度 / 同校区。
///
/// 五项之和为 1.0，便于在文档中直接解释每一项的贡献。
pub const W_TAG: f64 = 0.45;
pub const W_CF: f64 = 0.25;
pub const W_HOT: f64 = 0.10;
pub const W_PRICE: f64 = 0.05;
/// 同校区贴合度权重：校园二手交易里「同校区」≈「能当面交易」，
/// 是很强的可达性信号，所以单独作为一维打分，而不是混进标签。
pub const W_CAMPUS: f64 = 0.15;
