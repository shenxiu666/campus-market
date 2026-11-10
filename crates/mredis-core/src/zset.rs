//! 跳表有序集合，对标 `redis/src/t_zset.c`。
//!
//! 层级按 `1/4` 几何分布随机生成（最大 32 层）；同分数时按成员字典序排序，
//! 与 Redis 的 `ZRANGE` 语义一致。
//!
//! # 为什么用跳表而不是平衡树
//!
//! `ZRANGE start stop` 需要按下标定位（`rank`）。平衡树要做到这一点，得给每个
//! 节点维护子树大小并在每次旋转后修正；跳表只要在每层的前进指针上记一个
//! `span`（这一跳跨过多少个节点），累加 `span` 就得到排名。实现与维护都简单得多，
//! 期望复杂度同样是 O(log n)。
//!
//! # 为什么用 arena 而不是 `Box` 指针链
//!
//! 跳表插入时要把「每一层的前驱节点」都改指向新节点，用 `Box` 实现就要同时持有
//! 多个 `&mut`，安全 Rust 表达不了，只能退化成 `unsafe` 裸指针。这里把节点统一
//! 放进 `Vec<Node>`，前进指针改存**下标**，所有链接操作都是普通索引赋值：
//! 零 `unsafe`，同时节点连续存放，遍历时缓存命中率也更高。

use std::collections::HashMap;

/// 最大层数。
const MAX_LEVEL: usize = 32;

/// 升层概率的分母：每层有 1/4 概率继续往上。
const LEVEL_P: u64 = 4;

/// 空指针哨兵。
const NIL: usize = usize::MAX;

/// 跳表节点（存放在 [`ZSet::nodes`] 里，用下标互相链接）。
#[derive(Debug, Clone)]
struct Node {
    member: Vec<u8>,
    score: f64,
    /// `forward[i]` 是第 `i` 层的前进节点下标，`NIL` 表示该层到头了。
    forward: Vec<usize>,
    /// `span[i]` 是沿 `forward[i]` 跳一步**跨过**的节点数（含目标节点本身）。
    span: Vec<usize>,
}

impl Node {
    fn new(member: Vec<u8>, score: f64, level: usize) -> Node {
        Node {
            member,
            score,
            forward: vec![NIL; level],
            span: vec![0; level],
        }
    }
}

/// 有序集合。
///
/// 内部是「跳表 + 成员哈希索引」的双结构：跳表负责按分数有序遍历与排名，
/// 哈希表负责 O(1) 判断成员是否存在以及取分数——这正是 Redis `zset`
/// 同时使用 `skiplist` 与 `dict` 的原因。
#[derive(Debug)]
pub struct ZSet {
    /// 节点 arena；下标 0 恒为哨兵头节点。
    nodes: Vec<Node>,
    /// 回收槽位（删除后留下的空洞），避免 arena 无限增长。
    free: Vec<usize>,
    /// 当前最大层数（至少 1）。
    level: usize,
    /// 元素个数（不含头节点）。
    length: usize,
    /// 成员 → 分数，用于 O(1) 的 `score()` / 存在性判断。
    scores: HashMap<Vec<u8>, f64>,
    /// 确定性随机源状态（xorshift64），让测试可复现。
    seed: u64,
}

/// 头节点固定占用下标 0。
const HEADER: usize = 0;

impl Default for ZSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ZSet {
    /// 新建空有序集合。
    pub fn new() -> Self {
        let mut nodes = Vec::with_capacity(16);
        nodes.push(Node::new(Vec::new(), f64::NEG_INFINITY, MAX_LEVEL));
        ZSet {
            nodes,
            free: Vec::new(),
            level: 1,
            length: 0,
            scores: HashMap::new(),
            // 固定非零种子：同一操作序列的结果可复现，测试才能断言层数等细节。
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// 元素个数。
    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// 插入或更新；返回 `true` 表示新增成员。
    pub fn insert(&mut self, member: Vec<u8>, score: f64) -> bool {
        if let Some(&old) = self.scores.get(&member) {
            // 分数没变就不用动跳表，省一次删除 + 插入。
            if old.to_bits() == score.to_bits() {
                return false;
            }
            let idx = self.find_index(&member, old);
            self.unlink(idx);
            self.scores.insert(member.clone(), score);
            self.link(member, score);
            return false;
        }
        self.scores.insert(member.clone(), score);
        self.link(member, score);
        true
    }

    /// 删除成员；返回是否真的删掉了。
    pub fn remove(&mut self, member: &[u8]) -> bool {
        let Some(score) = self.scores.remove(member) else {
            return false;
        };
        let idx = self.find_index(member, score);
        self.unlink(idx);
        true
    }

    /// 成员分数。
    pub fn score(&self, member: &[u8]) -> Option<f64> {
        self.scores.get(member).copied()
    }

    /// 升序排名（0 起）；成员不存在返回 `None`。
    ///
    /// 沿每层前进时把 `span` 累加：走到「最后一个仍排在目标之前的节点」时，
    /// 累加和就是目标的排名。
    pub fn rank(&self, member: &[u8]) -> Option<usize> {
        let target = *self.scores.get(member)?;
        let mut rank = 0usize;
        let mut cur = HEADER;
        for i in (0..self.level).rev() {
            while self.nodes[cur].forward[i] != NIL {
                let next = self.nodes[cur].forward[i];
                let node = &self.nodes[next];
                if node.score < target || (node.score == target && node.member.as_slice() < member)
                {
                    rank += self.nodes[cur].span[i];
                    cur = next;
                } else {
                    break;
                }
            }
        }
        Some(rank)
    }

    /// 升序区间，`start`/`stop` 支持负下标（-1 表示最后一个）。
    pub fn range(&self, start: i64, stop: i64) -> Vec<(Vec<u8>, f64)> {
        let Some((start, stop)) = normalize_range(start, stop, self.length) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(stop - start + 1);
        let mut cur = self.node_at(start);
        while cur != NIL && out.len() <= stop - start {
            let node = &self.nodes[cur];
            out.push((node.member.clone(), node.score));
            cur = node.forward[0];
        }
        out
    }

    /// 降序区间，下标从大到小计数（`0` 是分数最高的那个）。
    ///
    /// 跳表只往前单向链接，反向逐节点走要么多存一套 `backward` 指针，要么退化成
    /// 全量收集再反转。这里用第三种办法：把反向下标换算成正向下标，
    /// 一次 O(log n) 定位再顺着第 0 层走 `k` 步，最后整体反转。
    pub fn range_rev(&self, start: i64, stop: i64) -> Vec<(Vec<u8>, f64)> {
        let Some((start, stop)) = normalize_range(start, stop, self.length) else {
            return Vec::new();
        };
        // 反向第 i 个 == 正向第 length-1-i 个。
        let lo = self.length - 1 - stop;
        let hi = self.length - 1 - start;
        let mut out = Vec::with_capacity(hi - lo + 1);
        let mut cur = self.node_at(lo);
        while cur != NIL && out.len() <= hi - lo {
            let node = &self.nodes[cur];
            out.push((node.member.clone(), node.score));
            cur = node.forward[0];
        }
        out.reverse();
        out
    }

    /// 当前最大层数（用于 `INFO` 与演示）。
    pub fn level(&self) -> usize {
        self.level
    }

    /// 按分数升序迭代全部成员。
    pub fn iter(&self) -> Box<dyn Iterator<Item = (&Vec<u8>, f64)> + '_> {
        Box::new(Iter {
            z: self,
            cur: self.nodes[HEADER].forward[0],
        })
    }

    /// 分数最高的前 `k` 个成员（降序）。
    pub fn top(&self, k: usize) -> Vec<(Vec<u8>, f64)> {
        if k == 0 || self.length == 0 {
            return Vec::new();
        }
        let k = k.min(self.length);
        self.range_rev(0, k as i64 - 1)
    }

    /// 分数闭区间内的成员（升序）。
    pub fn range_by_score(&self, min: f64, max: f64) -> Vec<(Vec<u8>, f64)> {
        let mut out = Vec::new();
        let mut cur = self.nodes[HEADER].forward[0];
        while cur != NIL {
            let node = &self.nodes[cur];
            if node.score > max {
                break;
            }
            if node.score >= min {
                out.push((node.member.clone(), node.score));
            }
            cur = node.forward[0];
        }
        out
    }

    /// 取第 `idx` 个节点下标（0 起，不含头节点）。
    ///
    /// `span` 的语义是「这一跳前进多少个位置」：头节点到第 0 个节点是 1。
    /// 因此 `rank` 累加出来后等于 `index(cur) + 1`，要落在下标 `idx` 上
    /// 就得让累加和达到 `idx + 1`。
    fn node_at(&self, idx: usize) -> usize {
        let mut rank = 0usize;
        let mut cur = HEADER;
        for i in (0..self.level).rev() {
            while self.nodes[cur].forward[i] != NIL && rank + self.nodes[cur].span[i] <= idx + 1 {
                rank += self.nodes[cur].span[i];
                cur = self.nodes[cur].forward[i];
            }
        }
        cur
    }

    /// 找到成员对应的节点下标；找不到返回 `NIL`。
    fn find_index(&self, member: &[u8], score: f64) -> usize {
        let mut cur = HEADER;
        for i in (0..self.level).rev() {
            while self.nodes[cur].forward[i] != NIL {
                let next = self.nodes[cur].forward[i];
                let node = &self.nodes[next];
                if node.score < score || (node.score == score && node.member.as_slice() < member) {
                    cur = next;
                } else {
                    break;
                }
            }
        }
        let cand = self.nodes[cur].forward[0];
        if cand != NIL && self.nodes[cand].member.as_slice() == member {
            cand
        } else {
            NIL
        }
    }

    /// 生成一个新节点的层数：每层 1/4 概率继续升。
    fn random_level(&mut self) -> usize {
        let mut level = 1;
        while level < MAX_LEVEL && self.next_rand() % LEVEL_P == 0 {
            level += 1;
        }
        level
    }

    /// xorshift64 随机数（确定性，测试可复现）。
    fn next_rand(&mut self) -> u64 {
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed = x;
        x
    }

    /// 分配一个节点槽位（优先复用删除留下的空洞）。
    fn alloc(&mut self, member: Vec<u8>, score: f64, level: usize) -> usize {
        let node = Node::new(member, score, level);
        match self.free.pop() {
            Some(idx) => {
                self.nodes[idx] = node;
                idx
            }
            None => {
                self.nodes.push(node);
                self.nodes.len() - 1
            }
        }
    }

    /// 把成员插入跳表（成员此时必定不在表中）。
    ///
    /// 严格照 `zslInsert` 的顺序做三件事：
    /// 1. 若新节点比当前最大层还高，先把头节点多出来的那几层补成「直达表尾」；
    /// 2. 自上而下记录每层的 `update[i]`（该层最后一个仍排在新节点之前的节点）
    ///    与 `rank[i]`（从表头走到 `update[i]` 时跨过的节点数，1 起算）；
    /// 3. 自下而上接线，并按 `span` 的不变量修正跨度。
    fn link(&mut self, member: Vec<u8>, score: f64) {
        let new_level = self.random_level();
        let mut update = [HEADER; MAX_LEVEL];
        let mut rank = [0usize; MAX_LEVEL];

        // 第 1 步：头节点新增的高层直接指向表尾，跨度就是整表长度。
        if new_level > self.level {
            for i in self.level..new_level {
                self.nodes[HEADER].forward[i] = NIL;
                self.nodes[HEADER].span[i] = self.length;
                update[i] = HEADER;
                rank[i] = 0;
            }
            self.level = new_level;
        }

        // 第 2 步：定位插入点。
        let mut cur = HEADER;
        for i in (0..self.level).rev() {
            rank[i] = if i == self.level - 1 { 0 } else { rank[i + 1] };
            while self.nodes[cur].forward[i] != NIL {
                let next = self.nodes[cur].forward[i];
                let node = &self.nodes[next];
                if node.score < score || (node.score == score && node.member.as_slice() < &member) {
                    rank[i] += self.nodes[cur].span[i];
                    cur = next;
                } else {
                    break;
                }
            }
            update[i] = cur;
        }

        // 第 3 步：接线。`crossed` 是新节点在 `update[i]` 之后跨过的距离。
        let idx = self.alloc(member, score, new_level);
        for i in 0..new_level {
            let prev = update[i];
            self.nodes[idx].forward[i] = self.nodes[prev].forward[i];
            self.nodes[prev].forward[i] = idx;
            let crossed = rank[0] - rank[i];
            self.nodes[idx].span[i] = self.nodes[prev].span[i] - crossed;
            self.nodes[prev].span[i] = crossed + 1;
        }
        // 新节点没有覆盖到的高层：它们的 `update[i]` 之后多了一个节点，跨度 +1。
        for (i, &prev) in update.iter().enumerate().take(self.level).skip(new_level) {
            self.nodes[prev].span[i] += 1;
        }
        self.length += 1;
    }

    /// 从跳表中摘掉 `target` 节点（必定在表中）。
    fn unlink(&mut self, target: usize) {
        if target == NIL {
            return;
        }
        let tscore = self.nodes[target].score;
        let tmember = self.nodes[target].member.clone();
        let mut update = [HEADER; MAX_LEVEL];
        let mut cur = HEADER;
        for i in (0..self.level).rev() {
            while self.nodes[cur].forward[i] != NIL {
                let next = self.nodes[cur].forward[i];
                if next == target {
                    break;
                }
                let node = &self.nodes[next];
                if node.score < tscore || (node.score == tscore && node.member < tmember) {
                    cur = next;
                } else {
                    break;
                }
            }
            update[i] = cur;
        }

        for (i, &prev) in update.iter().enumerate().take(self.level) {
            if self.nodes[prev].forward[i] == target {
                // 前驱直接跳过目标：跨度 = 原来的 1 步换成目标自己跨过的那一段。
                // 注意 target 若恰好是这一层的最后一个节点，`span[i]` 就是 0，
                // 所以必须先 `-1` 再 `+target.span[i]`，否则无符号减法会下溢
                // （Redis 靠 C 的无符号回绕绕过，Rust 调试模式会直接 panic）。
                self.nodes[prev].span[i] =
                    (self.nodes[prev].span[i] - 1) + self.nodes[target].span[i];
                self.nodes[prev].forward[i] = self.nodes[target].forward[i];
            } else if self.nodes[prev].forward[i] != NIL {
                // 该层没有目标，说明目标在这一层之前，跨度减一。
                self.nodes[prev].span[i] -= 1;
            }
        }

        // 回收槽位；最高层空掉时收缩。
        self.nodes[target].forward.clear();
        self.nodes[target].span.clear();
        self.nodes[target].member.clear();
        self.free.push(target);
        while self.level > 1 && self.nodes[HEADER].forward[self.level - 1] == NIL {
            self.level -= 1;
        }
        self.length -= 1;
    }
}

/// 把负下标换算成正下标；区间为空时返回 `None`。
fn normalize_range(start: i64, stop: i64, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let len_i = len as i64;
    let mut s = if start < 0 { start + len_i } else { start };
    let mut e = if stop < 0 { stop + len_i } else { stop };
    if s < 0 {
        s = 0;
    }
    if e >= len_i {
        e = len_i - 1;
    }
    if s > e || s >= len_i || e < 0 {
        return None;
    }
    Some((s as usize, e as usize))
}

/// 第 0 层遍历器。
struct Iter<'a> {
    z: &'a ZSet,
    cur: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a Vec<u8>, f64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur == NIL {
            return None;
        }
        let node = &self.z.nodes[self.cur];
        self.cur = node.forward[0];
        Some((&node.member, node.score))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn collect(z: &ZSet) -> Vec<(Vec<u8>, f64)> {
        z.iter().map(|(m, s)| (m.clone(), s)).collect()
    }

    #[test]
    fn insert_score_rank_remove() {
        let mut z = ZSet::new();
        assert!(z.insert(b"a".to_vec(), 1.0));
        assert!(z.insert(b"b".to_vec(), 2.0));
        assert!(z.insert(b"c".to_vec(), 3.0));
        assert_eq!(z.len(), 3);
        assert_eq!(z.score(b"b"), Some(2.0));
        assert_eq!(z.rank(b"a"), Some(0));
        assert_eq!(z.rank(b"b"), Some(1));
        assert_eq!(z.rank(b"c"), Some(2));
        assert_eq!(z.score(b"zzz"), None);
        assert_eq!(z.rank(b"zzz"), None);
        // 更新已有成员分数：不是新增，且排名要跟着重排。
        assert!(!z.insert(b"b".to_vec(), 4.0));
        assert_eq!(z.len(), 3);
        assert_eq!(z.score(b"b"), Some(4.0));
        assert_eq!(z.rank(b"b"), Some(2), "b 涨到 4.0 后应排到 c 之后");
        assert!(z.remove(b"b"));
        assert!(!z.remove(b"b"));
        assert_eq!(z.len(), 2);
    }

    #[test]
    fn same_score_orders_by_member_lexicographically() {
        let mut z = ZSet::new();
        for m in ["d", "a", "c", "b"] {
            z.insert(m.as_bytes().to_vec(), 5.0);
        }
        let got: Vec<String> = collect(&z)
            .into_iter()
            .map(|(m, _)| String::from_utf8_lossy(&m).into_owned())
            .collect();
        assert_eq!(got, vec!["a", "b", "c", "d"]);
        assert_eq!(z.rank(b"c"), Some(2));
    }

    #[test]
    fn range_and_range_rev_with_negative_index() {
        let mut z = ZSet::new();
        for i in 0..10 {
            z.insert(format!("m{i}").into_bytes(), i as f64);
        }
        assert_eq!(z.range(0, 2).len(), 3);
        assert_eq!(z.range(0, 2)[0].0, b"m0".to_vec());
        assert_eq!(z.range(-1, -1)[0].0, b"m9".to_vec());
        assert_eq!(z.range(0, -1).len(), 10);
        assert!(z.range(5, 2).is_empty(), "start > stop 应为空");
        assert!(z.range(100, 200).is_empty(), "越界应为空");
        let rev = z.range_rev(0, 2);
        assert_eq!(rev[0].0, b"m9".to_vec());
        assert_eq!(rev[2].0, b"m7".to_vec());
        assert_eq!(z.range_rev(0, -1).len(), 10);
    }

    #[test]
    fn top_k_returns_descending() {
        let mut z = ZSet::new();
        for i in 0..20 {
            z.insert(format!("m{i:02}").into_bytes(), i as f64);
        }
        let top = z.top(3);
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].1, 19.0);
        assert_eq!(top[1].1, 18.0);
        assert_eq!(top[2].1, 17.0);
        assert!(z.top(0).is_empty());
        assert_eq!(z.top(1000).len(), 20, "k 超过 size 时返回全部");
        let empty = ZSet::new();
        assert!(empty.top(5).is_empty());
    }

    #[test]
    fn range_by_score_is_inclusive() {
        let mut z = ZSet::new();
        for i in 0..10 {
            z.insert(format!("m{i}").into_bytes(), i as f64);
        }
        let got = z.range_by_score(3.0, 5.0);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1, 3.0);
        assert_eq!(got[2].1, 5.0);
    }

    #[test]
    fn level_is_bounded() {
        let mut z = ZSet::new();
        for i in 0..5000 {
            z.insert(format!("k{i}").into_bytes(), i as f64);
        }
        assert!(z.level() >= 1);
        assert!(z.level() <= MAX_LEVEL, "层数 {} 超过上限", z.level());
    }

    #[test]
    fn single_element_boundaries() {
        let mut z = ZSet::new();
        assert!(z.is_empty());
        assert!(z.range(0, -1).is_empty());
        z.insert(b"only".to_vec(), 42.0);
        assert_eq!(z.range(0, -1).len(), 1);
        assert_eq!(z.range(-1, -1)[0].0, b"only".to_vec());
        assert_eq!(z.top(1).len(), 1);
        assert_eq!(z.rank(b"only"), Some(0));
    }

    #[test]
    fn matches_btreemap_on_1000_random_ops() {
        let mut rng = 0xdead_beef_1234_5678u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let mut z = ZSet::new();
        let mut model: BTreeMap<Vec<u8>, f64> = BTreeMap::new();

        for step in 0..1000 {
            let member = format!("m{:03}", next() % 200).into_bytes();
            match next() % 4 {
                // 插入/更新
                0 | 1 => {
                    // 分数用整数，避免 f64 相等比较的精度噪声。
                    let score = (next() % 100) as f64;
                    let added = z.insert(member.clone(), score);
                    assert_eq!(
                        added,
                        !model.contains_key(&member),
                        "step {step} 返回值不符"
                    );
                    model.insert(member, score);
                }
                // 删除
                2 => {
                    assert_eq!(z.remove(&member), model.remove(&member).is_some());
                }
                // 查询
                _ => {
                    assert_eq!(z.score(&member), model.get(&member).copied());
                }
            }
            assert_eq!(z.len(), model.len(), "step {step} 长度不符");
        }

        // 全量顺序比对（分数升序 + 同分成员字典序）。
        let mut expected: Vec<(Vec<u8>, f64)> =
            model.iter().map(|(k, v)| (k.clone(), *v)).collect();
        expected.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        assert_eq!(collect(&z), expected);

        // 排名逐个比对。
        for (i, (member, _)) in expected.iter().enumerate() {
            assert_eq!(z.rank(member), Some(i), "排名不符：{member:?}");
        }
    }

    #[test]
    fn remove_all_leaves_empty_structure() {
        let mut z = ZSet::new();
        let members: Vec<Vec<u8>> = (0..500).map(|i| format!("r{i}").into_bytes()).collect();
        for (i, m) in members.iter().enumerate() {
            z.insert(m.clone(), i as f64);
        }
        for m in &members {
            assert!(z.remove(m));
        }
        assert!(z.is_empty());
        assert_eq!(z.level(), 1, "全删后层数应回落到 1");
        assert!(z.iter().next().is_none());
        assert_eq!(z.range(0, -1), Vec::new());
    }

    #[test]
    fn reinsert_after_removal_reuses_slots() {
        let mut z = ZSet::new();
        for i in 0..100 {
            z.insert(format!("x{i}").into_bytes(), i as f64);
        }
        let arena_after_insert = z.nodes.len();
        for i in 0..100 {
            z.remove(format!("x{i}").as_bytes());
        }
        for i in 0..100 {
            z.insert(format!("y{i}").into_bytes(), i as f64);
        }
        assert_eq!(
            z.nodes.len(),
            arena_after_insert,
            "arena 应复用删除后的空洞，而不是持续增长"
        );
        assert_eq!(z.len(), 100);
        assert_eq!(z.rank(b"y50"), Some(50));
    }
}
