//! 自研哈希表：链地址法 + FNV-1a + 增量 rehash。
//!
//! 设计对标 `redis/src/dict.c`，但用安全 Rust 实现：
//! * 两张桶数组 `ht[0]` / `ht[1]`，rehash 期间新键写 `ht[1]`，查找两边都看；
//! * 每次写操作迁移 1 个桶（`rehash_step`），把 O(n) 的扩容摊还到 O(1)；
//! * 负载因子 > 1.0 扩容到 2 倍，< 0.1 且桶数 > 16 时缩容到一半。
//!
//! # 为什么不用 `std::collections::HashMap`
//!
//! 本题的「哈希」亮点就是这张表本身：桶数组、链地址冲突解决、增量 rehash、
//! SCAN 游标语义都必须自己实现，才能讲清楚 O(1) 摊还扩容是怎么做到的。
//! 用标准库会把这个过程完全藏起来。

/// 键值对。
#[derive(Debug, Clone)]
pub struct Entry<V> {
    pub key: Vec<u8>,
    pub value: V,
}

/// 桶数组的初始大小，与 Redis 的 `DICT_HT_INITIAL_SIZE` 一致。
const INITIAL_BUCKETS: usize = 4;

/// 缩容阈值：负载因子低于 `1 / SHRINK_RATIO` 且桶数大于 `SHRINK_MIN_BUCKETS` 时缩容。
const SHRINK_RATIO: usize = 10;
const SHRINK_MIN_BUCKETS: usize = 16;

/// 链地址法的一个节点。
///
/// 节点里缓存了 64 位哈希值：查找时先比哈希再比键，绝大多数不匹配的节点
/// 只需要一次 `u64` 比较，省掉 `memcmp`。
#[derive(Debug)]
struct Node<V> {
    hash: u64,
    key: Vec<u8>,
    value: V,
    next: Option<Box<Node<V>>>,
}

/// 一张桶数组。
#[derive(Debug)]
struct Table<V> {
    buckets: Vec<Option<Box<Node<V>>>>,
}

impl<V> Table<V> {
    fn empty() -> Self {
        Table {
            buckets: Vec::new(),
        }
    }

    /// 创建 `size` 个空桶；`size` 必须是 2 的幂，用 `& (size - 1)` 代替取模。
    fn with_size(size: usize) -> Self {
        debug_assert!(size.is_power_of_two());
        Table {
            buckets: (0..size).map(|_| None).collect(),
        }
    }

    fn len(&self) -> usize {
        self.buckets.len()
    }

    fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// 桶下标掩码；空表返回 0（空表不会被索引，仅用于防御）。
    fn mask(&self) -> u64 {
        if self.buckets.is_empty() {
            0
        } else {
            (self.buckets.len() - 1) as u64
        }
    }
}

/// 哈希表。
#[derive(Debug)]
pub struct Dict<V> {
    /// `ht[0]` 平时是唯一在用的表；rehash 期间是「旧表」。
    ht: [Table<V>; 2],
    /// 已迁移到的桶下标；`-1` 表示没有在 rehash。
    rehash_idx: isize,
    /// 元素总数。
    used: usize,
}

impl<V> Default for Dict<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> Dict<V> {
    /// 新建空表，桶数取 `INITIAL_BUCKETS`（与 Redis 一致：避免空表要特判）。
    pub fn new() -> Self {
        Dict {
            ht: [Table::with_size(INITIAL_BUCKETS), Table::empty()],
            rehash_idx: -1,
            used: 0,
        }
    }

    /// 预分配至少容纳 `capacity` 个元素的桶数。
    pub fn with_capacity(capacity: usize) -> Self {
        Dict {
            ht: [Table::with_size(next_pow2(capacity.max(1))), Table::empty()],
            rehash_idx: -1,
            used: 0,
        }
    }

    /// 元素个数。
    pub fn len(&self) -> usize {
        self.used
    }

    pub fn is_empty(&self) -> bool {
        self.used == 0
    }

    /// 查找（只读）。
    pub fn get(&self, key: &[u8]) -> Option<&V> {
        let hash = fnv1a(key);
        find_in_table(&self.ht[0], hash, key).or_else(|| {
            if self.is_rehashing() {
                find_in_table(&self.ht[1], hash, key)
            } else {
                None
            }
        })
    }

    /// 查找（可变）。
    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        let hash = fnv1a(key);
        let rehashing = self.rehash_idx >= 0;
        // 先把两张表拆成两个独立的可变借用，否则「先返回 ht[0] 的结果、
        // 再借用 ht[1]」会被借用检查器拒绝（返回值的生命周期覆盖整个函数）。
        let [t0, t1] = &mut self.ht;
        if let Some(v) = find_in_table_mut(t0, hash, key) {
            return Some(v);
        }
        if rehashing {
            return find_in_table_mut(t1, hash, key);
        }
        None
    }

    /// 键是否存在。
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// 插入；键已存在时返回旧值。
    pub fn insert(&mut self, key: Vec<u8>, value: V) -> Option<V> {
        let hash = fnv1a(&key);
        // 已存在的键：原地替换，不改变桶位置。
        if let Some(slot) = self.find_slot_mut(hash, &key) {
            return Some(std::mem::replace(slot, value));
        }
        // 负载因子超过 1.0 就先扩容。注意扩容只是「启动 rehash」，
        // 真正的搬运摊还到后续每次写操作里。
        if !self.is_rehashing() && self.used + 1 > self.ht[0].len() {
            let need = next_pow2((self.used + 1) * 2).max(self.ht[0].len() * 2);
            self.start_rehash(need);
        }
        // rehash 期间新键一律写 ht[1]，这样 ht[0] 只会单调减少，一定能搬完。
        let t = usize::from(self.is_rehashing());
        let idx = (hash & self.ht[t].mask()) as usize;
        let node = Box::new(Node {
            hash,
            key,
            value,
            next: self.ht[t].buckets[idx].take(),
        });
        self.ht[t].buckets[idx] = Some(node);
        self.used += 1;
        self.rehash_step(1);
        None
    }

    /// 删除，返回被删除的值。
    pub fn remove(&mut self, key: &[u8]) -> Option<V> {
        let hash = fnv1a(key);
        let mut removed = remove_from_table(&mut self.ht[0], hash, key);
        if removed.is_none() && self.is_rehashing() {
            removed = remove_from_table(&mut self.ht[1], hash, key);
        }
        if removed.is_some() {
            self.used -= 1;
            self.maybe_shrink();
            self.rehash_step(1);
        }
        removed
    }

    /// 清空，并把表恢复成初始状态。
    pub fn clear(&mut self) {
        self.ht[0] = Table::with_size(INITIAL_BUCKETS);
        self.ht[1] = Table::empty();
        self.rehash_idx = -1;
        self.used = 0;
    }

    /// 遍历所有键值对（rehash 期间两张表都要走）。
    pub fn iter(&self) -> Box<dyn Iterator<Item = (&Vec<u8>, &V)> + '_> {
        Box::new(Iter {
            tables: [self.ht[0].buckets.as_slice(), self.ht[1].buckets.as_slice()],
            table: 0,
            bucket: 0,
            cur: None,
        })
    }

    /// 遍历所有键。
    pub fn keys(&self) -> Box<dyn Iterator<Item = &Vec<u8>> + '_> {
        Box::new(self.iter().map(|(k, _)| k))
    }

    /// 遍历所有值。
    pub fn values(&self) -> Box<dyn Iterator<Item = &V> + '_> {
        Box::new(self.iter().map(|(_, v)| v))
    }

    /// Redis 风格游标式遍历：返回 `(下一个游标, 本批键)`，游标 0 表示遍历结束。
    ///
    /// # 为什么不能简单地「从上次的桶号往后扫」
    ///
    /// 两次 `SCAN` 调用之间表可能扩容：原来 8 个桶变成 16 个，桶 `i` 里的元素
    /// 会被拆到 `i` 和 `i + 8`。如果按自然顺序 `0,1,2...` 扫，扩容后就会
    /// **重复**扫到已经扫过的桶。Redis 的解法是「反向二进制递增游标」：
    /// 把桶号按位反转后 +1，等价于从高位向低位进位。这样扩容时新增的桶
    /// 一定是「已扫描区域」的高位扩展，不会退回低位，因此：
    ///
    /// * 遍历期间一直存在的元素**至少**被返回一次；
    /// * 从头到尾都在的元素**恰好**返回一次（不漏不重）。
    ///
    /// 简化点：本项目在 rehash（而非扩容瞬间）期间采用 Redis 的双表展开扫描，
    /// 保证小表的一个桶与它在大表里的两个扩展桶在同一批里被扫到。
    pub fn scan(&self, cursor: u64, count: usize) -> (u64, Vec<Vec<u8>>) {
        let mut out = Vec::new();
        if self.used == 0 || self.ht[0].is_empty() {
            return (0, out);
        }
        let steps = count.max(1);
        let mut v = cursor;
        for _ in 0..steps {
            if !self.is_rehashing() {
                let m0 = self.ht[0].mask();
                collect_chain(&self.ht[0].buckets[(v & m0) as usize], &mut out);
                // 把掩码之外的高位全部置 1，反转后 +1 就只会动掩码内的位。
                v |= !m0;
                v = rev64(v).wrapping_add(1);
                v = rev64(v);
            } else {
                // 小表是 ht[0] 还是 ht[1] 取决于当前是扩容还是缩容。
                let (small, big) = if self.ht[0].len() <= self.ht[1].len() {
                    (&self.ht[0], &self.ht[1])
                } else {
                    (&self.ht[1], &self.ht[0])
                };
                let m0 = small.mask();
                let m1 = big.mask();
                collect_chain(&small.buckets[(v & m0) as usize], &mut out);
                // 小表的一个桶对应大表里的 2^k 个桶，必须一次性全部扫完，
                // 否则同一个元素会被「小表这次扫到、大表下次又扫到」而重复。
                loop {
                    collect_chain(&big.buckets[(v & m1) as usize], &mut out);
                    v |= !m1;
                    v = rev64(v).wrapping_add(1);
                    v = rev64(v);
                    if v & (m0 ^ m1) == 0 {
                        break;
                    }
                }
            }
            if v == 0 {
                break;
            }
        }
        (v, out)
    }

    /// 当前桶数量（rehash 期间为两张表之和）。
    pub fn bucket_count(&self) -> usize {
        self.ht[0].len() + self.ht[1].len()
    }

    /// 是否正在 rehash。
    pub fn is_rehashing(&self) -> bool {
        self.rehash_idx >= 0
    }

    /// 每个桶的链长，用于演示面板画分布图。
    ///
    /// 返回值长度恒等于 [`Dict::bucket_count`]：先 `ht[0]` 的桶，再 `ht[1]` 的桶。
    /// 不在 rehash 时 `ht[1]` 为空，因此就是 `ht[0]` 的分布。
    pub fn bucket_distribution(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.bucket_count());
        for table in &self.ht {
            for bucket in &table.buckets {
                let mut n = 0;
                let mut cur = bucket.as_deref();
                while let Some(node) = cur {
                    n += 1;
                    cur = node.next.as_deref();
                }
                out.push(n);
            }
        }
        out
    }

    /// 手动推进 rehash（每个写操作会自动调用一次）。
    ///
    /// `steps` 计的是「搬运的非空桶个数」：空桶只是跳过游标，不消耗步数，
    /// 这样一次调用的工作量是可预期的。
    pub fn rehash_step(&mut self, steps: usize) {
        if !self.is_rehashing() {
            return;
        }
        let mut done = 0;
        while done < steps {
            if self.rehash_idx as usize >= self.ht[0].len() {
                self.finish_rehash();
                return;
            }
            let i = self.rehash_idx as usize;
            let chain = self.ht[0].buckets[i].take();
            let Some(mut node) = chain else {
                // 空桶：只推进游标，不计步数。
                self.rehash_idx += 1;
                continue;
            };
            // 整条链一次性搬到 ht[1]（头插，链内顺序会反转，但哈希表不关心顺序）。
            let mask = self.ht[1].mask();
            loop {
                let idx = (node.hash & mask) as usize;
                let next = node.next.take();
                node.next = self.ht[1].buckets[idx].take();
                self.ht[1].buckets[idx] = Some(node);
                match next {
                    Some(n) => node = n,
                    None => break,
                }
            }
            self.rehash_idx += 1;
            done += 1;
        }
        if self.rehash_idx as usize >= self.ht[0].len() {
            self.finish_rehash();
        }
    }

    /// 预分配，保证至少容纳 `n` 个元素而不触发扩容。
    pub fn reserve(&mut self, n: usize) {
        if self.is_rehashing() {
            return;
        }
        let want = next_pow2(n.max(1));
        if self.ht[0].len() < want {
            self.start_rehash(want);
            // 立刻搬一部分，避免调用方紧接着插入时还落在旧表上。
            self.rehash_step(1);
        }
    }

    /// 启动一次 rehash：把 `ht[1]` 建成目标大小，游标归零。
    fn start_rehash(&mut self, new_size: usize) {
        debug_assert!(new_size.is_power_of_two());
        if self.is_rehashing() {
            return;
        }
        self.ht[1] = Table::with_size(new_size);
        self.rehash_idx = 0;
    }

    /// 搬迁结束：`ht[1]` 转正，`ht[0]` 释放。
    fn finish_rehash(&mut self) {
        self.ht[0] = std::mem::replace(&mut self.ht[1], Table::empty());
        self.rehash_idx = -1;
    }

    /// 负载因子过低时缩容，避免删空后仍占着大数组。
    fn maybe_shrink(&mut self) {
        if self.is_rehashing() {
            return;
        }
        let buckets = self.ht[0].len();
        if buckets > SHRINK_MIN_BUCKETS && self.used * SHRINK_RATIO < buckets {
            let target = next_pow2(self.used.max(INITIAL_BUCKETS));
            if target < buckets {
                self.start_rehash(target);
            }
        }
    }

    /// 在两张表里找可写槽位。
    fn find_slot_mut(&mut self, hash: u64, key: &[u8]) -> Option<&mut V> {
        let rehashing = self.rehash_idx >= 0;
        let [t0, t1] = &mut self.ht;
        if let Some(v) = find_in_table_mut(t0, hash, key) {
            return Some(v);
        }
        if rehashing {
            return find_in_table_mut(t1, hash, key);
        }
        None
    }
}

/// `ht` 的只读遍历器：先走 `ht[0]` 的每个桶，再走 `ht[1]`。
struct Iter<'a, V> {
    tables: [&'a [Option<Box<Node<V>>>]; 2],
    table: usize,
    bucket: usize,
    cur: Option<&'a Node<V>>,
}

impl<'a, V> Iterator for Iter<'a, V> {
    type Item = (&'a Vec<u8>, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(node) = self.cur {
                self.cur = node.next.as_deref();
                return Some((&node.key, &node.value));
            }
            let table = *self.tables.get(self.table)?;
            if self.bucket >= table.len() {
                self.table += 1;
                self.bucket = 0;
                continue;
            }
            self.cur = table[self.bucket].as_deref();
            self.bucket += 1;
        }
    }
}

/// 在单张表里查找。
fn find_in_table<'a, V>(table: &'a Table<V>, hash: u64, key: &[u8]) -> Option<&'a V> {
    if table.is_empty() {
        return None;
    }
    let mut cur = table.buckets[(hash & table.mask()) as usize].as_deref();
    while let Some(node) = cur {
        if node.hash == hash && node.key == key {
            return Some(&node.value);
        }
        cur = node.next.as_deref();
    }
    None
}

/// 在单张表里查找（可变）。
fn find_in_table_mut<'a, V>(table: &'a mut Table<V>, hash: u64, key: &[u8]) -> Option<&'a mut V> {
    if table.is_empty() {
        return None;
    }
    let idx = (hash & table.mask()) as usize;
    let mut cur = table.buckets[idx].as_deref_mut();
    while let Some(node) = cur {
        if node.hash == hash && node.key == key {
            return Some(&mut node.value);
        }
        cur = node.next.as_deref_mut();
    }
    None
}

/// 在单张表里删除。
fn remove_from_table<V>(table: &mut Table<V>, hash: u64, key: &[u8]) -> Option<V> {
    if table.is_empty() {
        return None;
    }
    let idx = (hash & table.mask()) as usize;
    // 先处理头节点：链表的头节点没有前驱，摘链方式与中间节点不同。
    let head = table.buckets[idx].as_ref()?;
    if head.hash == hash && head.key == key {
        let mut node = table.buckets[idx].take()?;
        table.buckets[idx] = node.next.take();
        return Some(node.value);
    }
    // 再沿链找前驱：找到后把 `prev.next` 直接指向 `cur.next`。
    let mut prev = table.buckets[idx].as_mut()?;
    while let Some(next) = prev.next.as_mut() {
        if next.hash == hash && next.key == key {
            let mut removed = prev.next.take()?;
            prev.next = removed.next.take();
            return Some(removed.value);
        }
        prev = prev.next.as_mut()?;
    }
    None
}

/// 把一条链上的键全部追加到 `out`。
fn collect_chain<V>(bucket: &Option<Box<Node<V>>>, out: &mut Vec<Vec<u8>>) {
    let mut cur = bucket.as_deref();
    while let Some(node) = cur {
        out.push(node.key.clone());
        cur = node.next.as_deref();
    }
}

/// 大于等于 `n` 的最小 2 的幂（下限 `INITIAL_BUCKETS`）。
fn next_pow2(n: usize) -> usize {
    let mut size = INITIAL_BUCKETS;
    while size < n {
        size <<= 1;
    }
    size
}

/// 64 位逐位反转。SCAN 的「反向二进制递增」靠它实现。
fn rev64(mut v: u64) -> u64 {
    v = ((v >> 1) & 0x5555_5555_5555_5555) | ((v & 0x5555_5555_5555_5555) << 1);
    v = ((v >> 2) & 0x3333_3333_3333_3333) | ((v & 0x3333_3333_3333_3333) << 2);
    v = ((v >> 4) & 0x0F0F_0F0F_0F0F_0F0F) | ((v & 0x0F0F_0F0F_0F0F_0F0F) << 4);
    v = ((v >> 8) & 0x00FF_00FF_00FF_00FF) | ((v & 0x00FF_00FF_00FF_00FF) << 8);
    v = ((v >> 16) & 0x0000_FFFF_0000_FFFF) | ((v & 0x0000_FFFF_0000_FFFF) << 16);
    v.rotate_left(32)
}

/// FNV-1a 64 位哈希。
///
/// 选择 FNV-1a 的理由：实现只有两行、无查表、对短键（本项目绝大多数键是
/// `product:12` 这类短串）分布均匀，且完全确定性——测试里可以直接断言桶分布。
pub fn fnv1a(key: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in key {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 确定性 xorshift64，测试里替代 `rand`（本 crate 不引入额外依赖）。
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn insert_get_remove() {
        let mut d: Dict<i32> = Dict::new();
        assert!(d.is_empty());
        assert_eq!(d.insert(b"a".to_vec(), 1), None);
        assert_eq!(d.insert(b"a".to_vec(), 2), Some(1));
        assert_eq!(d.get(b"a"), Some(&2));
        assert_eq!(d.len(), 1);
        assert_eq!(d.remove(b"a"), Some(2));
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn update_in_place_keeps_len() {
        let mut d: Dict<i32> = Dict::new();
        for i in 0..100 {
            d.insert(format!("k{i}").into_bytes(), i);
        }
        let before = d.len();
        d.insert(b"k0".to_vec(), 999);
        assert_eq!(d.len(), before);
        assert_eq!(d.get(b"k0"), Some(&999));
    }

    #[test]
    fn remove_missing_returns_none() {
        let mut d: Dict<i32> = Dict::new();
        assert_eq!(d.remove(b"nope"), None);
        d.insert(b"x".to_vec(), 1);
        assert_eq!(d.remove(b"nope"), None);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn get_mut_modifies_value() {
        let mut d: Dict<i32> = Dict::new();
        d.insert(b"x".to_vec(), 1);
        if let Some(v) = d.get_mut(b"x") {
            *v += 41;
        }
        assert_eq!(d.get(b"x"), Some(&42));
        assert!(d.get_mut(b"missing").is_none());
    }

    #[test]
    fn fnv1a_is_stable_and_distinct() {
        // 固定向量：保证哈希实现不会被无意改动。
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fnv1a(b"abc"), fnv1a(b"abd"));
    }

    #[test]
    fn grows_and_keeps_everything_reachable() {
        let mut d: Dict<usize> = Dict::new();
        for i in 0..2000 {
            d.insert(format!("key:{i}").into_bytes(), i);
        }
        assert_eq!(d.len(), 2000);
        // 扩容后所有元素必须仍然可达。
        for i in 0..2000 {
            assert_eq!(
                d.get(format!("key:{i}").as_bytes()),
                Some(&i),
                "丢失 key:{i}"
            );
        }
        // 插入过程中 rehash 已经被逐步推进完毕。
        assert!(!d.is_rehashing(), "2000 次插入后 rehash 应该已完成");
    }

    #[test]
    fn load_factor_triggers_rehash_and_rehash_completes() {
        let mut d: Dict<usize> = Dict::new();
        let buckets = d.bucket_count();
        for i in 0..(buckets + 1) {
            d.insert(format!("k{i}").into_bytes(), i);
        }
        // 刚超过负载因子 1.0 时应该进入 rehash 状态（rehash_step(1) 只搬一步）。
        assert!(d.bucket_count() > buckets, "负载因子超限后桶数应增长");
        assert_eq!(d.len(), buckets + 1);
        // 推进到底后必须结束。
        d.rehash_step(usize::MAX / 2);
        assert!(!d.is_rehashing());
    }

    #[test]
    fn read_and_write_during_rehash() {
        let mut d: Dict<usize> = Dict::new();
        // 先把 ht[0] 撑到 2048 个桶，再预分配一个远大于它的 ht[1]：
        // 这样 rehash 需要搬 2048 个桶，而每次写只搬 1 个，能稳定停在「中途」。
        for i in 0..1000 {
            d.insert(format!("a{i}").into_bytes(), i);
        }
        d.rehash_step(usize::MAX / 2);
        assert!(!d.is_rehashing());
        d.reserve(65536);
        for i in 1000..1100 {
            d.insert(format!("a{i}").into_bytes(), i);
        }
        assert!(d.is_rehashing(), "构造阶段应处于 rehash 中");

        // rehash 中途：老键可读、新键可写、删除也对。
        assert_eq!(d.get(b"a0"), Some(&0));
        d.insert(b"fresh".to_vec(), 12345);
        assert_eq!(d.get(b"fresh"), Some(&12345));
        assert_eq!(d.remove(b"a1"), Some(1));
        assert_eq!(d.get(b"a1"), None);
        // 1000 + 100 个 a* 键，加上 fresh，再删掉 a1。
        assert_eq!(d.len(), 1100);

        // 继续推进 rehash，之前写的键必须还在。
        d.rehash_step(usize::MAX / 2);
        assert!(!d.is_rehashing());
        assert_eq!(d.get(b"fresh"), Some(&12345));
        for i in 2..1100 {
            assert_eq!(d.get(format!("a{i}").as_bytes()), Some(&i));
        }
    }

    #[test]
    fn scan_visits_every_key_exactly_once() {
        let mut d: Dict<usize> = Dict::new();
        for i in 0..777 {
            d.insert(format!("s{i}").into_bytes(), i);
        }
        let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut cursor = 0u64;
        let mut rounds = 0;
        loop {
            let (next, keys) = d.scan(cursor, 7);
            for k in keys {
                *seen.entry(k).or_insert(0) += 1;
            }
            cursor = next;
            rounds += 1;
            assert!(rounds < 10_000, "SCAN 没有收敛，疑似死循环");
            if cursor == 0 {
                break;
            }
        }
        assert_eq!(seen.len(), 777, "有键没被扫到");
        for (k, n) in &seen {
            assert_eq!(*n, 1, "键 {k:?} 被重复扫到 {n} 次");
        }
    }

    #[test]
    fn scan_on_empty_dict_terminates() {
        let d: Dict<usize> = Dict::new();
        assert_eq!(d.scan(0, 10), (0, Vec::new()));
    }

    #[test]
    fn bucket_distribution_matches_bucket_count() {
        let mut d: Dict<usize> = Dict::new();
        assert_eq!(d.bucket_distribution().len(), d.bucket_count());
        for i in 0..1000 {
            d.insert(format!("d{i}").into_bytes(), i);
        }
        let dist = d.bucket_distribution();
        assert_eq!(dist.len(), d.bucket_count());
        assert_eq!(dist.iter().sum::<usize>(), d.len());
    }

    #[test]
    fn clear_resets_everything() {
        let mut d: Dict<usize> = Dict::new();
        for i in 0..500 {
            d.insert(format!("c{i}").into_bytes(), i);
        }
        d.clear();
        assert_eq!(d.len(), 0);
        assert!(!d.is_rehashing());
        assert_eq!(d.get(b"c0"), None);
        assert_eq!(d.bucket_count(), INITIAL_BUCKETS);
    }

    #[test]
    fn shrink_after_massive_removal() {
        let mut d: Dict<usize> = Dict::new();
        for i in 0..4096 {
            d.insert(format!("z{i}").into_bytes(), i);
        }
        let big = d.bucket_count();
        for i in 0..4090 {
            d.remove(format!("z{i}").as_bytes());
        }
        d.rehash_step(usize::MAX / 2);
        assert!(d.bucket_count() < big, "大量删除后应该缩容");
        assert_eq!(d.len(), 6);
        for i in 4090..4096 {
            assert_eq!(d.get(format!("z{i}").as_bytes()), Some(&i));
        }
    }

    #[test]
    fn matches_std_hashmap_on_100k_random_keys() {
        let mut rng = Rng(0x1234_5678_9abc_def0);
        let mut d: Dict<u64> = Dict::new();
        let mut std_map: HashMap<Vec<u8>, u64> = HashMap::new();

        // 10 万个随机键，长度 1..=24 字节。
        let mut keys: Vec<Vec<u8>> = Vec::with_capacity(100_000);
        for _ in 0..100_000 {
            let len = (rng.next() % 24 + 1) as usize;
            let mut key = Vec::with_capacity(len);
            for _ in 0..len {
                key.push((rng.next() % 256) as u8);
            }
            keys.push(key);
        }

        for key in keys.iter() {
            let val = rng.next();
            assert_eq!(d.insert(key.clone(), val), std_map.insert(key.clone(), val));
        }
        assert_eq!(d.len(), std_map.len());

        // 读：与 std 逐键比对。
        for key in &keys {
            assert_eq!(d.get(key), std_map.get(key));
        }

        // 删一半，再比对。
        for (i, key) in keys.iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(d.remove(key), std_map.remove(key));
            }
        }
        assert_eq!(d.len(), std_map.len());
        for key in &keys {
            assert_eq!(d.get(key), std_map.get(key));
        }

        // 遍历结果集合必须一致。
        let mine: std::collections::HashSet<&Vec<u8>> = d.keys().collect();
        let theirs: std::collections::HashSet<&Vec<u8>> = std_map.keys().collect();
        assert_eq!(mine, theirs);
    }
}
