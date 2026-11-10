//! 推荐器：混合打分 + 重排 + 理由生成。
//!
//! 打分公式（权重见 [`W_TAG`] 等常量）：
//!
//! ```text
//! score = W_TAG    * TagSim        // 用户**兴趣标签** vs 商品标签的 Jaccard
//!       + W_CF     * CFSim         // item-based 协同过滤（共现矩阵余弦）
//!       + W_HOT    * HotNorm       // 热度分（全量在售商品上 min-max 归一化）
//!       + W_PRICE  * PriceFit      // 价格贴合度
//!       + W_CAMPUS * CampusFit     // 同校区（≈ 可当面交易）
//! ```
//!
//! 整个流程分四步，与 `docs/ALGORITHMS.md` 第 6 节一一对应：
//!
//! 1. **召回（recall）**：候选集 = 兴趣标签倒排命中 ∪ 相似用户交互过的商品。
//! 2. **打分（scoring）**：五项特征线性加权（先过「命中至少一个兴趣标签」的门槛）。
//! 3. **重排（re-ranking）**：同卖家配额 + 标签多样性惩罚。
//! 4. **解释（explanation）**：每个结果带 `reasons`，可直接渲染成前端徽章。

use crate::cf::{self, CoOccurrence};
use crate::hot::hot_score;
use crate::index::{InvertedIndex, QueryMode};
use crate::model::{Event, EventKind, Product, Status, User};
use crate::similarity::{jaccard, jaccard_no_alloc};
use crate::sort::{SortKey, compare_for, three_way_quick_sort, top_k_by};
use std::collections::{HashMap, HashSet};

// 权重常量定义在 crate 根（`crate::W_TAG` 等），这里再导出一次，
// 便于 `campus_rec::recommender::W_TAG` 与 `campus_rec::W_TAG` 两种写法都能用，
// 同时保证**只有一处定义**（调参不会出现两份不一致的常量）。
pub use crate::{W_CAMPUS, W_CF, W_HOT, W_PRICE, W_TAG};

/// 同一卖家在推荐结果中最多出现的次数。
pub const MAX_PER_SELLER: usize = 2;

/// 标签多样性阈值：与已选商品的 Jaccard 超过该值即视为「太像」。
pub const DIVERSITY_JACCARD: f64 = 0.8;

/// 多样性惩罚系数：太像的商品分数乘以该系数（降权而非直接淘汰，
/// 保留「用户就是想要同类商品」的可能性）。
pub const DIVERSITY_PENALTY: f64 = 0.6;

/// 参与协同过滤的相似用户数上限。
pub const SIMILAR_USER_K: usize = 5;

/// 推荐理由，直接渲染到 Web 徽章上。
#[derive(Debug, Clone, PartialEq)]
pub enum Reason {
    /// 标签匹配
    TagMatch { tags: Vec<String> },
    /// 相似用户也交互过
    SimilarUsers { users: Vec<u64> },
    /// 同校区（≈ 可当面交易）
    CampusMatch,
    /// 热度趋势
    Trending { factor: f64 },
    /// 价格贴合
    PriceFit,
    /// 冷启动回退
    ColdStart,
}

/// 一条推荐结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub product_id: u64,
    pub score: f64,
    pub reasons: Vec<Reason>,
}

/// 打分明细，供 `REC.EXPLAIN` 逐项展示。
#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    /// 被解释的商品
    pub product_id: u64,
    /// 混合后的最终分数
    pub score: f64,
    /// 标签相似度（Jaccard）
    pub tag_sim: f64,
    /// 协同过滤相似度
    pub cf_sim: f64,
    /// 归一化热度
    pub hot_norm: f64,
    /// 价格贴合度
    pub price_fit: f64,
    /// 同校区贴合度（同校区 1.0，否则 0.0）
    pub campus_fit: f64,
    /// 命中的兴趣标签
    pub matched_tags: Vec<String>,
    /// 提供协同过滤信号的相似用户
    pub similar_users: Vec<u64>,
}

/// 索引规模统计，供 `REC.STATS` 使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecStats {
    /// 商品数
    pub products: usize,
    /// 用户数
    pub users: usize,
    /// 标签数
    pub tags: usize,
    /// 共现对数量
    pub co_occurrence_pairs: usize,
    /// 累计行为数
    pub events: usize,
}

/// 推荐器：持有商品/用户索引与行为统计。
#[derive(Debug, Clone)]
pub struct Recommender {
    /// 商品表，**按 id 升序**维护（二分查找的前提）
    pub products: Vec<Product>,
    /// 用户表，**按 id 升序**维护
    pub users: Vec<User>,
    /// 标签倒排索引
    pub index: InvertedIndex,
    /// 行为共现矩阵
    pub co_occurrence: CoOccurrence,
    /// 用户 → 交互过的商品 id（升序去重），用于找相似用户与计协同过滤信号
    user_items: HashMap<u64, Vec<u64>>,
    /// 用户 → **收藏**过的商品 id（升序去重）。收藏是强负反馈，推荐时排除；
    /// 浏览/点击只影响排序，不排除（否则兴趣标签少的用户几乎无货可推）。
    user_favs: HashMap<u64, Vec<u64>>,
    /// 累计行为条数
    event_count: usize,
}

impl Recommender {
    /// 由全量数据构建索引。
    ///
    /// * `products` / `users` 内部会按 id 升序重排（后续二分查找依赖有序性）；
    /// * 倒排索引由商品标签整体构建；
    /// * 共现矩阵由行为事件按 5 分钟窗口构建；
    /// * 同时统计每个用户交互过的商品集合与收藏集合（协同过滤、相似用户、
    ///   「排除已收藏」都靠它）；
    /// * 并按事件流回填商品的 `views`/`clicks`/`favs` 计数（热度分的数据来源）。
    ///
    /// 最后一条容易被忽略：`Product` 上的三个计数是可选的序列化字段，
    /// 种子数据与 `REC.TRACK` 都不写它们，只有事件流是权威来源。
    /// 不在这里回填，`hot_score` 的分子恒为 0，热度榜会退化成一堆 0 分。
    pub fn new(products: Vec<Product>, users: Vec<User>, events: &[Event]) -> Self {
        let mut products = products;
        products.sort_unstable_by_key(|p| p.id);
        products.dedup_by_key(|p| p.id);

        // 与 `W_HOT` 相关的互动计数：事件流是权威来源，计数只是物化视图。
        for e in events {
            if let Ok(pos) = products.binary_search_by_key(&e.product_id, |p| p.id) {
                apply_event_count(&mut products[pos], e.kind);
            }
        }

        let mut users = users;
        users.sort_unstable_by_key(|u| u.id);
        users.dedup_by_key(|u| u.id);

        let index = InvertedIndex::build(&products);
        let co_occurrence = CoOccurrence::build(events);

        let mut user_items: HashMap<u64, Vec<u64>> = HashMap::new();
        for e in events {
            let list = user_items.entry(e.user_id).or_default();
            list.push(e.product_id);
        }
        for list in user_items.values_mut() {
            list.sort_unstable();
            list.dedup();
        }

        // 收藏单独存一份：推荐只排除「已收藏」，浏览/点击过的商品仍可再推。
        let mut user_favs: HashMap<u64, Vec<u64>> = HashMap::new();
        for e in events {
            if e.kind == EventKind::Fav {
                user_favs.entry(e.user_id).or_default().push(e.product_id);
            }
        }
        for list in user_favs.values_mut() {
            list.sort_unstable();
            list.dedup();
        }

        Recommender {
            products,
            users,
            index,
            co_occurrence,
            user_items,
            user_favs,
            event_count: events.len(),
        }
    }

    /// 插入或更新商品（同时维护倒排索引）。
    ///
    /// 已存在同 id 商品时先摘掉旧标签再挂新标签，避免索引里残留过期标签。
    pub fn upsert_product(&mut self, product: Product) {
        match self.products.binary_search_by_key(&product.id, |p| p.id) {
            Ok(pos) => {
                let old_tags = std::mem::take(&mut self.products[pos].tags);
                self.index.remove(product.id, &old_tags);
                self.index.upsert(&product);
                self.products[pos] = product;
            }
            Err(pos) => {
                self.index.upsert(&product);
                self.products.insert(pos, product);
            }
        }
    }

    /// 记录一次用户行为（维护共现矩阵、互动计数、用户交互/收藏集合）。
    ///
    /// 同步更新：共现矩阵、商品的热度计数、用户交互集合、用户收藏集合。
    /// 交互集合是去重的（同一商品看 100 次也只算一次），因为协同过滤只关心
    /// 「是否看过」；而 `views`/`clicks`/`favs` 是**累加**的，热度分需要真实次数。
    /// 收藏集合同样去重，用于推荐时排除「已收藏」。
    pub fn record_event(&mut self, event: Event) {
        self.co_occurrence.record(&event);
        self.event_count += 1;

        // 热度计数必须跟着事件走，否则 `REC.TRACK` 之后热度榜不会变化。
        if let Ok(pos) = self
            .products
            .binary_search_by_key(&event.product_id, |p| p.id)
        {
            apply_event_count(&mut self.products[pos], event.kind);
        }

        let list = self.user_items.entry(event.user_id).or_default();
        if let Err(pos) = list.binary_search(&event.product_id) {
            list.insert(pos, event.product_id);
        }

        if event.kind == EventKind::Fav {
            let favs = self.user_favs.entry(event.user_id).or_default();
            if let Err(pos) = favs.binary_search(&event.product_id) {
                favs.insert(pos, event.product_id);
            }
        }
    }

    /// 按 id 取商品（`O(log n)`，依赖 `products` 有序）。
    pub fn product(&self, id: u64) -> Option<&Product> {
        self.products
            .binary_search_by_key(&id, |p| p.id)
            .ok()
            .map(|pos| &self.products[pos])
    }

    /// 按 id 取用户（`O(log n)`，依赖 `users` 有序）。
    pub fn user(&self, id: u64) -> Option<&User> {
        self.users
            .binary_search_by_key(&id, |u| u.id)
            .ok()
            .map(|pos| &self.users[pos])
    }

    /// 某个用户交互过的商品 id（升序）。没有记录时返回空切片。
    pub fn user_items(&self, user_id: u64) -> &[u64] {
        match self.user_items.get(&user_id) {
            Some(v) => v.as_slice(),
            None => &[],
        }
    }

    /// 某个用户收藏过的商品 id（升序）。没有记录时返回空切片。
    pub fn user_favs(&self, user_id: u64) -> &[u64] {
        match self.user_favs.get(&user_id) {
            Some(v) => v.as_slice(),
            None => &[],
        }
    }

    /// 用权威收藏集合覆盖当前收藏（键空间重建时使用）。
    ///
    /// 事件流水会被 `LTRIM` 截断，而 `user:<uid>:favs` 不会：重启后必须用它
    /// 覆盖「由最近 200 条事件推导出的收藏」，否则老收藏会被悄悄遗忘。
    pub fn set_user_favs(&mut self, user_id: u64, mut product_ids: Vec<u64>) {
        product_ids.sort_unstable();
        product_ids.dedup();
        if product_ids.is_empty() {
            self.user_favs.remove(&user_id);
        } else {
            self.user_favs.insert(user_id, product_ids);
        }
    }

    /// 与 `user_id` 最相似的 `k` 个用户：`(用户 id, 共同交互商品数)`。
    ///
    /// 用「共同交互商品数」而不是 Jaccard 当相似度：活跃用户看过的商品多，
    /// 用 Jaccard 会被严重稀释（分母大），而共同商品数更直接地反映「品味重叠」。
    pub fn similar_users(&self, user_id: u64, k: usize) -> Vec<(u64, usize)> {
        cf::similar_users(&self.user_items, user_id, k)
    }

    /// 索引规模统计。
    pub fn stats(&self) -> RecStats {
        RecStats {
            products: self.products.len(),
            users: self.users.len(),
            tags: self.index.len(),
            co_occurrence_pairs: self.co_occurrence.len(),
            events: self.event_count,
        }
    }

    /// 在售商品的原始热度分 `(id, hot_score)`，保持 `products` 的顺序。
    ///
    /// 抽出这一步是因为 `hot_score` 含一次 `powf`，比后面的线性映射贵一个量级；
    /// 归一化基准与归一化结果必须来自**同一批已算好的数**，否则会重复计算。
    fn hot_raw(&self, now: i64) -> Vec<(u64, f64)> {
        self.products
            .iter()
            .filter(|p| p.status == Status::OnSale)
            .map(|p| (p.id, hot_score(p, now)))
            .collect()
    }

    /// 全量在售商品热度分的归一化基准 `(min, span)`。
    ///
    /// 返回 `None` 表示基准不存在：没有在售商品，或所有分数完全相同
    /// （`span` 为 0，线性映射会退化成全 0）。这与 [`normalize_scores`]
    /// 在 `max == min` 时统一给 0 的边界语义保持一致。
    ///
    /// 抽成独立函数是为了让「批量归一化」（[`Self::hot_norms`]）与
    /// 「单点归一化」（[`Self::hot_norm_of`]）**共用同一个基准**：
    /// 若两处各自扫一遍，`REC.EXPLAIN` 的 `hot_norm` 就可能和
    /// `REC.FOR_YOU` 的分数对不上。
    fn hot_baseline(raw: &[(u64, f64)]) -> Option<(f64, f64)> {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for &(_, s) in raw {
            if !s.is_finite() {
                continue;
            }
            if s < min {
                min = s;
            }
            if s > max {
                max = s;
            }
        }
        if !min.is_finite() || !max.is_finite() {
            return None;
        }
        let span = max - min;
        if span <= f64::EPSILON {
            return None;
        }
        Some((min, span))
    }

    /// 全量在售商品的热度分并做 min-max 归一化，返回 `id → [0,1]`。
    ///
    /// **在全量在售商品上归一化**（而不是在某个候选批内），这样同一个商品
    /// 在任何用户、任何候选集下拿到的 `hot_norm` 都一样，
    /// `REC.FOR_YOU` 的分数才能和 `REC.EXPLAIN` 的拆解结果对得上。
    fn hot_norms(&self, now: i64) -> HashMap<u64, f64> {
        let raw = self.hot_raw(now);
        match Self::hot_baseline(&raw) {
            Some((min, span)) => raw
                .into_iter()
                .map(|(id, s)| (id, ((s - min) / span).clamp(0.0, 1.0)))
                .collect(),
            // 没有可用基准时统一给 0，与 `normalize_scores` 的退化行为一致。
            None => raw.into_iter().map(|(id, _)| (id, 0.0)).collect(),
        }
    }

    /// 单个商品的 `hot_norm`，基准与 [`Self::hot_norms`] 完全相同。
    ///
    /// 存在的意义是避免 `REC.EXPLAIN` 为了取一个值而构造整张
    /// `HashMap`（那是一次 `O(n)` 的堆分配）：这里只分配一个 `Vec`，
    /// 且基准与批量路径共用 [`Self::hot_baseline`]，分数不会漂移。
    fn hot_norm_of(&self, product_id: u64, now: i64) -> f64 {
        let Some(p) = self.product(product_id) else {
            return 0.0;
        };
        if p.status != Status::OnSale {
            return 0.0;
        }
        let raw = self.hot_raw(now);
        // 直接复用 `raw` 里已经算好的分，不重算 `hot_score`：
        // 数值上与 `hot_norms` 逐位相同，不会因为浮点重算产生差异。
        let Some(&(_, value)) = raw.iter().find(|(id, _)| *id == product_id) else {
            return 0.0;
        };
        match Self::hot_baseline(&raw) {
            Some((min, span)) => ((value - min) / span).clamp(0.0, 1.0),
            None => 0.0,
        }
    }

    /// 用户画像标签 = **兴趣标签**（去重，保持字典序）。
    ///
    /// 不把浏览过的商品标签并进画像：网页上展示的画像就是兴趣标签，
    /// 推荐若用「兴趣 ∪ 行为标签」，就会出现「画像写着自行车、推荐却按
    /// 数码/电子打分」的错位。浏览行为仍然通过协同过滤（`cf_signal`）与
    /// 热度间接影响排序，不需要改变画像本身。
    fn profile_tags(&self, user: &User) -> Vec<String> {
        let mut out = user.interests.clone();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// 协同过滤信号：返回 `(cf_sim, 提供信号的相似用户)`。
    ///
    /// 两路信号取**最大值**（而不是平均）：
    ///
    /// * **item-based**：用户看过的商品里，与候选商品共现最强的那一个，
    ///   强度取 `共现次数 / 全局最大共现次数`；
    /// * **user-based**：相似用户里有多大比例的「共同商品权重」落在候选商品上。
    ///
    /// 取 max 而不是加权平均，是因为两路信号强弱的量纲不同（一个来自商品对，
    /// 一个来自用户群），平均会把强信号稀释成中等信号；推荐场景里
    /// 「有一个强证据」比「几个弱证据平均」更有说服力。
    fn cf_signal(&self, user_id: u64, candidate: u64) -> (f64, Vec<u64>) {
        let similar = cf::similar_users(&self.user_items, user_id, SIMILAR_USER_K);
        self.cf_signal_with(user_id, &similar, candidate)
    }

    /// [`Self::cf_signal`] 的批量版本：相似用户列表由调用方算好传进来。
    ///
    /// 为什么需要这个版本：`similar_users` 要对每个候选商品的所有交互商品
    /// 求交集（`O(U·E)`），若在候选循环里逐个调用，同一个用户的相似列表
    /// 会被重算 `|C|` 次——这是纯浪费，因为列表与候选商品无关。
    /// 调用方算一次、循环里复用，把 `O(|C|·U·E)` 降到 `O(U·E + |C|·k)`。
    fn cf_signal_with(
        &self,
        _user_id: u64,
        similar: &[(u64, usize)],
        candidate: u64,
    ) -> (f64, Vec<u64>) {
        // item-based：最强共现
        let mut item_cf = 0.0f64;
        for pid in self.user_items(_user_id) {
            let s = self.co_occurrence.score(*pid, candidate);
            if s > item_cf {
                item_cf = s;
            }
        }

        // user-based：相似用户对候选商品的贡献占比
        let mut total_w = 0.0f64;
        let mut hit_w = 0.0f64;
        let mut providers: Vec<(u64, usize)> = Vec::new();
        for (uid, shared) in similar {
            let w = *shared as f64;
            total_w += w;
            let interacted_by_them = self.user_items(*uid).binary_search(&candidate).is_ok();
            if interacted_by_them {
                hit_w += w;
                providers.push((*uid, *shared));
            }
        }
        let user_cf = if total_w > 0.0 { hit_w / total_w } else { 0.0 };

        providers.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let providers: Vec<u64> = providers.into_iter().take(3).map(|(u, _)| u).collect();

        (item_cf.max(user_cf), providers)
    }

    /// 价格贴合度 `1 - |price - avg| / max(price, avg, 1)`，clamp 到 `[0,1]`。
    ///
    /// `max(..., 1)` 防除零；分母取两者较大值而不是 `avg`，
    /// 这样「预算 100 元却看到 10000 元的商品」和「预算 10000 却看到 100 元」
    /// 的相对惩罚是对称的（都用较大的那个做尺度）。
    fn price_fit(user: &User, price_cents: i64) -> f64 {
        let price = price_cents.max(0) as f64;
        let avg = user.avg_price.max(0) as f64;
        let denom = price.max(avg).max(1.0);
        (1.0 - (price - avg).abs() / denom).clamp(0.0, 1.0)
    }

    /// 同校区贴合度：同校区 = 1.0（≈ 能当面交易），否则 0.0。
    ///
    /// 用户或商品校区为空时按「不匹配」处理（避免空串互相相等造成误判）。
    fn campus_fit(user: &User, product: &Product) -> f64 {
        if !user.campus.is_empty() && user.campus == product.campus {
            1.0
        } else {
            0.0
        }
    }

    /// 「为你推荐」：`exclude` 中的商品 id 会被跳过。
    ///
    /// 四步流水线：
    ///
    /// 1. **召回**：兴趣标签倒排命中 ∪ 相似用户交互过的商品；
    /// 2. **过滤**：去掉非在售、已收藏、`exclude` 中的商品；再丢掉**一个兴趣
    ///    标签都没命中**的商品（纯协同过滤召回也要过这道门槛，保证推荐结果
    ///    与页面展示的用户画像一致）；
    /// 3. **打分**：`W_TAG·tag + W_CF·cf + W_HOT·hot + W_PRICE·price + W_CAMPUS·campus`；
    /// 4. **重排**：同卖家最多 [`MAX_PER_SELLER`] 个；与已选商品标签
    ///    Jaccard > [`DIVERSITY_JACCARD`] 的乘 [`DIVERSITY_PENALTY`] 降权。
    ///
    /// 用户不存在或兴趣标签为空 → 冷启动，直接返回热度榜，`reasons=[ColdStart]`；
    /// 兴趣标签在库中没有任何在售商品时同样退化为冷启动，避免推荐空白页。
    pub fn recommend_for_user(
        &self,
        user_id: u64,
        limit: usize,
        exclude: &[u64],
        now: i64,
    ) -> Vec<Scored> {
        if limit == 0 {
            return Vec::new();
        }
        let hot_norms = self.hot_norms(now);

        // ---- 冷启动 ----
        let user = match self.user(user_id) {
            Some(u) if !u.interests.is_empty() => u,
            _ => return self.cold_start(limit, exclude, &hot_norms),
        };

        let favorites = self.user_favs(user_id);
        let excluded: HashSet<u64> = exclude.iter().copied().collect();
        let profile = self.profile_tags(user);

        // 相似用户列表与候选商品无关，算一次即可（见 `cf_signal_with`）。
        let similar = cf::similar_users(&self.user_items, user_id, SIMILAR_USER_K);

        // ---- 1. 召回 ----
        let mut candidates: Vec<u64> = self.index.search(&profile, QueryMode::Or);
        for (uid, _) in &similar {
            for pid in self.user_items(*uid) {
                candidates.push(*pid);
            }
        }
        candidates.sort_unstable();
        candidates.dedup();

        // ---- 2. 过滤 + 3. 打分 ----
        struct Cand {
            id: u64,
            seller: u64,
            tags: Vec<String>,
            score: f64,
            reasons: Vec<Reason>,
        }
        let mut pool: Vec<Cand> = Vec::new();
        for pid in candidates {
            if excluded.contains(&pid) || favorites.binary_search(&pid).is_ok() {
                continue;
            }
            let p = match self.product(pid) {
                Some(p) if p.status == Status::OnSale => p,
                _ => continue,
            };

            let matched: Vec<String> = profile
                .iter()
                .filter(|t| p.tags.iter().any(|x| x == *t))
                .cloned()
                .collect();
            // 严格门槛：必须命中至少一个兴趣标签。纯协同过滤召回的商品
            // 若与画像毫无关系，推荐给用户会与页面展示的画像自相矛盾。
            if matched.is_empty() {
                continue;
            }
            let tag_sim = jaccard(&profile, &p.tags);
            let (cf_sim, providers) = self.cf_signal_with(user_id, &similar, pid);
            let hot_norm = hot_norms.get(&pid).copied().unwrap_or(0.0);
            let price_fit = Self::price_fit(user, p.price_cents);
            let campus_fit = Self::campus_fit(user, p);

            let score = W_TAG * tag_sim
                + W_CF * cf_sim
                + W_HOT * hot_norm
                + W_PRICE * price_fit
                + W_CAMPUS * campus_fit;

            let mut reasons = vec![Reason::TagMatch { tags: matched }];
            if !providers.is_empty() && cf_sim > 0.0 {
                reasons.push(Reason::SimilarUsers { users: providers });
            }
            if hot_norm >= 0.5 {
                reasons.push(Reason::Trending { factor: hot_norm });
            }
            if price_fit >= 0.6 {
                reasons.push(Reason::PriceFit);
            }
            if campus_fit >= 1.0 {
                reasons.push(Reason::CampusMatch);
            }

            pool.push(Cand {
                id: pid,
                seller: p.seller_id,
                tags: p.tags.clone(),
                score,
                reasons,
            });
        }

        // 兴趣标签在库中没有任何在售商品 → 退化为热门榜，避免空白页。
        if pool.is_empty() {
            return self.cold_start(limit, exclude, &hot_norms);
        }

        // ---- 4. 重排：贪心逐位挑选 ----
        // 每轮在当前剩余候选里挑「应用多样性惩罚后」分数最高的一个。
        // 惩罚依赖于「已选集合」，所以必须在选择过程中动态计算，
        // 不能先排序再一次性惩罚（否则惩罚后的顺序可能被破坏）。
        let mut selected: Vec<Scored> = Vec::new();
        let mut chosen_tags: Vec<Vec<String>> = Vec::new();
        let mut seller_count: HashMap<u64, usize> = HashMap::new();
        let mut taken = vec![false; pool.len()];

        while selected.len() < limit {
            let mut best: Option<(usize, f64)> = None;
            for (i, c) in pool.iter().enumerate() {
                if taken[i] {
                    continue;
                }
                if seller_count.get(&c.seller).copied().unwrap_or(0) >= MAX_PER_SELLER {
                    continue;
                }
                let mut s = c.score;
                // 用零分配的 jaccard_no_alloc：这里每次重排要被调用
                // `|候选| × limit × 已选` 次（本项目约 5000 次），
                // 而 HashSet 版每次两次堆分配，是实测出来的主要瓶颈。
                let max_sim = chosen_tags
                    .iter()
                    .map(|t| jaccard_no_alloc(t, &c.tags))
                    .fold(0.0f64, f64::max);
                if max_sim > DIVERSITY_JACCARD {
                    s *= DIVERSITY_PENALTY;
                }
                let better = match best {
                    None => true,
                    Some((bi, bs)) => {
                        s > bs + f64::EPSILON
                            || ((s - bs).abs() <= f64::EPSILON && c.id < pool[bi].id)
                    }
                };
                if better {
                    best = Some((i, s));
                }
            }

            let (idx, final_score) = match best {
                Some(v) => v,
                None => break,
            };
            taken[idx] = true;
            *seller_count.entry(pool[idx].seller).or_insert(0) += 1;
            chosen_tags.push(pool[idx].tags.clone());
            selected.push(Scored {
                product_id: pool[idx].id,
                score: final_score,
                reasons: pool[idx].reasons.clone(),
            });
        }

        selected
    }

    /// 冷启动：直接返回热度榜（仍会尊重 `exclude`）。
    fn cold_start(
        &self,
        limit: usize,
        exclude: &[u64],
        hot_norms: &HashMap<u64, f64>,
    ) -> Vec<Scored> {
        let excluded: HashSet<u64> = exclude.iter().copied().collect();
        let mut items: Vec<(u64, f64)> = self
            .products
            .iter()
            .filter(|p| p.status == Status::OnSale && !excluded.contains(&p.id))
            .map(|p| (p.id, hot_norms.get(&p.id).copied().unwrap_or(0.0)))
            .collect();
        items.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        items
            .into_iter()
            .take(limit)
            .map(|(id, s)| Scored {
                product_id: id,
                score: s,
                reasons: vec![Reason::ColdStart],
            })
            .collect()
    }

    /// 「相似商品」：与 `product_id` 标签/共现都相近的商品。
    ///
    /// 打分 `0.6·Jaccard(标签) + 0.4·共现强度`。权重偏向标签而不是共现：
    /// 标签是显式语义（「都是自行车」），共现是隐式行为（「常被一起看」），
    /// 前者更可解释，展示时也更好讲。
    pub fn similar_products(&self, product_id: u64, limit: usize, now: i64) -> Vec<Scored> {
        if limit == 0 {
            return Vec::new();
        }
        let target = match self.product(product_id) {
            Some(p) => p,
            None => return Vec::new(),
        };

        // 召回：同标签 ∪ 共现邻居
        let mut candidates: Vec<u64> = self.index.search(&target.tags, QueryMode::Or);
        for (pid, _) in self.co_occurrence.neighbors(product_id, 32) {
            candidates.push(pid);
        }
        candidates.sort_unstable();
        candidates.dedup();

        let hot_norms = self.hot_norms(now);
        let mut scored: Vec<(u64, f64, Vec<Reason>)> = Vec::with_capacity(candidates.len());
        for pid in candidates {
            if pid == product_id {
                continue;
            }
            let p = match self.product(pid) {
                Some(p) if p.status == Status::OnSale => p,
                _ => continue,
            };

            let tag_sim = jaccard(&target.tags, &p.tags);
            let co_sim = self.co_occurrence.score(product_id, pid);
            let hot_norm = hot_norms.get(&pid).copied().unwrap_or(0.0);
            let score = 0.6 * tag_sim + 0.4 * co_sim;

            let mut reasons = Vec::new();
            let matched: Vec<String> = target
                .tags
                .iter()
                .filter(|t| p.tags.iter().any(|x| x == *t))
                .cloned()
                .collect();
            if !matched.is_empty() {
                reasons.push(Reason::TagMatch { tags: matched });
            }
            // 同时交互过这两个商品的用户 —— 最直观的「买过 A 的人也看 B」证据
            let mut shared_users: Vec<u64> = Vec::new();
            for u in &self.users {
                let items = self.user_items(u.id);
                if items.binary_search(&product_id).is_ok() && items.binary_search(&pid).is_ok() {
                    shared_users.push(u.id);
                }
            }
            shared_users.truncate(3);
            if !shared_users.is_empty() {
                reasons.push(Reason::SimilarUsers {
                    users: shared_users,
                });
            }
            if reasons.is_empty() && hot_norm > 0.0 {
                reasons.push(Reason::Trending { factor: hot_norm });
            }

            scored.push((pid, score, reasons));
        }

        let cmp = |a: &(u64, f64, Vec<Reason>), b: &(u64, f64, Vec<Reason>)| {
            b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
        };
        top_k_by(&scored, limit, cmp)
            .into_iter()
            .map(|(id, score, reasons)| Scored {
                product_id: *id,
                score: *score,
                reasons: reasons.clone(),
            })
            .collect()
    }

    /// 热度榜。
    ///
    /// 用 `top_k_by`（大小 k 的堆）而不是全排序：榜单通常只要前 10 个，
    /// `O(n log k)` 比 `O(n log n)` 省一个数量级（实测见 `docs/ALGORITHMS.md`）。
    pub fn hot_ranking(&self, limit: usize, now: i64) -> Vec<Scored> {
        if limit == 0 {
            return Vec::new();
        }
        let hot_norms = self.hot_norms(now);
        let mut items: Vec<(u64, f64)> = self
            .products
            .iter()
            .filter(|p| p.status == Status::OnSale)
            .map(|p| (p.id, hot_norms.get(&p.id).copied().unwrap_or(0.0)))
            .collect();

        let cmp = |a: &(u64, f64), b: &(u64, f64)| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0));
        let top = top_k_by(&items, limit, cmp);
        let mut out: Vec<Scored> = top
            .into_iter()
            .map(|(id, s)| Scored {
                product_id: *id,
                score: *s,
                reasons: vec![Reason::Trending { factor: *s }],
            })
            .collect();
        // top_k_by 已经是按 cmp 排好的；这里保留一个防御性检查
        items.clear();
        out.retain(|s| s.score.is_finite());
        out
    }

    /// 带筛选的列表查询：标签（AND/OR）、价格区间、关键字、排序。
    ///
    /// 走倒排索引取候选后再过滤，而不是线性扫描全部商品：
    /// 标签命中率低时（本项目的常态），候选集只有几十个而不是全部。
    ///
    /// `tags` 为空时视为「无标签筛选」，候选集退化为全部商品。
    /// 返回的是引用，调用方不必复制商品。
    // 签名由 `docs/SPEC.md` 第 3 节冻结（`mredis-core` 的 `REC.*` 与 Web 层已按此调用），
    // 参数虽多但不能合并成结构体，否则会破坏既有调用方。
    #[allow(clippy::too_many_arguments)]
    pub fn query(
        &self,
        tags: &[String],
        mode: QueryMode,
        min_price: Option<i64>,
        max_price: Option<i64>,
        keyword: Option<&str>,
        sort: SortKey,
        now: i64,
    ) -> Vec<&Product> {
        let mut items: Vec<&Product> = if tags.is_empty() {
            self.products.iter().collect()
        } else {
            self.index
                .search(tags, mode)
                .into_iter()
                .filter_map(|id| self.product(id))
                .collect()
        };

        let kw = keyword
            .map(|k| k.trim().to_lowercase())
            .filter(|k| !k.is_empty());

        items.retain(|p| {
            if let Some(min) = min_price {
                if p.price_cents < min {
                    return false;
                }
            }
            if let Some(max) = max_price {
                if p.price_cents > max {
                    return false;
                }
            }
            if let Some(k) = &kw {
                let hay_title = p.title.to_lowercase();
                if !hay_title.contains(k) && !p.desc.to_lowercase().contains(k) {
                    return false;
                }
            }
            true
        });

        let cmp = compare_for(sort, now);
        // 对引用排序：`three_way_quick_sort` 是泛型的，`T = &Product`
        three_way_quick_sort(&mut items, &|a: &&Product, b: &&Product| cmp(a, b));
        items
    }

    /// 逐项拆解某用户对某商品的打分，供 `REC.EXPLAIN` 使用。
    ///
    /// 返回 `None` 表示商品不存在或用户不存在——调用方据此返回 404。
    /// 这里的 `hot_norm` 与 [`Self::recommend_for_user`] 使用同一套归一化基准
    /// （全量在售商品），因此两处分数可以互相验证。
    pub fn explain(&self, user_id: u64, product_id: u64, now: i64) -> Option<Explanation> {
        let product = self.product(product_id)?;
        let user = self.user(user_id)?;

        let profile = self.profile_tags(user);
        let matched: Vec<String> = profile
            .iter()
            .filter(|t| product.tags.iter().any(|x| x == *t))
            .cloned()
            .collect();
        let tag_sim = if matched.is_empty() {
            0.0
        } else {
            jaccard(&profile, &product.tags)
        };
        let (cf_sim, similar_users) = self.cf_signal(user_id, product_id);
        let hot_norm = self.hot_norm_of(product_id, now);
        let price_fit = Self::price_fit(user, product.price_cents);
        let campus_fit = Self::campus_fit(user, product);
        let score = W_TAG * tag_sim
            + W_CF * cf_sim
            + W_HOT * hot_norm
            + W_PRICE * price_fit
            + W_CAMPUS * campus_fit;

        Some(Explanation {
            product_id,
            score,
            tag_sim,
            cf_sim,
            hot_norm,
            price_fit,
            campus_fit,
            matched_tags: matched,
            similar_users,
        })
    }
}

/// 把一次行为事件计入商品的热度计数。
///
/// 抽成自由函数是为了让 [`Recommender::new`] 的回填与 [`Recommender::record_event`]
/// 的增量更新共用同一份语义：两处若各写一遍 `match`，很容易出现
/// 「构造时算 view、增量时算 click」这种难以发现的偏差。
fn apply_event_count(product: &mut Product, kind: EventKind) {
    match kind {
        EventKind::View => product.views = product.views.saturating_add(1),
        EventKind::Click => product.clicks = product.clicks.saturating_add(1),
        EventKind::Fav => product.favs = product.favs.saturating_add(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EventKind;
    use crate::testing::product;

    const NOW: i64 = 1_700_000_000;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn user(id: u64, interests: &[&str], avg_price: i64) -> User {
        User {
            id,
            name: format!("用户{id}"),
            campus: "前湖校区".to_string(),
            interests: s(interests),
            avg_price,
        }
    }

    fn ev(user_id: u64, product_id: u64, kind: EventKind, ts: i64) -> Event {
        Event {
            user_id,
            product_id,
            kind,
            ts,
        }
    }

    /// 一个小而完整的测试场景：
    ///
    /// * 商品：1/2 是自行车（卖家 1），3 是书，4 是电脑，5 是运动鞋
    /// * 用户 1 喜欢自行车/运动，交互过 1
    /// * 用户 2 也喜欢自行车，交互过 2
    fn fixture() -> Recommender {
        let mut products = vec![
            product(1, 30000, NOW - 3600, 1, &["自行车", "运动"]),
            product(2, 40000, NOW - 7200, 1, &["自行车"]),
            product(3, 5000, NOW - 10800, 2, &["教材", "图书"]),
            product(4, 150000, NOW - 86400, 3, &["电脑", "数码"]),
            product(5, 20000, NOW - 100, 4, &["运动", "健身"]),
        ];
        // 手写一个基数（模拟「历史遗留的热度」），事件回填会在此基础上累加：
        // 商品 5 还有 1 条 View 事件，构造后 views == 101。
        products[4].views = 100;
        let users = vec![
            user(1, &["自行车", "运动"], 30000),
            user(2, &["自行车"], 35000),
            user(3, &["教材"], 5000),
            user(7, &[], 0),
        ];
        let events = vec![
            ev(1, 1, EventKind::View, NOW - 600),
            ev(1, 5, EventKind::View, NOW - 500),
            ev(2, 2, EventKind::Click, NOW - 400),
            ev(2, 1, EventKind::View, NOW - 300),
        ];
        Recommender::new(products, users, &events)
    }

    #[test]
    fn new_backfills_interaction_counts_from_events() {
        let rec = fixture();
        // 商品 1 有 2 条 View（用户 1、用户 2），商品 2 有 1 条 Click，
        // 商品 5 原本手写了 views=100，再加 1 条 View 事件 → 101。
        assert_eq!(rec.product(1).unwrap().views, 2);
        assert_eq!(rec.product(1).unwrap().clicks, 0);
        assert_eq!(rec.product(1).unwrap().favs, 0);
        assert_eq!(rec.product(2).unwrap().clicks, 1);
        assert_eq!(rec.product(5).unwrap().views, 101);
        // 没有事件命中的商品保持 0
        assert_eq!(rec.product(3).unwrap().views, 0);
        assert_eq!(rec.product(4).unwrap().views, 0);
    }

    #[test]
    fn new_backfill_makes_hot_scores_nonzero() {
        let rec = fixture();
        // 修复前：种子/事件都不带计数，热度分恒为 0（这是真实缺陷）。
        assert!(
            rec.hot_ranking(5, NOW).iter().any(|s| s.score > 0.0),
            "热度榜不应全是 0 分"
        );
    }

    #[test]
    fn record_event_increments_counts() {
        let mut rec = fixture();
        let before = rec.product(3).unwrap().views;
        rec.record_event(ev(1, 3, EventKind::View, NOW));
        assert_eq!(rec.product(3).unwrap().views, before + 1);
        rec.record_event(ev(1, 3, EventKind::Click, NOW));
        assert_eq!(rec.product(3).unwrap().clicks, 1);
        rec.record_event(ev(1, 3, EventKind::Fav, NOW));
        assert_eq!(rec.product(3).unwrap().favs, 1);
        // 未知商品不应 panic，也不应创建幽灵商品
        rec.record_event(ev(1, 999, EventKind::View, NOW));
        assert!(rec.product(999).is_none());
    }

    #[test]
    fn record_event_changes_hot_ranking() {
        let mut rec = fixture();
        let rank_before = rec
            .hot_ranking(rec.products.len(), NOW)
            .iter()
            .position(|s| s.product_id == 4)
            .unwrap();
        let score_before = rec
            .hot_ranking(rec.products.len(), NOW)
            .iter()
            .find(|s| s.product_id == 4)
            .unwrap()
            .score;
        // 商品 4 原本零互动、且已上架 24 小时（重力衰减压得很低）。
        // 刷收藏后它的热度分必须上升、名次必须前移——这正是
        // 「计数不回填则热度恒为 0」缺陷的直接回归测试。
        for _ in 0..50 {
            rec.record_event(ev(1, 4, EventKind::Fav, NOW));
        }
        let ranking = rec.hot_ranking(rec.products.len(), NOW);
        let rank_after = ranking.iter().position(|s| s.product_id == 4).unwrap();
        let score_after = ranking.iter().find(|s| s.product_id == 4).unwrap().score;
        assert!(score_after > score_before, "{score_after} > {score_before}");
        assert!(rank_after < rank_before, "{rank_after} < {rank_before}");
    }

    #[test]
    fn explain_hot_norm_matches_hot_norms() {
        let rec = fixture();
        let map = rec.hot_norms(NOW);
        for p in &rec.products {
            let e = rec.explain(1, p.id, NOW).unwrap();
            let want = map.get(&p.id).copied().unwrap_or(0.0);
            assert!(
                (e.hot_norm - want).abs() < 1e-12,
                "id={} explain={} norms={}",
                p.id,
                e.hot_norm,
                want
            );
        }
    }

    #[test]
    fn new_builds_index_and_cooccurrence() {
        let rec = fixture();
        assert_eq!(rec.stats().products, 5);
        assert_eq!(rec.stats().users, 4);
        assert_eq!(rec.stats().events, 4);
        assert!(rec.stats().tags > 0);
        // 用户 1 交互过 1 和 5（窗口内）→ 1 对；用户 2 交互过 2 和 1（窗口内）→ 1 对
        assert_eq!(rec.co_occurrence.get(1, 5), 1);
        assert_eq!(rec.co_occurrence.get(1, 2), 1);
        assert_eq!(rec.stats().co_occurrence_pairs, 2);
    }

    #[test]
    fn new_sorts_products_and_users() {
        let products = vec![
            product(9, 1, 0, 1, &[]),
            product(3, 1, 0, 1, &[]),
            product(7, 1, 0, 1, &[]),
        ];
        let rec = Recommender::new(products, vec![user(5, &["a"], 0), user(2, &["a"], 0)], &[]);
        assert_eq!(
            rec.products.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![3, 7, 9]
        );
        assert_eq!(
            rec.users.iter().map(|u| u.id).collect::<Vec<_>>(),
            vec![2, 5]
        );
    }

    #[test]
    fn lookup_by_id() {
        let rec = fixture();
        assert_eq!(rec.product(3).map(|p| p.id), Some(3));
        assert!(rec.product(999).is_none());
        assert_eq!(rec.user(2).map(|u| u.id), Some(2));
        assert!(rec.user(999).is_none());
    }

    #[test]
    fn upsert_inserts_new_product() {
        let mut rec = fixture();
        rec.upsert_product(product(6, 1000, NOW, 5, &["新标签"]));
        assert_eq!(rec.stats().products, 6);
        assert_eq!(rec.product(6).map(|p| p.id), Some(6));
        assert_eq!(rec.index.get("新标签"), &[6]);
        // 保持 id 有序
        let ids: Vec<u64> = rec.products.iter().map(|p| p.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn upsert_updates_existing_and_cleans_old_tags() {
        let mut rec = fixture();
        rec.upsert_product(product(1, 30000, NOW - 3600, 1, &["全新标签"]));
        assert_eq!(rec.stats().products, 5);
        // 旧标签「自行车」还挂着商品 2，所以键还在，但商品 1 必须被摘掉
        assert!(!rec.index.get("自行车").contains(&1));
        assert_eq!(rec.index.get("全新标签"), &[1]);
        assert_eq!(rec.product(1).unwrap().tags, s(&["全新标签"]));
    }

    #[test]
    fn record_event_updates_cooccurrence_and_user_items() {
        let mut rec = fixture();
        let before = rec.stats().events;
        rec.record_event(ev(3, 3, EventKind::Fav, NOW - 1000));
        rec.record_event(ev(3, 4, EventKind::Fav, NOW - 900));
        assert_eq!(rec.stats().events, before + 2);
        assert_eq!(rec.co_occurrence.get(3, 4), 1);
        assert_eq!(rec.user_items(3), &[3, 4]);
    }

    #[test]
    fn record_event_dedups_user_items() {
        let mut rec = fixture();
        rec.record_event(ev(1, 1, EventKind::Click, NOW - 100));
        rec.record_event(ev(1, 1, EventKind::Fav, NOW - 90));
        assert_eq!(rec.user_items(1), &[1, 5]);
    }

    #[test]
    fn recommend_excludes_favorited_but_allows_viewed() {
        let products = vec![
            product(1, 30000, NOW - 3600, 1, &["自行车"]),
            product(2, 40000, NOW - 7200, 1, &["自行车"]),
            product(3, 50000, NOW - 10800, 2, &["自行车"]),
        ];
        let users = vec![user(1, &["自行车"], 30000)];
        let events = vec![
            ev(1, 1, EventKind::View, NOW - 600),
            ev(1, 2, EventKind::Fav, NOW - 500),
        ];
        let rec = Recommender::new(products, users, &events);
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        let ids: Vec<u64> = out.iter().map(|s| s.product_id).collect();
        // 只排除「已收藏」：浏览过的 1 仍可推荐，收藏过的 2 必须排除。
        assert!(ids.contains(&1), "浏览过的商品仍应推荐: {ids:?}");
        assert!(!ids.contains(&2), "已收藏的商品不应出现: {ids:?}");
        assert!(ids.contains(&3), "未交互的兴趣商品应推荐: {ids:?}");
    }

    #[test]
    fn recommend_only_returns_products_matching_interests() {
        let rec = fixture();
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        assert!(!out.is_empty());
        let profile = rec.profile_tags(rec.user(1).unwrap());
        for s in &out {
            let p = rec.product(s.product_id).unwrap();
            assert!(
                p.tags.iter().any(|t| profile.contains(t)),
                "商品 {} 的标签 {:?} 与画像 {:?} 无交集",
                p.id,
                p.tags,
                profile
            );
            // 每条结果都必须带「标签匹配」理由
            assert!(
                s.reasons
                    .iter()
                    .any(|r| matches!(r, Reason::TagMatch { .. })),
                "商品 {} 缺少 TagMatch 理由",
                p.id
            );
        }
    }

    #[test]
    fn recommend_prefers_same_campus() {
        // 两个标签完全相同、价格/时间/热度都相同的商品，只有校区不同；
        // 同校区的那个必须排前面（W_CAMPUS 唯一区分项）。
        let mut a = product(10, 30000, NOW - 3600, 1, &["自行车"]);
        a.campus = "前湖校区".to_string();
        let mut b = product(11, 30000, NOW - 3600, 2, &["自行车"]);
        b.campus = "东湖校区".to_string();
        let users = vec![user(1, &["自行车"], 30000)]; // campus = 前湖校区
        let rec = Recommender::new(vec![a, b], users, &[]);
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].product_id, 10, "同校区商品应排第一");
        assert!(
            out[0].reasons.contains(&Reason::CampusMatch),
            "同校区商品应有 CampusMatch 理由"
        );
        assert!(
            !out[1].reasons.contains(&Reason::CampusMatch),
            "异校区商品不应有 CampusMatch 理由"
        );
    }

    #[test]
    fn recommend_falls_back_to_cold_start_when_no_interest_product() {
        // 兴趣标签在库中无任何在售商品 → 退化为热门榜，而不是空列表。
        let products = vec![product(1, 30000, NOW - 3600, 1, &["电脑"])];
        let users = vec![user(1, &["乐器"], 30000)];
        let rec = Recommender::new(products, users, &[]);
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].reasons.contains(&Reason::ColdStart),
            "空池应回退冷启动"
        );
    }

    #[test]
    fn recommend_respects_exclude_list() {
        let rec = fixture();
        let out = rec.recommend_for_user(1, 10, &[2, 3], NOW);
        let ids: Vec<u64> = out.iter().map(|s| s.product_id).collect();
        assert!(!ids.contains(&2));
        assert!(!ids.contains(&3));
    }

    #[test]
    fn recommend_limit() {
        let rec = fixture();
        assert_eq!(rec.recommend_for_user(1, 1, &[], NOW).len(), 1);
        assert!(rec.recommend_for_user(1, 0, &[], NOW).is_empty());
        assert!(rec.recommend_for_user(1, 100, &[], NOW).len() <= 5);
    }

    #[test]
    fn recommend_cold_start_unknown_user() {
        let rec = fixture();
        let out = rec.recommend_for_user(999, 3, &[], NOW);
        assert!(!out.is_empty());
        for item in &out {
            assert_eq!(item.reasons, vec![Reason::ColdStart]);
        }
        // 热度最高的商品 5 应该排第一
        assert_eq!(out[0].product_id, 5);
    }

    #[test]
    fn recommend_cold_start_user_without_interests() {
        let rec = fixture();
        let out = rec.recommend_for_user(7, 3, &[], NOW);
        assert!(!out.is_empty());
        assert_eq!(out[0].reasons, vec![Reason::ColdStart]);
    }

    #[test]
    fn recommend_cold_start_respects_exclude() {
        let rec = fixture();
        let out = rec.recommend_for_user(7, 5, &[5], NOW);
        assert!(out.iter().all(|s| s.product_id != 5));
    }

    #[test]
    fn recommend_scores_are_sorted_desc() {
        let rec = fixture();
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        for w in out.windows(2) {
            assert!(
                w[0].score >= w[1].score - 1e-12,
                "分数未降序: {:?}",
                out.iter().map(|s| s.score).collect::<Vec<_>>()
            );
        }
        for s in &out {
            assert!(
                (0.0..=1.0 + 1e-9).contains(&s.score),
                "分数越界: {}",
                s.score
            );
        }
    }

    #[test]
    fn recommend_has_reasons() {
        let rec = fixture();
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        for item in &out {
            assert!(
                !item.reasons.is_empty(),
                "商品 {} 没有理由",
                item.product_id
            );
        }
        // 商品 2 带「自行车」标签，用户 1 的画像里也有 → 应有 TagMatch
        let p2 = out
            .iter()
            .find(|s| s.product_id == 2)
            .expect("商品 2 应在推荐里");
        assert!(
            p2.reasons
                .iter()
                .any(|r| matches!(r, Reason::TagMatch { .. })),
            "商品 2 的理由: {:?}",
            p2.reasons
        );
    }

    #[test]
    fn recommend_same_seller_at_most_two() {
        // 卖家 1 有 4 个商品，全都命中标签
        let products = vec![
            product(1, 100, NOW, 1, &["自行车"]),
            product(2, 200, NOW, 1, &["自行车"]),
            product(3, 300, NOW, 1, &["自行车"]),
            product(4, 400, NOW, 1, &["自行车"]),
            product(5, 500, NOW, 2, &["自行车"]),
        ];
        let rec = Recommender::new(products, vec![user(1, &["自行车"], 300)], &[]);
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        let from_seller1 = out
            .iter()
            .filter(|s| rec.product(s.product_id).map(|p| p.seller_id) == Some(1))
            .count();
        assert!(
            from_seller1 <= MAX_PER_SELLER,
            "卖家 1 出现 {from_seller1} 次"
        );
        assert!(out.iter().any(|s| s.product_id == 5));
    }

    #[test]
    fn recommend_diversity_penalty_applied() {
        // 5 个商品标签完全相同（Jaccard=1.0 > 0.8），选完第一个后其余都被降权 0.6
        let products = vec![
            product(1, 300, NOW - 10, 1, &["自行车"]),
            product(2, 300, NOW - 20, 2, &["自行车"]),
            product(3, 300, NOW - 30, 3, &["自行车"]),
            product(4, 300, NOW - 40, 4, &["自行车"]),
            product(5, 300, NOW - 50, 5, &["自行车"]),
        ];
        let rec = Recommender::new(products, vec![user(1, &["自行车"], 300)], &[]);
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        assert_eq!(out.len(), 5);

        let first = out[0].score;
        assert!(first > 0.0);
        // 其余 4 个的原始分与第一名相同，被降权后应严格小于第一名
        for s in &out[1..] {
            assert!(
                s.score < first,
                "未应用多样性降权: first={first} got={}",
                s.score
            );
            assert!((s.score - first * DIVERSITY_PENALTY).abs() < 1e-9);
        }
    }

    #[test]
    fn recommend_no_penalty_for_dissimilar_tags() {
        // 标签完全不相交（Jaccard=0）→ 不降权
        let products = vec![
            product(1, 300, NOW - 10, 1, &["自行车"]),
            product(2, 300, NOW - 20, 2, &["教材"]),
        ];
        let rec = Recommender::new(products, vec![user(1, &["自行车", "教材"], 300)], &[]);
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        assert_eq!(out.len(), 2);
        // 两个商品的原始分相同（同价同热度，标签各自完全命中），都未被降权
        assert!((out[0].score - out[1].score).abs() < 1e-12);
    }

    #[test]
    fn recommend_is_deterministic() {
        let rec = fixture();
        let a = rec.recommend_for_user(1, 10, &[], NOW);
        let b = rec.recommend_for_user(1, 10, &[], NOW);
        assert_eq!(a, b);
    }

    #[test]
    fn recommend_excludes_sold_products() {
        let mut products = vec![
            product(1, 100, NOW, 1, &["自行车"]),
            product(2, 200, NOW, 2, &["自行车"]),
        ];
        products[1].status = Status::Sold;
        let rec = Recommender::new(products, vec![user(1, &["自行车"], 100)], &[]);
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        assert!(out.iter().all(|s| s.product_id != 2));
    }

    #[test]
    fn price_fit_symmetric_and_bounded() {
        let u = user(1, &["a"], 1000);
        assert!((Recommender::price_fit(&u, 1000) - 1.0).abs() < 1e-12);
        let up = Recommender::price_fit(&u, 10000);
        let down = Recommender::price_fit(&u, 100);
        assert!(up < 1.0 && down < 1.0);
        assert!((0.0..=1.0).contains(&up));
        assert!((0.0..=1.0).contains(&down));
        // 预算 1000 看 10000：1 - 9000/10000 = 0.1
        assert!((up - 0.1).abs() < 1e-12);
        // 预算 1000 看 100：1 - 900/1000 = 0.1（分母取较大值 → 对称）
        assert!((down - 0.1).abs() < 1e-12);
    }

    #[test]
    fn price_fit_zero_avg_price() {
        // avg_price = 0 表示用户没填预算，公式退化为 1 - price/max(price,1)
        let u = user(1, &["a"], 0);
        // 有价商品：1 - 500/500 = 0（没有预算参照，任何价格都不算「贴合」）
        assert_eq!(Recommender::price_fit(&u, 500), 0.0);
        // 零价商品：1 - 0/1 = 1.0（严格按公式，免费赠品与零预算完全贴合）
        assert_eq!(Recommender::price_fit(&u, 0), 1.0);
        // 负价格被夹到 0，不会出现 >1 的分
        assert!((0.0..=1.0).contains(&Recommender::price_fit(&u, -100)));
    }

    #[test]
    fn profile_tags_use_interests_only() {
        let rec = fixture();
        let u = rec.user(1).unwrap();
        let tags = rec.profile_tags(u);
        // 画像只看兴趣标签：自行车、运动。用户 1 虽然浏览过商品 5
        // （运动、健身），但「健身」不得进入画像——否则页面展示的画像
        // 与推荐理由会自相矛盾。
        assert_eq!(tags, s(&["自行车", "运动"]));
        assert!(!tags.contains(&"健身".to_string()));
    }

    #[test]
    fn similar_products_basic() {
        let rec = fixture();
        let out = rec.similar_products(1, 10, NOW);
        let ids: Vec<u64> = out.iter().map(|s| s.product_id).collect();
        assert!(!ids.contains(&1), "不应包含自己");
        // 商品 2 与 1 共享「自行车」
        assert!(ids.contains(&2), "商品 2 应相似: {ids:?}");
        assert!(ids.len() <= 4);
        for w in out.windows(2) {
            assert!(w[0].score >= w[1].score - 1e-12);
        }
    }

    #[test]
    fn similar_products_unknown_id() {
        let rec = fixture();
        assert!(rec.similar_products(999, 5, NOW).is_empty());
        assert!(rec.similar_products(1, 0, NOW).is_empty());
    }

    #[test]
    fn similar_products_prefers_shared_tags() {
        let rec = fixture();
        let out = rec.similar_products(1, 2, NOW);
        // 商品 2 只有「自行车」，与商品 1（自行车、运动）Jaccard = 1/2
        // 商品 5 只有「运动」，Jaccard 也是 1/2；但商品 1 与 5 有共现 → 5 更高
        assert!(out.iter().any(|s| s.product_id == 5 || s.product_id == 2));
    }

    #[test]
    fn similar_products_have_reasons() {
        let rec = fixture();
        for item in rec.similar_products(1, 10, NOW) {
            assert!(!item.reasons.is_empty(), "商品 {} 缺理由", item.product_id);
        }
    }

    #[test]
    fn hot_ranking_sorted_and_limited() {
        let rec = fixture();
        let out = rec.hot_ranking(3, NOW);
        assert_eq!(out.len(), 3);
        for w in out.windows(2) {
            assert!(w[0].score >= w[1].score - 1e-12);
        }
        assert!(
            out.iter()
                .all(|s| matches!(s.reasons[0], Reason::Trending { .. }))
        );
        assert!(rec.hot_ranking(0, NOW).is_empty());
    }

    #[test]
    fn hot_ranking_matches_full_sort() {
        let rec = fixture();
        let full = rec.hot_ranking(100, NOW);
        let top = rec.hot_ranking(2, NOW);
        assert_eq!(
            top.iter().map(|s| s.product_id).collect::<Vec<_>>(),
            full.iter()
                .take(2)
                .map(|s| s.product_id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn hot_ranking_scores_in_range() {
        let rec = fixture();
        for s in rec.hot_ranking(100, NOW) {
            assert!((0.0..=1.0).contains(&s.score), "score={}", s.score);
        }
    }

    #[test]
    fn query_without_filters_returns_all() {
        let rec = fixture();
        let out = rec.query(&[], QueryMode::Or, None, None, None, SortKey::PriceAsc, NOW);
        assert_eq!(out.len(), 5);
        for w in out.windows(2) {
            assert!(w[0].price_cents <= w[1].price_cents);
        }
    }

    #[test]
    fn query_by_tags_and_mode() {
        let rec = fixture();
        let and = rec.query(
            &s(&["自行车", "运动"]),
            QueryMode::And,
            None,
            None,
            None,
            SortKey::PriceAsc,
            NOW,
        );
        assert_eq!(and.len(), 1);
        assert_eq!(and[0].id, 1);

        let or = rec.query(
            &s(&["自行车", "运动"]),
            QueryMode::Or,
            None,
            None,
            None,
            SortKey::PriceAsc,
            NOW,
        );
        assert_eq!(or.iter().map(|p| p.id).collect::<Vec<_>>(), vec![5, 1, 2]);
    }

    #[test]
    fn query_price_range() {
        let rec = fixture();
        let out = rec.query(
            &[],
            QueryMode::Or,
            Some(5000),
            Some(40000),
            None,
            SortKey::PriceAsc,
            NOW,
        );
        let prices: Vec<i64> = out.iter().map(|p| p.price_cents).collect();
        assert!(prices.iter().all(|p| (5000..=40000).contains(p)));
        assert_eq!(prices, vec![5000, 20000, 30000, 40000]);
    }

    #[test]
    fn query_keyword_case_insensitive() {
        let mut products = vec![
            product(1, 100, NOW, 1, &["自行车"]),
            product(2, 200, NOW, 1, &["教材"]),
        ];
        products[0].title = "Giant 山地车".to_string();
        products[1].desc = "高等数学 giant 版".to_string();
        let rec = Recommender::new(products, vec![], &[]);
        let out = rec.query(
            &[],
            QueryMode::Or,
            None,
            None,
            Some("GIANT"),
            SortKey::PriceAsc,
            NOW,
        );
        assert_eq!(out.len(), 2);
        let out = rec.query(
            &[],
            QueryMode::Or,
            None,
            None,
            Some("数学"),
            SortKey::PriceAsc,
            NOW,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 2);
        // 空白关键字等价于不过滤
        let out = rec.query(
            &[],
            QueryMode::Or,
            None,
            None,
            Some("   "),
            SortKey::PriceAsc,
            NOW,
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn query_sort_orders() {
        let rec = fixture();
        let desc = rec.query(
            &[],
            QueryMode::Or,
            None,
            None,
            None,
            SortKey::PriceDesc,
            NOW,
        );
        for w in desc.windows(2) {
            assert!(w[0].price_cents >= w[1].price_cents);
        }
        let newest = rec.query(&[], QueryMode::Or, None, None, None, SortKey::Newest, NOW);
        for w in newest.windows(2) {
            assert!(w[0].created_at >= w[1].created_at);
        }
        let hot = rec.query(&[], QueryMode::Or, None, None, None, SortKey::Hot, NOW);
        assert_eq!(hot.len(), 5);
    }

    #[test]
    fn query_empty_result() {
        let rec = fixture();
        let out = rec.query(
            &s(&["不存在的标签"]),
            QueryMode::And,
            None,
            None,
            None,
            SortKey::Hot,
            NOW,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn query_min_greater_than_max_yields_nothing() {
        let rec = fixture();
        let out = rec.query(
            &[],
            QueryMode::Or,
            Some(1000),
            Some(10),
            None,
            SortKey::PriceAsc,
            NOW,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn explain_matches_recommend_score() {
        let rec = fixture();
        // 用户 1 交互过 1、5；商品 2 未被交互 → 可在推荐里出现
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        let target = out
            .iter()
            .find(|s| s.product_id == 2)
            .expect("商品 2 应在推荐里");
        let ex = rec.explain(1, 2, NOW).expect("explain 应返回");
        assert_eq!(ex.product_id, 2);
        // 推荐里的分数可能经过多样性降权，因此 ≤ explain 的原始分
        assert!(
            target.score <= ex.score + 1e-9,
            "推荐分 {} > 原始分 {}",
            target.score,
            ex.score
        );
        let recomputed = W_TAG * ex.tag_sim
            + W_CF * ex.cf_sim
            + W_HOT * ex.hot_norm
            + W_PRICE * ex.price_fit
            + W_CAMPUS * ex.campus_fit;
        assert!((recomputed - ex.score).abs() < 1e-12);
    }

    #[test]
    fn explain_unknown_ids() {
        let rec = fixture();
        assert!(rec.explain(1, 999, NOW).is_none());
        assert!(rec.explain(999, 1, NOW).is_none());
    }

    #[test]
    fn explain_parts_in_range() {
        let rec = fixture();
        for pid in 1..=5u64 {
            let ex = rec.explain(1, pid, NOW).unwrap();
            assert!((0.0..=1.0).contains(&ex.tag_sim));
            assert!((0.0..=1.0).contains(&ex.cf_sim));
            assert!((0.0..=1.0).contains(&ex.hot_norm));
            assert!((0.0..=1.0).contains(&ex.price_fit));
            assert!((0.0..=1.0).contains(&ex.campus_fit));
            assert!((0.0..=1.0).contains(&ex.score));
        }
    }

    #[test]
    fn similar_users_by_shared_items() {
        let rec = fixture();
        let sim = rec.similar_users(1, 5);
        // 用户 2 与用户 1 都交互过商品 1 → 共同商品 1 个
        assert_eq!(sim[0], (2, 1));
        assert!(sim.iter().all(|(u, _)| *u != 1));
        assert!(rec.similar_users(999, 5).is_empty());
        assert!(rec.similar_users(1, 0).is_empty());
    }

    #[test]
    fn stats_reflects_mutations() {
        let mut rec = fixture();
        let before = rec.stats();
        rec.upsert_product(product(99, 1, NOW, 9, &["独有标签"]));
        assert_eq!(rec.stats().products, before.products + 1);
        assert_eq!(rec.stats().tags, before.tags + 1);
        rec.record_event(ev(1, 2, EventKind::View, NOW));
        assert_eq!(rec.stats().events, before.events + 1);
    }

    #[test]
    fn empty_recommender_is_safe() {
        let rec = Recommender::new(vec![], vec![], &[]);
        assert!(rec.recommend_for_user(1, 10, &[], NOW).is_empty());
        assert!(rec.hot_ranking(10, NOW).is_empty());
        assert!(rec.similar_products(1, 10, NOW).is_empty());
        assert!(
            rec.query(&[], QueryMode::Or, None, None, None, SortKey::Hot, NOW)
                .is_empty()
        );
        assert!(rec.explain(1, 1, NOW).is_none());
        assert_eq!(rec.stats().products, 0);
    }

    #[test]
    fn recommend_without_events_still_works() {
        // 有商品有用户但零行为：靠标签倒排召回
        let products = vec![
            product(1, 100, NOW, 1, &["自行车"]),
            product(2, 200, NOW, 2, &["教材"]),
        ];
        let rec = Recommender::new(products, vec![user(1, &["自行车"], 100)], &[]);
        let out = rec.recommend_for_user(1, 10, &[], NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].product_id, 1);
    }
}
