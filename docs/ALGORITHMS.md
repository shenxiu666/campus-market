# 推荐引擎算法设计

本文档说明 `crates/campus-rec` 中每个算法的**问题背景、设计思路、数学公式、伪代码、复杂度与工程取舍**，并附**实测数据**。

阅读顺序建议：第 1 节（排序）→ 第 2 节（Top-K）→ 第 3 节（倒排索引）→ 第 4 节（相似度）→ 第 5 节（共现）→ 第 6 节（热度）→ 第 7 节（混合推荐）。

---

## 0. 模块总览

| 文件 | 职责 | 核心数据结构 | 关键复杂度 |
|---|---|---|---|
| `sort.rs` | 三路快排、Top-K 堆、排序键 | 原地数组 | `O(n log n)` / `O(n log k)` |
| `index.rs` | 标签倒排索引 | `HashMap<String, Vec<u64>>` | 求交 `O(m+n)` |
| `similarity.rs` | Jaccard / 余弦 | 集合、稀疏向量 | `O(nnz)` |
| `cf.rs` | 行为共现矩阵 | `HashMap<(u64,u64), u32>` | 构建 `O(ΣEᵤ·w)` |
| `hot.rs` | 热度重力衰减 | 标量 | `O(1)` |
| `recommender.rs` | 混合打分 + 重排 + 解释 | 召回池 | `O(\|C\|·log\|C\|)` |

数据规模（课程项目 `data/seed.json`）：8 个用户、64 个商品、76 条行为事件、25 个标签。算法全部按「能扛住 10⁵ 量级」设计，便于演示扩展性。

**验证**：`crates/campus-rec` 共有 **125 个单元测试**——`sort` 20 个、`index` 14 个、`similarity` 14 个、`cf` 15 个、`hot` 13 个、`recommender` 49 个。其中包含随机对照测试（1000 组随机数据与 `slice::sort_by` 逐元素比对、300 组 `top_k` 与全排序前缀比对、200 组索引查询与 `HashSet` 比对、300 组双指针归并正确性比对、300 组零分配 Jaccard 与标准实现比对）。

```bash
cargo test -p campus-rec    # 125 passed; 0 failed
```

---

## 1. 三路快速排序（`sort.rs`）

### 1.1 问题

商品列表需要按价格 / 时间 / 热度排序。课程要求**自研排序**，因此不能调用 `slice::sort_by` 或 `slice::sort_unstable_by` 作为主排序（标准库只在单元测试里充当"正确答案"对照组）。

真实输入有两个特点，决定了算法选型：

1. **重复键多**。价格档位有限（100 元、200 元……），同一档位有大量商品；`created_at` 精度到秒，同秒发布的商品也会撞键。
2. **可能已经有序**。数据库按 id 自增插入，如果恰好按 id 排序，取首元素当枢轴的标准快排会退化成 `O(n²)`。

### 1.2 思路

在经典快排上做三处改进：

**(a) median-of-3 选枢轴**

取首、中、尾三个元素的中位数作为枢轴。这三个元素恰好是「已排序」「逆序」输入下的极端值，取中位数就把它们排除了，从而避免 `O(n²)` 退化。

**(b) 三路划分（Dutch National Flag）**

一次扫描把区间切成三段：

```
[ < pivot | == pivot | > pivot ]
```

二路划分（`partition`）把「等于枢轴」的元素随意分到左右两侧，重复键多时会反复处理同一批相等元素。三路划分把相等段一次性固定下来，**后续递归完全跳过它**。当所有键都相等时，一次划分就结束，复杂度从 `O(n log n)` 降到 `O(n)`。

**(c) 小区间插入排序 + 尾递归优化**

- 区间长度 `≤ INSERTION_THRESHOLD = 16` 时改用插入排序。小数组上插入排序的常数因子远小于快排（无递归、无函数调用、顺序访存）。
- 划分后**只把较小的一侧压栈**，较大的一侧用 `loop` 继续处理（尾递归优化的迭代写法）。这样栈深 ≤ `log₂(n)`，即使遇到极端不平衡的划分也不会爆栈。若两侧都压栈，最坏情况栈深可达 `O(n)`。

### 1.3 公式

三路划分后的不变量：

$$
\text{items} = \underbrace{[\ldots]}_{<p} \;\; \underbrace{[p, p, \ldots, p]}_{=p} \;\; \underbrace{[\ldots]}_{>p}
$$

递归只作用于左右两段：

$$
T(n) = T(k) + T(n - k - m) + \Theta(n), \quad m = |\{i : a_i = p\}|
$$

当 $m = n$（全相等）时 $T(n) = \Theta(n)$。

### 1.4 伪代码

```text
three_way_quick_sort(items, cmp):
    stack ← []
    lo, hi ← 0, len(items)
    loop:
        if hi - lo > 1:
            if hi - lo ≤ 16:
                insertion_sort(items[lo..hi], cmp)
            else:
                (eq_lo, eq_hi) ← partition_three_way(items[lo..hi], cmp)
                eq_lo, eq_hi ← lo + eq_lo, lo + eq_hi
                if (eq_lo - lo) < (hi - eq_hi):      # 左侧更小
                    if eq_lo > lo: stack.push((lo, eq_lo))
                    lo ← eq_hi                       # 循环处理右侧
                else:
                    if eq_hi < hi: stack.push((eq_hi, hi))
                    hi ← eq_lo                       # 循环处理左侧
                continue
        if stack is empty: break
        (lo, hi) ← stack.pop()

partition_three_way(a, cmp):
    # median-of-3：把首/中/尾排好，中位数放到 mid
    if cmp(a[mid], a[0]) < 0: swap(a[0], a[mid])
    if cmp(a[n-1], a[mid]) < 0: swap(a[mid], a[n-1])
    if cmp(a[mid], a[0]) < 0: swap(a[0], a[mid])
    swap(a[0], a[mid])                # 枢轴搬到 0 号位
    lt, i, gt ← 1, 1, n
    while i < gt:
        c ← cmp(a[i], a[0])
        if c < 0:      swap(a[lt], a[i]); lt++; i++
        elif c > 0:    gt--; swap(a[i], a[gt])
        else:          i++
    swap(a[0], a[lt-1])               # 枢轴归位到相等段左端
    return (lt - 1, gt)
```

### 1.5 复杂度

| 指标 | 值 |
|---|---|
| 平均时间 | `O(n log n)` |
| 最好时间 | `O(n)`（全部相等键） |
| 最坏时间 | `O(n²)`（概率极低，median-of-3 已消除有序/逆序退化） |
| 空间 | `O(log n)`（显式栈，因只压小侧） |
| 稳定性 | 不稳定（划分会打乱相等元素顺序） |

### 1.6 本项目中的取舍

**为什么不直接调 `sort_unstable_by`？** 课程要求自研，这是硬约束。但更重要的教学价值在于：实测表明（见 §8）自研实现在 `n = 10⁵` 时约为标准库的 **0.83×**，即慢 20%。标准库的 `sort_unstable` 是 pattern-defeating quicksort（pdqsort），额外做了「检测到划分不均衡就改用 heapsort」「检测到已有序就提前返回」等启发式。我们的实现只做了三路划分 + median-of-3，**差距的来源是可解释的**——这正是本次实验想展示的：算法工程里的常数优化（pdqsort 的探测逻辑）比大 O 更影响实际性能。

**为什么不追求稳定排序？** `compare_for` 已经用「主键 → `created_at` 降序 → `id` 升序」做了**全序** tie-break，任意两个商品的比较结果都不相等（除非 id 相同，而 id 唯一）。全序下稳定与否不影响结果，因此可以放心用不稳定的三路快排。

### 1.7 排序键与三级 tie-break（`compare_for`）

```rust
pub fn compare_for(key: SortKey, now: i64) -> impl Fn(&Product, &Product) -> Ordering
```

四种主键：

| `SortKey` | 主键比较 |
|---|---|
| `PriceAsc` | `price_cents` 升序 |
| `PriceDesc` | `price_cents` 降序 |
| `Newest` | `created_at` 降序 |
| `Hot` | `hot_score(p, now)` 降序（浮点用 `total_cmp`） |

tie-break 链：

```text
primary
  .then_with(|| b.created_at.cmp(&a.created_at))   // 2. 新的在前
  .then_with(|| a.id.cmp(&b.id))                   // 3. id 小的在前
```

**为什么必须做 tie-break？** 三个理由：

1. **确定性**。没有 tie-break 时，相等元素的相对顺序取决于划分的随机性，同一份数据两次查询可能给出不同顺序，前端会出现「刷新一下顺序就变了」的诡异现象，也让单元测试无法断言。
2. **可复现**。实验数据（§8）要求同一输入多次运行结果一致，否则测出来的耗时没有可比性。
3. **业务合理性**。同价格时新品优先（`created_at` 降序）；同价格同时刻时按 id 升序，保证最终兜底。

**浮点为什么用 `total_cmp`？** `f64::partial_cmp` 在遇到 `NaN` 时返回 `None`，而排序比较器必须返回全序。热度分理论上不会出现 `NaN`（分母有 `+2` 缓冲），但一旦上游数据出现 `NaN`（比如手写 JSON 里塞了非法数字），`partial_cmp().unwrap()` 会直接 panic，整个服务崩掉。`total_cmp` 按 IEEE 754 位模式比较，给出确定的全序，`NaN` 被排到末尾而不是崩溃——**防御性编程优于乐观假设**。

---

## 2. Top-K 二叉堆（`sort.rs`）

### 2.1 问题

「热度榜前 10」「为你推荐前 20」这类需求只要**前 k 个**，不需要全序。`n = 10⁵`、`k = 10` 时，全排序要排 10 万个元素只为取前 10 个，浪费 99.99% 的计算。

### 2.2 思路

维护一个**大小为 k 的最小堆**（堆顶是当前保留集合中「最差」的元素）：

1. 前 k 个元素直接入堆，用 Floyd 建堆一次性调整为堆（`O(k)`）；
2. 对后续每个元素，与堆顶比较：
   - 比堆顶差 → 直接丢弃（一次比较，`O(1)`）
   - 比堆顶好 → 替换堆顶并下沉（`O(log k)`）

扫描结束后堆里就是前 k 个，再按堆排序取出升序结果。

**关键点：为什么是「最小堆」而不是「最大堆」？** 因为我们要保留的是「最好的 k 个」，所以堆顶必须是最差的那个——这样新元素只要和堆顶比一次就知道该不该留。如果用最大堆，堆顶是最好的，新元素无从判断。

### 2.3 公式

堆操作的比较次数：

$$
C(n, k) = \underbrace{n - k}_{\text{与堆顶比较}} + \underbrace{\sum_{i=k+1}^{n} [a_i > \text{heap}[0]] \cdot \log_2 k}_{\text{替换后下沉}}
$$

平均情况下替换概率约 $k/i$，故总代价约 $O(n \log k)$。

### 2.4 伪代码

```text
top_k(items, k, cmp):
    if k == 0 or items is empty: return []
    k ← min(k, len(items))
    heap ← []                                  # 存下标，避免借用冲突
    for i in 0 .. len(items):
        if len(heap) < k:
            heap.push(i)
            if len(heap) == k:
                for j in (0 .. k/2).reversed():   # Floyd 建堆 O(k)
                    sift_down(heap, j)
        elif cmp(items[i], items[heap[0]]) < 0:   # 比堆顶好
            heap[0] ← i
            sift_down(heap, 0)
    # 堆排序取出升序：反复把堆顶换到末尾
    for end in (1 .. k).reversed():
        swap(heap[0], heap[end])
        sift_down(heap[0 .. end], 0)
    return heap.map(|i| &items[i])

sift_down(heap, i, len):
    loop:
        l, r ← 2*i + 1, 2*i + 2
        worst ← i
        if l < len and cmp(items[heap[l]], items[heap[worst]]) > 0: worst ← l
        if r < len and cmp(items[heap[r]], items[heap[worst]]) > 0: worst ← r
        if worst == i: return
        swap(heap[i], heap[worst]); i ← worst
```

### 2.5 复杂度

| 方案 | 时间 | 额外空间 |
|---|---|---|
| 全排序后截断 | `O(n log n)` | `O(n)`（或 `O(log n)` 原地） |
| **大小为 k 的堆** | **`O(n log k)`** | **`O(k)`** |
| 快速选择（quickselect） | `O(n)` 平均 | `O(1)` |

### 2.6 本项目中的取舍

**为什么不用 quickselect？** quickselect 平均 `O(n)`，理论上比堆快。但：

1. quickselect 需要**原地修改**数组（把前 k 个元素划分到前面），而 `top_k` 的签名是 `&[Product]`（不可变切片），返回 `Vec<&Product>`。要么复制整个数组（`O(n)` 空间 + 拷贝开销），要么改成 `&mut`——两者都会破坏冻结的公开签名。
2. quickselect 的最坏情况是 `O(n²)`（虽可用 median-of-medians 保证 `O(n)`，但常数极大，实际比堆慢）。
3. 堆的实现更简单、更稳定，且 `O(n log k)` 在 `k` 很小时（推荐场景 `k = 10`）几乎等同于 `O(n)`。

**为什么堆里存下标而不是引用？** 如果堆存 `&T`，那么 `cmp(&items[x], &items[y])` 需要不可变借用 `items`，而堆本身的可变操作（`swap`、`sift_down`）又需要可变借用堆——两者生命周期交叉时借用检查器会拒绝。存下标后，堆是 `Vec<usize>`，与 `items` 完全解耦，`by_index` 闭包只捕获 `items` 的不可变引用。

**Floyd 建堆为什么是 `O(k)` 而不是 `O(k log k)`？** 逐个插入是 `k` 次 `O(log k)` 下沉。Floyd 建堆从最后一个非叶节点往前下沉，高度为 `h` 的节点有 `k/2^h` 个，总代价 $\sum_h \frac{k}{2^h} \cdot h = O(k)$。

---

## 3. 标签倒排索引（`index.rs`）

### 3.1 问题

搜索页要按标签筛选：「自行车 AND 运动」「教材 OR 考研」。最直接的做法是线性扫描全部商品，逐个检查标签——`O(N·T)`，`N = 10⁵` 时每次查询要遍历 10 万个商品。

### 3.2 思路

**倒排索引**：预先建立「标签 → 商品 id 列表」的映射（posting list），查询时直接取出相关列表再做集合运算，只处理命中的商品。

```
自行车  → [3, 7, 12, 45, ...]
运动    → [3, 12, 60, ...]
教材    → [8, 9, 15, ...]
```

**关键设计：posting list 始终保持升序。** 这带来三个好处：

1. 求交 / 求并可以用**双指针归并**，`O(m + n)`，顺序访存，对 CPU 缓存友好；
2. 结果天然有序，不需要再排一次；
3. 输出确定（同一查询永远返回同样的顺序），便于测试。

**为什么不用 `HashSet`？** `HashSet` 求交要遍历较小集合、逐个哈希查找。虽然平均 `O(1)`，但每次查找都有哈希计算 + 探测 + 随机访存（cache miss）。更重要的是结果**无序**——想要确定输出还得额外排序，反而更慢。倒排表的有序性是「免费的」，构建时排一次，之后所有查询都受益。

### 3.3 公式

设查询有 $t$ 个标签，posting list 长度为 $m_1, m_2, \ldots, m_t$：

- **OR（并集）**：两两归并，总代价 $O\left(\sum_{i} m_i\right)$
- **AND（交集）**：按长度**升序**依次求交，总代价 $O\left(\min_i m_i \cdot t\right)$（先与最短的求交，结果集迅速缩小）

### 3.4 伪代码

```text
build(products):
    for p in products:
        for tag in p.tags:
            postings[tag].push(p.id)
    for list in postings.values():
        sort_unstable(list); dedup(list)

upsert(product):
    for tag in product.tags:
        list ← postings[tag]
        pos ← partition_point(list, |x| x < product.id)   # 二分
        if list[pos] != product.id: list.insert(pos, product.id)

remove(product_id, tags):
    for tag in tags:
        list ← postings[tag]
        pos ← binary_search(list, product_id)
        if found: list.remove(pos)
        if list.is_empty(): postings.remove(tag)          # 清理空键

intersect_sorted(a, b):
    i, j ← 0, 0; out ← []
    while i < len(a) and j < len(b):
        if a[i] == b[j]: out.push(a[i]); i++; j++
        elif a[i] < b[j]: i++
        else: j++
    return out

union_sorted(a, b):
    i, j ← 0, 0; out ← []
    while i < len(a) and j < len(b):
        if a[i] < b[j]: out.push(a[i]); i++
        elif a[i] > b[j]: out.push(b[j]); j++
        else: out.push(a[i]); i++; j++
    append remaining
    return out

search(tags, mode):
    if tags is empty: return []
    lists ← tags.map(|t| postings.get(t) or [])
    return mode == And ? intersect_all(lists) : union_all(lists)
```

### 3.5 复杂度

| 操作 | 时间 | 说明 |
|---|---|---|
| `build` | `O(N·T + Σ|list|·log|list|)` | 一次性构建，插入后各自排序 |
| `upsert` | `O(T·log|list|)` | 二分插入，幂等 |
| `remove` | `O(T·log|list| + T)` | 二分查找 + `Vec::remove`（`O(|list|)`） |
| `search(OR)` | `O(Σmᵢ)` | 归并 |
| `search(AND)` | `O(min(mᵢ)·t)` | 先与最短求交 |
| 空间 | `O(Σ|posting|)` | 每个 (标签, 商品) 对存一个 `u64` |

### 3.6 本项目中的取舍

**为什么 posting list 用 `Vec<u64>` 而不是压缩位图（Roaring Bitmap）？** 位图在标签基数很低（每个标签命中大量商品）时更快，但：

1. 本项目标签基数高（40+ 个标签，每个标签只命中 5% 商品），位图会浪费大量零位；
2. 位图求交是位运算，需要预先分配 `N/64` 个 `u64`，`N = 10⁵` 时每个标签 1.5KB，40 个标签 60KB——比稀疏 posting list 大；
3. 课程演示用 `Vec` 更直观，双指针归并的 `O(m+n)` 也更容易讲清楚。

**为什么 `remove` 后要删空键？** 否则 `len()` 会把「没有任何商品的标签」也算进去，`tag_counts()` 会输出一堆 0 计数的噪音。删除空键后，`len()` 的语义是「当前有商品的标签数」，符合直觉。

**`intersect_all` 为什么按长度升序？** 交集只会变小。先与最短的 list 求交，结果集迅速缩小，后续每次求交的代价都更低。若按原顺序，可能先用最长的 list 求交，白白处理大量不可能命中的元素。

---

## 4. 相似度度量（`similarity.rs`）

### 4.1 问题

两个商品的标签有多像？两个用户的行为有多像？这是推荐系统的基石。

### 4.2 Jaccard 相似度

**适用场景**：集合语义——「有 / 无」，不关心次数和权重。商品的标签集合天然是集合语义：一件商品要么带「自行车」标签，要么不带，带两次和带一次没区别。

$$
J(A, B) = \frac{|A \cap B|}{|A \cup B|} = \frac{|A \cap B|}{|A| + |B| - |A \cap B|}
$$

取值范围 $[0, 1]$，完全相同为 1，完全不相交为 0。

**空集约定**：两边都为空时返回 **0.0** 而不是 1.0。数学上 $\frac{0}{0}$ 未定义，但业务上「两个都没信息」不应该被判为「完全相似」——否则一个没有标签的商品会与所有空标签商品「相似」，污染推荐结果。

**实现要点**：不真的构造并集，用容斥原理 `|A ∪ B| = |A| + |B| - |A ∩ B|`，省一次集合构造。

### 4.3 余弦相似度

**适用场景**：向量语义——关心**权重与强度**。用户 A 看了 5 次商品 X，用户 B 看了 1 次，强度不同，Jaccard 会把它们视为「都看过」而忽略差异，余弦能保留强度信息。

$$
\cos(\mathbf{a}, \mathbf{b}) = \frac{\sum_i a_i b_i}{\|\mathbf{a}\| \|\mathbf{b}\|}, \quad \|\mathbf{a}\| = \sqrt{\sum_i a_i^2}
$$

取值范围 $[-1, 1]$（非负权重时为 $[0, 1]$）。

**稀疏实现**：输入是 `&[(u64, f64)]`（下标 → 权重），只遍历非零分量。复杂度 `O(nnz(a) + nnz(b))`，而不是稠密的 `O(d)`（`d` 为维度）。用户-商品矩阵极其稀疏（一个用户只看几十个商品，商品有十万个），稠密实现会浪费 99.9% 的乘法。

**实现要点**：

1. 把较短的一侧建成 `HashMap<u64, f64>`，较长的一侧做探测——空间 `O(min(m,n))`；
2. 同一分量重复出现时累加（稀疏向量表示允许分片）；
3. 结果 `clamp(-1.0, 1.0)`——浮点误差可能让 `0.9999999999` 算成 `1.0000000002`，超出定义域会让下游断言失败。

### 4.4 对比

| 维度 | Jaccard | 余弦 |
|---|---|---|
| 输入语义 | 集合（有/无） | 向量（权重） |
| 是否看强度 | 否 | 是 |
| 取值范围 | `[0, 1]` | `[-1, 1]` |
| 典型用途 | 商品标签、用户兴趣标签 | 用户行为向量、TF-IDF |
| 本项目用在哪 | 标签匹配 `tag_sim`、多样性惩罚 | （预留）行为向量相似度 |

**为什么不统一用一种？** 标签是布尔语义，用余弦需要先构造「标签 → 0/1 权重」的稀疏向量，多一层转换却得到和 Jaccard 等价的结果（对 0/1 向量，余弦 = Jaccard 的单调变换）。行为是计数语义，用 Jaccard 会丢失「看 5 次 vs 看 1 次」的差异。**按语义选度量**，而不是按方便选。

### 4.5 小集合的工程优化：零分配 Jaccard

标准实现（`jaccard`）用两个 `HashSet` 求交，复杂度 `O(|a| + |b|)`，教科书最优。但**理论最优不等于实测最优**：当集合只有 1~5 个元素时，两次堆分配 + 每个元素一次哈希的固定开销，远远超过几次字符串比较。

于是本项目并存三个 Jaccard 实现，按调用频率选：

| 函数 | 时间 | 额外空间 | 用在哪 |
|---|---|---|---|
| `jaccard(a, b)` | `O(\|a\|+\|b\|)` 期望 | `O(\|a\|+\|b\|)`（两个 `HashSet`） | 每次请求只调一次的冷路径（`tag_sim`、`similar_products`） |
| `jaccard_str(a, b)` | 同上 | 同上 | 调用方已有 `&[&str]`，省一次 `Vec<String>` 分配 |
| `jaccard_no_alloc(a, b)` | `O(\|a\|·\|b\|)` | `O(1)` | 重排内循环（每次请求约 5000 次调用） |

`jaccard_no_alloc` 的三条实现要点：

1. **去重靠「向前查找」**：`a[..i].contains(x)` 判断 `x` 是否已在前面出现过，出现过就跳过。标签列表极短，这个 `O(n²)` 去重比建 `HashSet` 便宜。
2. **交集顺手统计**：遍历 `a` 的去重元素时，`b.contains(x)` 直接判交，不需要再遍历一次。
3. **并集仍用容斥**：`|A ∪ B| = |A| + |B| - |A ∩ B|`，不构造并集。

**语义必须逐位一致**，否则推荐分数会悄悄漂移。用两个测试锁死：9 个手工边界 case（空/单元素/完全重合/完全不相交/多重集）+ 300 组随机多重集与 `jaccard` 逐位比对（误差 < 1e-12）。

**效果**：`recommend_for_user(limit=10)` 从 160.5 µs/次降到 **39~44 µs/次**，提速约 4 倍（详见 §7.5）。

**为什么不用「排序 + 双指针」？** 那要求输入已排序。商品的 `tags` 来自 JSON，顺序由数据决定、不可控；为了省分配先去 `sort` 一遍，既破坏调用方数据又更慢。

### 4.6 复杂度

| 函数 | 时间 | 空间 |
|---|---|---|
| `jaccard(a, b)` | `O(\|a\| + \|b\|)` | `O(\|a\| + \|b\|)`（两个 `HashSet`） |
| `jaccard_str(a, b)` | `O(\|a\| + \|b\|)` | 同上（省一次 `String` 分配） |
| `jaccard_no_alloc(a, b)` | `O(\|a\| · \|b\|)` | `O(1)` |
| `cosine(a, b)` | `O(nnz(a) + nnz(b))` | `O(min(nnz(a), nnz(b)))` |

---

## 5. 行为共现矩阵（`cf.rs`）

### 5.1 问题

协同过滤的核心假设：**看过商品 A 的人也看了商品 B，那么 A 和 B 相关**。需要从行为日志里统计「哪些商品被一起看过」。

### 5.2 思路

**朴素做法**：对每个用户，把他看过的所有商品两两配对。问题是一个用户一周可能看几百个商品，两两配对是 $O(n^2)$，且会把「周一看了自行车、周日看了教材」这种毫无关系的商品也配对，共现矩阵退化成**全连接**，失去区分度。

**改进：时间窗口**。只有同一用户在 **5 分钟窗口内**先后交互的两个商品才算共现。理由是：短时间内连续浏览的商品，通常是在**比较同类商品**（看了一辆自行车，又看了一辆自行车），这才是真正的「相关」信号。5 分钟之外的行为，兴趣已经漂移。

这是本项目的**稀疏化手段**：不加窗口，$N$ 个商品的理论最大共现对数是 $N(N-1)/2$；加了窗口，实际共现对数与「用户短时行为簇」成正比，通常只有几百到几千对。

**对称存储**：`(a, b)` 和 `(b, a)` 是同一件事，统一按 `(min, max)` 存储，空间直接减半。

### 5.3 公式

共现次数：

$$
\text{count}(a, b) = \sum_{u} \sum_{\substack{i < j \\ |t_i - t_j| \le 300}} \mathbb{1}\left[\{p_i, p_j\} = \{a, b\}\right]
$$

归一化到 $[0, 1]$：

$$
\text{score}(a, b) = \frac{\text{count}(a, b)}{\max_{a', b'} \text{count}(a', b')}
$$

**为什么除以全局最大而不是 `count / (count + 1)`？** `count/(count+1)` 是常见的平滑归一化，但本项目共现次数集中在个位数（5 分钟窗口很窄，同一对商品被重复配对的机会不多），`1/(1+1)=0.5`、`2/(2+1)=0.67`、`3/(3+1)=0.75`——三个值挤在 `[0.5, 0.75]`，区分度极差。除以最大值能把 `1, 2, 3` 映射到 `0.33, 0.67, 1.0`，拉开差距。

（种子数据里最大共现次数是 1，此时所有非零对的 `score` 都是 1.0——归一化退化了。这是**数据量太小**的必然结果，而不是公式的问题：样本量上来后 `max_count` 会增长，区分度自然恢复。见 §5.5 的实测与 §5.6 的讨论。）

### 5.4 伪代码

```text
build(events):
    by_user ← group events by user_id
    for (uid, evs) in by_user:
        sort evs by ts                              # 稳定排序，保证可复现
        left ← 0
        for right in 0 .. len(evs):
            while evs[right].ts - evs[left].ts > 300:
                left++
            for j in left .. right:                 # 窗口内两两配对
                bump(evs[j].pid, evs[right].pid)
        # 保留窗口内最近的行为，供后续 record 续配对
        kept ← evs.filter(|e| last_ts - e.ts ≤ 300)
        recent[uid] ← kept

record(event):
    recent ← recent[uid]
    cutoff ← event.ts - 300
    drain recent 头部所有 ts < cutoff 的记录       # 滑出窗口
    partners ← recent.map(|(_, pid)| pid)
    for p in partners:
        bump(p, event.product_id)
    recent.push((event.ts, event.product_id))
    if len(recent) > 256: truncate                       # RECENT_CAP

bump(a, b):
    if a == b: return                           # 自己和自己不配对
    key ← (min(a,b), max(a,b))                  # 对称存储
    counts[key] ← saturating_add(counts[key], 1)
    max_count ← max(max_count, counts[key])

score(a, b):
    if a == b: return 0.0
    key ← (min(a,b), max(a,b))
    return counts[key] / max_count
```

### 5.5 复杂度

| 操作 | 时间 | 空间 |
|---|---|---|
| `build` | `O(E log E + Σ Eᵤ·w)` | `O(\|counts\| + \|recent\|)` |
| `record` | `O(w + \|recent\|)` | `O(w)` |
| `score` | `O(1)` | — |
| `neighbors` | `O(\|counts\|)` | `O(k)` |

其中 `E` 为事件总数，`Eᵤ` 为用户 `u` 的事件数，`w` 为窗口内平均事件数（本项目通常是个位数）。

**空间复杂度与稀疏化**：

$$
\text{空间} = O\left(\min\left(\frac{N(N-1)}{2},\ \sum_u \binom{w_u}{2}\right)\right)
$$

- 稠密矩阵：$N \times N$，`N = 10⁵` 时需要 $10^{10}$ 个元素（40GB），完全不可行；
- 对称存储：减半，仍是 $O(N^2)$；
- **窗口稀疏化后**：只有窗口内的配对才存储，实际大小与「用户短时行为簇」成正比。

**本项目种子数据的实测**（`cargo run -p campus-rec --release --example count_co`）：

```text
商品数           = 64
事件数           = 76
标签数           = 25
共现对数         = 57
最大共现次数     = 6
稠密矩阵格子数   = 4096 (N×N)
理论最大对子数   = 2016 (N(N-1)/2)
共现次数分布     = {1: 47, 2: 4, 3: 2, 4: 1, 5: 2, 6: 1}
```

76 条事件产生了 **57 对**共现（占理论上限 2016 的 2.8%），而稠密矩阵需要 4096 个格子——**稀疏化把存储量压到了 1.4%**。共现次数分布呈长尾（47 对只出现 1 次，少数几对出现 5~6 次），符合真实行为的「偶然同看 + 少数强关联」形态。

**这组数据修正了一个曾经的误判**。第一版种子数据按「每小时一条」生成事件，用户行为间隔普遍在几百到几千秒（用户 1 的前两条相隔 2570 秒），全部落在 5 分钟窗口之外，76 条事件只配上 **1 对**共现，`CFSim` 几乎恒为 0，`W_CF = 0.35` 的权重被浪费（现权重为 0.25）。

当时的分析结论是「数据问题而不是算法问题」，并给了两个方向：调大窗口，或补种子数据。**最终采用了后者**——`scripts/gen_seed.py` 改成**会话模型**（一个会话 3~5 个行为、间隔 20~180 秒，并加入跨用户的共同会话），重新生成后相邻事件间隔中 **51/68 落在 300 秒内**，共现对数 1 → 57。

**为什么不调大窗口？** 5 分钟窗口的语义是「短时比较同类商品」——用户在一次浏览会话里连着翻几件同类商品，才是真实的兴趣信号。把窗口调到 1 小时，会把「上午看了自行车、下午看了教材」这种不相关行为也配成对，共现矩阵迅速变稠密而噪声激增。**窗口的语义是对的，错的是数据的时间分布**，所以修数据。

（`CO_WINDOW_SECS` 保持 `300` 不变；`data/seed.json` 属于共享数据文件，由项目负责人按上述设计重新生成。）


### 5.6 本项目中的取舍

**为什么 `record` 要维护 `recent`？** `build` 是一次性全量构建，但线上服务要支持增量（用户刚点了一个商品，立刻影响后续推荐）。`record` 必须知道「这个用户刚才看了什么」才能配对，所以要把窗口内的最近行为缓存起来。

**为什么 `RECENT_CAP = 256`？** 长期运行的服务如果不设上限，`recent` 会无界增长（虽然窗口外的会被清理，但极端情况下用户在一秒内刷了几万次，`recent` 会爆）。256 条足够覆盖任何合理的 5 分钟窗口，超出部分对共现统计的边际贡献已经很小。

**为什么 `record` 里要先把 partners 收集成 `Vec`？** 直接边遍历 `recent` 边调用 `bump` 会同时持有 `recent` 的可变借用和 `counts` 的可变借用——两者都是 `self` 的字段，借用检查器会拒绝。先把 partners 拷出来（`O(w)` 的 `Vec<u64>`），释放 `recent` 的借用后再 `bump`，是 Rust 里处理「同时修改两个字段」的标准手法。

**`similar_users` 为什么按「共同商品数」排序而不是共现分数？** 共同商品数是**用户对**维度的统计（两个用户都交互过几个相同商品），共现矩阵是**商品对**维度的统计。找相似用户应该用前者。同数时按 uid 升序，保证确定性。

---

## 6. 热度重力衰减（`hot.rs`）

### 6.1 问题

「热门商品」不能只看绝对交互量——一个三个月前发布、积累了 1000 次浏览的商品，不应该压过今天刚发布、1 小时就有 100 次浏览的商品。**时间必须参与打分。**

### 6.2 思路

Hacker News 的经典公式：

$$
\text{hot} = \frac{w_v \cdot v + w_c \cdot c + w_f \cdot f}{(\text{age\_hours} + 2)^{1.5}}
$$

- **分子**：加权交互量。浏览（`view`）= 1，点击（`click`）= 3，收藏（`fav`）= 5。权重递增是因为行为强度递增：浏览是「扫到了」，点击是「感兴趣」，收藏是「打算买」。
- **分母**：重力衰减。`age_hours` 是发布至今的小时数，指数 `1.5` 控制衰减速度。
- **分子数据从哪来**：`v`/`c`/`f` 不是调用方填的，而是 `Recommender` 从事件流回填与增量累加的（见 §6.6）——这一步漏掉会导致热度恒为 0，是开发中真实踩过的坑。
- **`+2`**：防止 `age = 0` 时除零，同时给刚发布的商品一个「冷启动缓冲」——刚发布 2 小时内的商品分母变化剧烈，`+2` 平滑了这段。

### 6.3 为什么用幂律而不是指数衰减？

指数衰减 $e^{-\lambda t}$ 需要选半衰期参数 $\lambda$，且**下降速度是恒定的**（每个固定时间间隔衰减同样的比例）。幂律衰减 `(t + c)^{-α}` 的特点是：

- **早期下降快**：刚发布 1 小时和 2 小时，分母从 `3^1.5 ≈ 5.2` 变成 `4^1.5 = 8`，分数差 35%；
- **后期下降慢**：30 天和 31 天，分母从 `722^1.5 ≈ 19400` 变成 `746^1.5 ≈ 20380`，分数只差 5%。

这符合二手交易的实际规律：**新发布的商品需要曝光窗口，老商品本来就该稳定在低曝光位置**，而不是无限衰减到零（老商品仍有长尾价值）。

另一个工程理由：指数衰减有**下溢问题**（`e^{-1000}` 在 `f64` 下是 0），幂律不会。

### 6.4 公式细节

$$
\text{age\_hours} = \frac{\max(0,\ \text{now} - \text{created\_at})}{3600}
$$

`age_hours <= 0`（未来时间戳 / 时钟回拨）按 0 处理，避免负指数把分母变成 `(负数)^1.5`（在实数域无定义，Rust 的 `powf` 会返回 `NaN`）。

### 6.5 归一化

热度分的量级依赖具体数据（本项目几十到几百），而混合打分里 `W_HOT = 0.15` 要求 `hot_norm ∈ [0, 1]`，否则热度会淹没其他特征。

**min-max 归一化**：

$$
x' = \frac{x - \min}{\max - \min}
$$

**边界情况**：`max == min`（所有商品热度相同）时无法拉伸，统一给 **0**。为什么不是给 1？因为「全都满分」会让 `W_HOT` 这一项变成常数，对排序没有贡献却抬高了所有商品的总分，等于凭空增加 0.15 的基线。给 0 则让热度项「沉默」，把排序权交给标签和协同过滤——**没有信息时不应该伪造信息**。

含 `NaN` / `Inf` 时**不做归一化**（保持原值），避免脏数据通过 `min`/`max` 扩散到所有元素。

**为什么在「全量在售商品」上归一化，而不是在「候选集」上？**

这是一个容易踩的坑。如果对每个用户的候选集分别做 min-max，同一个商品在不同用户的推荐列表里会得到不同的 `hot_norm`，进而得到不同的总分。后果：

1. `REC.EXPLAIN`（解释某个商品的分数）与 `REC.FOR_YOU`（推荐列表里的分数）会对不上——同一个商品两个数字，演示时无法自圆其说；
2. 归一化结果依赖候选集的构成，加入一个极端热门商品就会把其他商品的 `hot_norm` 全部压低，产生「蝴蝶效应」。

在全量在售商品上归一化，保证**同一商品在任何上下文下的 `hot_norm` 一致**，这是可解释性的前提。

**实现上如何保证两处一致？** 批量路径（`hot_norms`，供 `REC.FOR_YOU` 用）与单点路径（`hot_norm_of`，供 `REC.EXPLAIN` 用）共用同一个基准函数 `hot_baseline(&raw) -> Option<(min, span)>`，且都从同一份 `hot_raw(now)` 里取原始分：

```rust
fn hot_raw(&self, now: i64) -> Vec<(u64, f64)>;          // 一次 powf 计算，只算一遍
fn hot_baseline(raw: &[(u64, f64)]) -> Option<(f64, f64)>; // (min, span)，无基准时 None
fn hot_norms(&self, now: i64) -> HashMap<u64, f64>;        // 批量：O(n)
fn hot_norm_of(&self, product_id: u64, now: i64) -> f64;   // 单点：O(n)，不建 HashMap
```

这是对初版实现的修正：初版 `explain` 为了取**一个**商品的 `hot_norm` 而调用 `self.hot_norms(now).get(&id)`，构造了整张 `HashMap`（一次 `O(n)` 堆分配 + 哈希插入），在 `REC.EXPLAIN` 这种单次查询里是纯浪费。改成单点函数后仍是 `O(n)`（基准必须扫全量），但只分配一个 `Vec`，且数值上与批量路径**逐位相同**——因为两处用的是同一批已经算好的 `hot_score`，不存在浮点重算带来的差异。

### 6.6 数据来源：为什么计数必须从事件流回填

`hot_score` 的分子依赖 `Product` 上的 `views` / `clicks` / `favs` 三个字段。但这三个字段**不能指望调用方填**：

- `data/seed.json` 的商品对象里压根没有这三个键（实际字段是 `campus` / `condition` / `created_at` / `desc` / `id` / `price_cents` / `seller_id` / `status` / `tags` / `title`）；
- `model.rs` 里三个字段都带 `#[serde(default)]`，缺字段时静默变成 `0`；
- `REC.TRACK` 只追加事件，不会去改写商品对象。

**这是本项目开发中真实踩到的坑**：初版实现没有回填，结果 `hot_score` 的分子恒为 0，于是

- `REC.HOT` 返回的每一条 `score` 都是 `0.0000`；
- 冷启动用户的推荐全部 `0` 分；
- `REC.EXPLAIN` 里 `hot_norm = 0.0000`；
- `W_HOT = 0.10` 这一项彻底失效（等于权重被悄悄改成了 0.45 / 0.25 / 0 / 0.05 / 0.15）。

**修复思路：事件流是唯一权威来源，三个计数只是它的物化视图。**

```rust
// 构造时全量回填
for e in events {
    if let Ok(pos) = products.binary_search_by_key(&e.product_id, |p| p.id) {
        apply_event_count(&mut products[pos], e.kind);
    }
}

// 增量时同步累加
fn apply_event_count(product: &mut Product, kind: EventKind) {
    match kind {
        EventKind::View  => product.views  = product.views.saturating_add(1),
        EventKind::Click => product.clicks = product.clicks.saturating_add(1),
        EventKind::Fav   => product.favs   = product.favs.saturating_add(1),
    }
}
```

两个设计要点：

1. **回填是「累加」而不是「覆盖」**。`Product` 上可能已经有历史基数（例如从数据库读出来的老商品），事件流只代表「本进程看到的那部分行为」。覆盖会把历史数据抹掉，累加则保持单调。
2. **回填与增量共用 `apply_event_count`**。如果两处各写一遍 `match`，很容易出现「构造时把 `Click` 记成 `views`、增量时记对了」这种难以发现的偏差——两处语义必须由同一段代码定义。

修复后同一份种子数据的实测输出（`cargo run -p campus-rec --example seed_stats`）：

```text
== 热度榜 Top 5 ==
  #1   捷安特山地车      388.00   score=1.0000
  #52  《百年孤独》       57.00    score=0.4846
  #22  机械键盘         1070.00  score=0.4019
  #44  电子琴          1429.00  score=0.1084
  #37  哑铃 15kg 一对    45.00    score=0.0766

== 冷启动：用户 7（interests 为空）==
  #1   score=1.0000 reasons=[ColdStart]
  #52  score=0.4846 reasons=[ColdStart]
  #22  score=0.4019 reasons=[ColdStart]
```

`#1` 分数为 1 而 `#52` 只有 0.48，是因为前者发布不久且被 5 个用户浏览过，后者已经挂了很久——重力衰减按预期工作了。

对应的回归测试：`new_backfills_interaction_counts_from_events`、`new_backfill_makes_hot_scores_nonzero`、`record_event_increments_counts`、`record_event_changes_hot_ranking`（断言刷收藏后分数上升且名次前移）、`explain_hot_norm_matches_hot_norms`（断言两条路径逐位一致）。

### 6.7 复杂度

`hot_score` 是 `O(1)` 时间、`O(1)` 空间。`normalize_scores` 是 `O(n)` 时间、`O(1)` 额外空间（原地修改）。

---

## 7. 混合推荐（`recommender.rs`）

### 7.1 问题

单一信号都有盲区：

- **只用标签**：用户兴趣标签只有几个，召回范围窄，会漏掉「标签没写但用户会喜欢」的商品；且无法利用群体智慧。
- **只用协同过滤**：冷启动完全失效（新用户没行为、新商品没曝光），且数据稀疏时共现矩阵几乎是空的。
- **只用热度**：千人一面，没有个性化，用户会觉得「这不叫推荐」。

### 7.2 四步流水线

```
召回 (recall) → 过滤 (filter) → 打分 (score) → 重排 (re-rank) → 解释 (explain)
```

#### 第一步：召回

候选集 = **兴趣标签倒排命中** ∪ **相似用户交互过的商品**。

- 倒排命中保证「相关性」：用户喜欢「自行车」，至少能看到所有自行车；
- 相似用户商品保证「发现性」：用户没搜过但同类用户喜欢的，也能被捞出来。

两路取并集后 `sort + dedup`，保证候选集确定。

#### 第二步：过滤

去掉三类商品：

1. **非在售**（`Reserved` / `Sold`）——推荐已售商品是纯粹的伤害；
2. **已收藏**（用户收藏过）——收藏是强负反馈，已经决定要了就不必再推；
   浏览/点击过的商品仍保留在候选里（目录只有 64 件，全排会很快无货可推）；
3. **`exclude` 列表**——调用方显式要求排除的（比如前端已经展示过的）。

此外还有一道**硬门槛**：候选必须命中至少一个兴趣标签才会进入打分。召回阶段为了利用协同过滤信号，
会把「相似用户交互过的商品」也并入候选，但这些商品若与用户画像毫无交集，直接丢弃——
否则页面展示「兴趣：自行车、运动、健身」，推荐理由里却出现「标签匹配：数码、电子」，
前后自相矛盾。

#### 第三步：打分

$$
\text{score} = W_{TAG} \cdot \text{TagSim} + W_{CF} \cdot \text{CFSim} + W_{HOT} \cdot \text{HotNorm} + W_{PRICE} \cdot \text{PriceFit} + W_{CAMPUS} \cdot \text{CampusFit}
$$

权重（定义在 `lib.rs`，`recommender.rs` 再导出）：

| 常量 | 值 | 含义 |
|---|---|---|
| `W_TAG` | 0.45 | 标签匹配——最可靠的显式信号 |
| `W_CF` | 0.25 | 协同过滤——群体智慧，发现性强 |
| `W_HOT` | 0.10 | 热度——保证新品有曝光，防止马太效应 |
| `W_PRICE` | 0.05 | 价格贴合——软约束，只做微调 |
| `W_CAMPUS` | 0.15 | 同校区——校园场景里「同校区」≈「可当面交易」 |

五项之和为 1.0，且每项都在 `[0, 1]`，因此总分也在 `[0, 1]`，便于前端展示成百分比。

**各项的计算**：

**(a) `TagSim`**

$$
\text{TagSim} = J(\text{profile},\ \text{product.tags})
$$

其中 `profile` = 用户**兴趣标签**（去重，保持字典序）。这里刻意**不**并入交互过的商品标签：
页面上展示给用户的画像就是兴趣标签，若打分用「兴趣 ∪ 行为标签」，就会出现
「画像写着自行车、推荐却按数码/电子打分」的错位。浏览行为仍通过 `CFSim` 与热度间接影响排序，
不需要改变画像本身。

若 `matched` 为空（完全不匹配），`TagSim = 0`；否则算 Jaccard。

**(b) `CFSim`**

两路信号取 **max**：

$$
\text{CFSim} = \max\left(\text{item\_cf},\ \text{user\_cf}\right)
$$

- **item-based**：用户交互过的商品里，与候选商品共现最强的那一个，强度 = `co_occurrence.score(pid, candidate)`；
- **user-based**：相似用户里，有多大比例的「共同商品权重」落在候选商品上：

$$
\text{user\_cf} = \frac{\sum_{u \in \text{similar}} w_u \cdot \mathbb{1}[u \text{ 交互过 candidate}]}{\sum_{u \in \text{similar}} w_u}
$$

其中 $w_u$ 是用户 $u$ 与目标用户的共同商品数。

**为什么取 max 而不是加权平均？** 两路信号量纲不同（一个来自商品对，一个来自用户群），平均会把强信号稀释成中等信号。推荐场景里「有一个强证据」比「几个弱证据平均」更有说服力。比如 item-based 给出 0.9（用户刚看的商品和候选商品共现极强），user-based 给出 0.1（相似用户没怎么看过），平均得到 0.5，候选商品就被埋没了；取 max 得到 0.9，正确保留。

**(c) `HotNorm`**

全量在售商品上的 min-max 归一化热度（见 §6.5）。

**(d) `PriceFit`**

$$
\text{PriceFit} = \text{clamp}\left(1 - \frac{|\text{price} - \text{avg}|}{\max(\text{price},\ \text{avg},\ 1)},\ 0,\ 1\right)
$$

- 分母取 `max(price, avg, 1)` 而不是 `avg`：这样「预算 100 元却看到 10000 元」和「预算 10000 却看到 100 元」的**相对惩罚是对称的**（都用较大的那个做尺度）。若分母固定用 `avg`，前者会算出 `1 - 9900/100 = -98`（clamp 到 0，正确），后者会算出 `1 - 9900/10000 = 0.01`（几乎不惩罚）——不对称。
- `max(..., 1)` 防除零（用户 `avg_price = 0` 且商品价格 0 时）。
- 价格完全相等时 `1 - 0 = 1`，完全贴合。

#### 第四步：重排（多样性）

打分是**逐商品独立**的，会导致两个问题：

1. **同卖家垄断**：一个卖家挂了 10 个相似商品，全部占据前 10 名，推荐列表变成「某店铺专场」；
2. **同质化**：前 10 个商品全是「自行车」，用户想看别的品类就看不到。

两条规则：

**规则 1：同卖家配额**

$$
\forall s \in \text{sellers}:\ \left|\{i \in \text{result} : \text{seller}(i) = s\}\right| \le 2
$$

**规则 2：标签多样性惩罚**

与已选商品的 Jaccard 超过 0.8 时，分数乘以 0.6：

$$
\text{score}' = \text{score} \cdot \begin{cases} 0.6 & \exists j \in \text{chosen}:\ J(\text{tags}_i, \text{tags}_j) > 0.8 \\ 1.0 & \text{otherwise} \end{cases}
$$

**为什么是降权而不是直接淘汰？** 保留「用户就是想要同类商品」的可能性。如果用户搜索「自行车」，前 10 个全是自行车是**正确**的；但如果用户只是浏览首页，全是自行车就是**糟糕**的。降权让同类商品仍然可能出现，只是需要更高的原始分来补偿。

**贪心重排算法**：

```text
chosen ← []
seller_count ← {}
while len(chosen) < limit and pool not empty:
    best ← None
    for cand in pool (未选):
        if seller_count[cand.seller] ≥ 2: skip
        adjusted ← cand.score
        if any(J(cand.tags, chosen_tags) > 0.8):
            adjusted ← adjusted * 0.6
        if best is None or adjusted > best.adjusted:
            best ← (cand, adjusted)
    if best is None: break
    chosen.push(best)
    seller_count[best.seller]++
    chosen_tags ∪= best.tags
```

**复杂度**：`O(limit · |pool| · T)`，`T` 为标签数。`limit` 通常 ≤ 20，`|pool|` 是候选集大小（几十到几百），完全可接受。

**为什么不用「先全排序再过滤」？** 因为多样性惩罚**依赖已选集合**——一个商品是否被惩罚，取决于它前面已经选了谁。这是**动态规划式**的贪心：每选一个，就要重新评估剩余商品的分数。全排序无法表达这种依赖。

#### 第五步：解释（`reasons`）

每个结果带 `reasons`，直接渲染成前端徽章：

| `Reason` | 触发条件 | 前端文案 |
|---|---|---|
| `TagMatch { tags }` | `matched` 非空 | 「匹配你的兴趣：自行车、运动」 |
| `SimilarUsers { users }` | `cf_sim > 0` 且有提供者 | 「看过相似商品的人也看了」 |
| `Trending { factor }` | `hot_norm >= 0.5` | 「近期热门」 |
| `PriceFit` | `price_fit >= 0.6` | 「符合你的预算」 |
| `ColdStart` | 冷启动回退 | 「热门推荐」 |

**为什么阈值是 0.5 / 0.6？** 为了让 `reasons` 只保留**有说服力**的信号。如果 `hot_norm = 0.05` 也标「热门」，用户点进去发现是个冷门商品，信任就崩了。阈值让解释变成「只在真的成立时才说」。

### 7.3 冷启动

**触发条件**：用户不存在 **或** 兴趣标签为空。

**处理**：直接返回热度榜（`hot_ranking`），所有结果的 `reasons = [ColdStart]`。

**为什么不用「随机推荐」或「最新发布」？**

- 随机推荐：质量无保证，可能推已售商品；
- 最新发布：新品可能质量差（标题乱写、图片糊），体验差；
- 热度榜：**群体智慧的兜底**——至少是「大多数人觉得不错」的商品，最差情况也是「不功不过」。

冷启动也要遵守 `exclude` 列表（前端已经展示过的不重复推）和 `limit`。

### 7.4 其他接口

**`similar_products(pid, limit)`**：商品详情页的「相似商品」。

$$
\text{score} = 0.6 \cdot J(\text{tags}_a, \text{tags}_b) + 0.4 \cdot \text{co\_occurrence.score}(a, b)
$$

标签占 0.6（显式、可靠），共现占 0.4（隐式、发现性）。用 `top_k_by` 取前 `limit` 个（`O(n log k)`，不用全排序）。

**`hot_ranking(limit, now)`**：纯热度榜，用 `top_k_by`。

**`query(tags, mode, price_min, price_max, keyword, sort, now)`**：搜索页。

1. `tags` 非空 → 走倒排索引（`And` / `Or`）；空 → 全部商品；
2. 价格区间过滤；
3. 关键字过滤（`title` + `desc`，`to_lowercase().contains`，大小写不敏感）；
4. 用 `three_way_quick_sort` 按 `compare_for(sort, now)` 排序。

**为什么关键字用 `contains` 而不是正则 / 分词？** 课程项目的商品标题是中文短句（「九成新捷安特自行车」），没有分词器的情况下，`contains` 是最稳妥的匹配方式。引入分词（如 `jieba`）会带来额外依赖和复杂度，而收益在这个数据规模下不明显。这是**有意识的简化**，文档里记录清楚，而不是「没想到」。

**`explain(user_id, product_id, now)`**：`REC.EXPLAIN` 的实现，返回打分明细：

```rust
pub struct Explanation {
    pub product_id: u64,
    pub score: f64,
    pub tag_sim: f64,
    pub cf_sim: f64,
    pub hot_norm: f64,
    pub price_fit: f64,
    pub matched_tags: Vec<String>,
    pub similar_users: Vec<u64>,
}
```

由于 `hot_norms` 在全量商品上归一化（§6.5），`explain` 算出的 `score` 与 `recommend_for_user` 里同一商品的分数**完全一致**，可以互相验证——这是可解释性的关键保证。

注意 `explain` 内部调用的是单点版 `hot_norm_of` 而不是批量版 `hot_norms`：为了取一个值而构造整张 `HashMap` 是浪费（详见 §6.5 末尾）。两者共用 `hot_baseline`，数值逐位相同。

**`stats()`**：`REC.STATS` 的实现，返回索引规模（商品数、用户数、标签数、共现对数、事件数）。

### 7.5 性能实测与两次优化

「为你推荐」是整个推荐引擎里最重的接口：它要召回、打分、再重排。用 `data/seed.json`（64 商品 / 8 用户 / 76 事件）在 `--release` 下跑 1000 次取平均：

| 版本 | `recommend_for_user(limit=10)` |
|---|---|
| 初版（朴素实现） | 180.9 µs/次 |
| 优化 1：相似用户只算一次 | 160.5 µs/次 |
| 优化 2：重排改用零分配 Jaccard | **39~44 µs/次** |

**优化 1：把 `similar_users` 提出循环。**

初版 `cf_signal(user_id, candidate)` 内部为**每个候选商品**都重新算一遍「该用户的相似用户列表」，而这份列表与候选商品**完全无关**。把它提到召回前算一次，通过 `cf_signal_with(user_id, &similar, candidate)` 传进去：

```rust
// 召回前算一次
let similar = cf::similar_users(&self.user_items, user_id, SIMILAR_USER_K);
// 召回循环直接用
for (uid, _) in &similar { /* ... */ }
// 打分循环复用
let (cf_sim, providers) = self.cf_signal_with(user_id, &similar, pid);
```

这一项省下的是 `|C|` 次 `O(U·E)` 的重复计算，但实测只快了 11%——因为真正的热点在别处。

**优化 2：重排循环里的 Jaccard 去掉哈希分配（关键）。**

第四步贪心重排里，每轮都要对每个未选候选算「它与所有已选商品的标签相似度」：

```
调用次数 = |C| × limit × |已选| ≈ 50 × 10 × 10 ≈ 5000 次
```

而 `jaccard` 每次调用都建**两个 `HashSet`**（两次堆分配 + 哈希全部标签）。标签集合只有 1~5 个元素，哈希表在这里是纯粹的负担。

于是新增零分配版本 `jaccard_no_alloc`：用 `O(|A|·|B|)` 的两两比较代替哈希，语义与 `jaccard` **逐位一致**（含去重、含空集返回 0）：

```rust
pub fn jaccard_no_alloc(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() { return 0.0; }
    let mut inter = 0usize;
    let mut distinct_a = 0usize;
    for (i, x) in a.iter().enumerate() {
        if a[..i].contains(x) { continue; }   // 去重
        distinct_a += 1;
        if b.contains(x) { inter += 1; }
    }
    let mut distinct_b = 0usize;
    for (i, y) in b.iter().enumerate() {
        if b[..i].contains(y) { continue; }
        distinct_b += 1;
    }
    if inter == 0 { return 0.0; }
    let union = distinct_a + distinct_b - inter;
    if union == 0 { 0.0 } else { inter as f64 / union as f64 }
}
```

**为什么这里不用「排序 + 双指针」？** 那需要输入已排序，而商品的 `tags` 来自 JSON、顺序不可控。为省分配去 `sort` 一遍反而更慢，且会打乱调用方的数据。`O(|A|·|B|)` 在 `|A|,|B| ≤ 5` 时最多 25 次字符串比较，比两次堆分配便宜得多。

这一项把 160.5 µs 打到 **39~44 µs**（多次运行 38.9/41.4/39.1 µs），**提速约 4 倍**。

**教训**：优化要看实测，不能靠直觉。第一反应是「相似用户列表重复计算」，确实是浪费，但只占 11%；真正的瓶颈是 5000 次小集合的哈希分配——这类开销在 profiler 里不显眼，却因为调用次数多而成为主导项。

**正确性保证**：`jaccard_no_alloc` 有两个测试盯着——`jaccard_no_alloc_matches_jaccard`（9 个手工边界 case）与 `jaccard_no_alloc_matches_jaccard_random`（300 组随机多重集，与 HashSet 版逐位比对，误差 < 1e-12）。

### 7.6 复杂度汇总

| 接口 | 时间 | 空间 |
|---|---|---|
| `new` | `O(N log N + U log U + E log E + ΣEᵤ·w)` | `O(N + U + \|counts\|)` |
| `recommend_for_user` | `O(\|C\| · (T + K·log\|C\|))` | `O(\|C\|)` |
| `similar_products` | `O(N·T + N log k)` | `O(k)` |
| `hot_ranking` | `O(N log k)` | `O(k)` |
| `query` | `O(\|C\| log\|C\|)` | `O(\|C\|)` |
| `explain` | `O(\|C\|)` | `O(1)` |

`|C|` 为候选集大小，`T` 为平均标签数，`K` 为 `SIMILAR_USER_K`。

---

## 8. 实测数据

**测试环境**：Windows，`cargo 1.99.1`，`--release`（`opt-level=3, lto="thin", codegen-units=1`），每个测量点跑 **7 次取中位数**。

**复现命令**：

```bash
cargo run -p campus-rec --release --example bench_sort
```

**数据生成**：固定种子 xorshift（可复现）。价格 `100~20000` 分、标签从 20 个里随机取 1~3 个、`created_at` 散布在一个月内、交互量随机——模拟真实校园二手数据。

### 8.1 主排序：三路快排 vs 标准库

`SortKey::Hot`（比较器含 `powf`，最贴近真实调用）：

| n | three_way_quick | std `sort_unstable_by` | top_k (堆) | 快排/标准库 | top_k/全排序 |
|---|---|---|---|---|---|
| 1,000 | 0.653 ms | 0.543 ms | 0.035 ms | 0.83× | **15.4×** |
| 10,000 | 9.593 ms | 8.185 ms | 0.292 ms | 0.85× | **28.0×** |
| 100,000 | 126.038 ms | 109.913 ms | 2.753 ms | 0.87× | **39.9×** |

**读法**：「快排/标准库 = 0.83×」意味着自研实现耗时是标准库的 0.83 倍，即**比标准库慢约 17%**。「top_k/全排序 = 39.9×」意味着堆方案比全排序**快约 40 倍**。

> 注：本文数字来自同一次 `--release` 运行（全部表格共用同一批数据）；同一台机器上重跑会有 ±10% 的抖动（0.83×~0.93× 都出现过），但**量级关系稳定**。

### 8.2 隔离比较器开销

上面的测试里比较器要算 `hot_score`（含 `powf`），开销远大于排序本身，会掩盖算法差异。这里用**纯整数比较**（`price_cents` + `id`），只看排序算法本身：

| n | three_way_quick | std `sort_unstable_by` | 快排/标准库 |
|---|---|---|---|
| 1,000 | 0.343 ms | 0.267 ms | 0.78× |
| 10,000 | 3.684 ms | 2.734 ms | 0.74× |
| 100,000 | 65.577 ms | 51.499 ms | 0.79× |

纯整数比较下差距更明显（0.74×~0.79×），说明**标准库的 pdqsort 启发式在比较廉价时优势更大**——它的「检测到已有序就提前返回」「检测到划分不均衡就切 heapsort」等探测逻辑，本身也有开销，只有在比较代价高时才被摊薄。

> 注意 1,000 与 100,000 两档的比值（0.79×/0.79×）比 10,000 档（0.70×）差，这是**缓存效应**：小规模全在 L2 里，大规模受内存带宽限制，而内存带宽对所有算法是公平的，会压缩相对差距。

### 8.3 重复键场景（三路划分的主场）

把价格压到 **5 个档位**，制造海量相等主键：

| n | three_way_quick | std `sort_unstable_by` | 快排/标准库 |
|---|---|---|---|
| 1,000 | 0.322 ms | 0.238 ms | 0.74× |
| 10,000 | 3.668 ms | 2.832 ms | 0.77× |
| 100,000 | 66.284 ms | 54.039 ms | 0.82× |

**注意**：三路划分的**理论优势**（重复键时降到 `O(n)`）没有在这组数据里体现，因为：

1. 主键只有 5 档，但 `compare_for` 的 tie-break 让「全序」仍然有 10 万个不同的键（`price` 相同则比 `created_at`，再相同则比 `id`）——**tie-break 破坏了重复键**，三路划分的相等段永远只有 1 个元素；
2. 标准库的 pdqsort 内部也做了三路划分（`partition_equal`），所以我们没有独占优势。

这是一个**诚实的负面结果**，值得记录：三路划分的优势只在「比较器本身不做 tie-break、允许大量真正相等的键」时才明显。本项目为了保证确定性必须做 tie-break，代价就是放弃了这项优化。**确定性和性能在这里是 trade-off，我们选了确定性。**

### 8.4 端到端：`sort_products`

| n | `sort_products(Hot)` |
|---|---|
| 1,000 | 0.657 ms |
| 10,000 | 9.078 ms |
| 100,000 | 127.726 ms |

与 §8.1 的 `three_way_quick` 几乎一致（差异在测量噪声内），说明 `compare_for` 闭包的开销可以忽略。

### 8.5 倒排索引 vs 线性扫描

单标签查询（命中约 5% 商品）：

| n | 倒排索引 | 线性扫描 | 加速比 |
|---|---|---|---|
| 1,000 | 0.0002 ms | 0.0050 ms | **25×** |
| 10,000 | 0.0002 ms | 0.1207 ms | **604×** |
| 100,000 | 0.0013 ms | 1.4802 ms | **1139×** |

**这是本文档里最亮眼的一组数字**：`n = 10⁵` 时倒排索引比线性扫描快 **1100 倍以上**。原因是倒排索引的复杂度是 `O(命中数)`，与 `n` 无关；线性扫描是 `O(n)`。数据量越大，差距越大（25× → 604× → 1139×，近似线性增长）。

> 绝对耗时已接近计时器分辨率，倍数抖动较大（不同次运行会跑出 25×~1350× 的区间），因此只给量级结论。

**注意**：这里的倒排查询是「只取 id 列表」，没有后续的商品详情组装。真实接口（`query`）还要排序和组装，绝对耗时会更长，但**相对优势不变**——因为排序的候选集大小就是 `O(命中数)`。

### 8.6 Top-K 的规模效应

从 §8.1 抽出 top_k 那一列：

| n | top_k | 全排序 | 加速比 |
|---|---|---|---|
| 1,000 | 0.035 ms | 0.543 ms | 15.4× |
| 10,000 | 0.292 ms | 8.185 ms | 28.0× |
| 100,000 | 2.753 ms | 109.913 ms | 39.9× |

**加速比随 `n` 增长**：`O(n log k)` vs `O(n log n)`，比值是 `log n / log k`。`k = 10` 时，`n` 每涨 10 倍，比值涨约 1.4 倍（实测 15.4 → 28.0 → 39.9，因为常数项和内存带宽的影响，不是严格的比例）。

**这说明**：`k` 固定时，`n` 越大，堆方案越划算。推荐场景 `n = 10⁵`、`k = 10` 时，堆比全排序快 **40 倍**——这就是为什么 `hot_ranking` 和 `similar_products` 都坚持用 `top_k_by` 而不是「全排序再截断」。

### 8.7 结论

| 算法 | 实测结论 |
|---|---|
| 三路快排 | 比标准库慢 10%~25%，差距来自 pdqsort 的启发式探测；教学价值 > 性能价值 |
| Top-K 堆 | 比全排序快 15~40 倍，`n` 越大优势越大；**推荐场景必用** |
| 倒排索引 | 比线性扫描快 25~1100 倍，`n` 越大优势越大；**搜索场景必用** |
| 共现窗口稀疏化 | 种子数据 76 条事件 → **57 对**共现（占理论上限 2.8%），稠密矩阵需 4096 格；**稀疏化是可行性的前提** |
| 零分配 Jaccard | 重排内循环的 Jaccard 去掉哈希分配后，`recommend_for_user` 从 160.5 µs 降到 39~44 µs（**约 4×**） |

---

## 9. 工程约定

### 9.1 确定性

所有涉及遍历 `HashMap` 的地方，输出前都**显式排序**：

- `tag_counts()` 按 `(count 降, tag 升)`；
- `neighbors()` 按 `(count 降, id 升)`；
- `similar_users()` 按 `(共同商品数降, uid 升)`；
- `profile_tags()` 按字典序；
- `cf_signal` 的 providers 按 `(共同数降, uid 升)` 取前 3。

**为什么？** `HashMap` 的迭代顺序由哈希种子决定，默认每次运行都不同（Rust 的 `RandomState` 用随机种子防哈希碰撞攻击）。不排序的话，同一份数据两次查询可能给出不同结果，前端出现「刷新顺序就变」，测试也无法断言。**确定性的可复现系统**比「可能快一点点」重要得多。

### 9.2 浮点比较

所有排序比较器用 `f64::total_cmp` 而不是 `partial_cmp().unwrap()`：

- `partial_cmp` 遇 `NaN` 返回 `None`，`unwrap` 会 panic；
- `total_cmp` 按 IEEE 754 位模式给出全序，`NaN` 排到末尾，服务不崩。

### 9.3 防御性边界

- `jaccard`：任一边为空返回 0（不返回 1）；
- `cosine`：零向量返回 0（方向未定义）；
- `hot_score`：`age < 0` 按 0 处理（防负指数 `NaN`）；
- `normalize_scores`：含 `NaN`/`Inf` 时不动（防脏数据扩散）；
- `price_fit`：分母 `max(..., 1)`（防除零）；
- `top_k`：`k = 0` 返回空，`k > n` 自动 clamp 到 `n`；
- `recommend_for_user`：`limit = 0` 返回空；用户不存在 → 冷启动而非 panic。

### 9.4 与 SPEC 的对应

`docs/SPEC.md` 第 3 节冻结的公开签名**全部保持原样**，未做任何修改。新增的都是 `pub` 辅助函数/常量/结构体（如 `three_way_quick_sort`、`top_k_by`、`intersect_all`、`Explanation`、`RecStats`、`explain`、`stats`），供 `mredis-core` 的 `cmd_rec.rs` 实现 `REC.*` 命令时使用。

**一处需要说明的差异**：任务描述里提到的 `SortKey::{TimeDesc, HotDesc, ConditionDesc}` 与冻结代码中的 `SortKey::{PriceAsc, PriceDesc, Newest, Hot}` 不一致。以**冻结代码**为准（签名不可改），`Newest` 对应 `TimeDesc` 的语义，`Hot` 对应 `HotDesc`；`ConditionDesc`（按成色排序）未在冻结枚举中，因此未实现。若后续需要按成色排序，可在 `SortKey` 中新增变体（属于新增，不破坏既有签名）。

### 9.5 开发中发现的两个真实缺陷（已修）

这两条不是「设计取舍」，而是**实现错误**，记录在此以免后人重踩：

1. **热度分恒为 0**（严重，已修）。`Recommender::new` 与 `record_event` 都没有维护 `Product` 的 `views`/`clicks`/`favs`，而种子数据里也没有这三个字段，导致 `hot_score` 分子恒为 0：热度榜全 `0.0000`、冷启动推荐全 0 分、`W_HOT = 0.15` 静默失效。修复方式见 §6.6（事件流回填 + 增量累加 + 共用 `apply_event_count`）。
2. **`explain` 为取单值构造整张 HashMap**（性能，已修）。初版 `self.hot_norms(now).get(&id)` 在单次查询里做了一次 `O(n)` 分配 + 哈希插入。改为 `hot_norm_of`（`O(n)` 扫描、只分配一个 `Vec`），并与批量路径共用 `hot_baseline` 保证数值一致。

两处都补了回归测试：`new_backfills_interaction_counts_from_events`、`new_backfill_makes_hot_scores_nonzero`、`record_event_increments_counts`、`record_event_changes_hot_ranking`、`explain_hot_norm_matches_hot_norms`。

**另一类问题不在本 crate 的代码里**（记录在 §5.5）：第一版种子数据的事件按「每小时一条」生成，用户行为间隔普遍超过 5 分钟共现窗口，76 条事件只配上 1 对共现，`W_CF = 0.35` 几乎不起作用（现权重 0.25）。**已通过重写种子数据的事件生成逻辑解决**（会话模型：一次会话 3~5 个行为、间隔 20~180 秒），重新生成后共现对数 1 → **57**，相邻事件间隔 51/68 落在 300 秒窗口内。`CO_WINDOW_SECS` 保持 300 未变——窗口语义是对的，错的是数据的时间分布。

### 9.6 性能优化的方法论

本项目做了两轮优化，过程本身就是一条结论：**优化必须靠测量，不能靠直觉**。

| 步骤 | 假设 | 实测 | 结论 |
|---|---|---|---|
| 优化 1 | 「`similar_users` 对每个候选都重算，肯定是瓶颈」 | 180.9 → 160.5 µs（**-11%**） | 假设成立但不是主因 |
| 优化 2 | 「重排里 5000 次 `jaccard`，每次两次堆分配」 | 160.5 → **39~44 µs**（**约 -75%**） | 这才是主因 |

优化 1 在复杂度上是实打实的浪费（`|C|` 次 `O(U·E)` 重复计算），但对 64 个商品的数据集只有 11% 的收益；优化 2 是「常数因子」问题——`O(|a|·|b|)` 的算法理论上比 `O(|a|+|b|)` 差，但因为集合只有 1~5 个元素且调用 5000 次，实测反而快约 4 倍。

**这正是课程里「复杂度分析 + 常数因子」两件事都要讲的原因**：渐近复杂度决定规模上限，常数因子决定实际手感。在小规模数据上，一个「理论更差但零分配」的算法可能完胜「理论最优但每次分配」的算法。

**做法建议**（对后续维护者）：

1. 先用 `--release` + `black_box` 跑基准，拿到数字；
2. 按调用次数 × 单次开销排序，不要按代码「看起来」的复杂度排序；
3. 优化后**必须**重跑正确性测试（本例中 `jaccard_no_alloc` 有 309 个对照 case 锁死语义）；
4. 把数字和判断写进文档——下一次改动时才知道有没有退化。

---

## 10. 参考文献

1. Sedgewick, R. *Algorithms in C, Parts 1-4*. Addison-Wesley, 1998.（三路快排 / Dutch National Flag）
2. Bentley, J. L., McIlroy, M. D. *Engineering a Sort Function*. Software—Practice and Experience, 1993.（median-of-3、小区间插排）
3. Peters, O. R. *Pattern-Defeating Quicksort*. 2021.（标准库 `sort_unstable` 的算法）
4. Manning, C. D., Raghavan, P., Schütze, H. *Introduction to Information Retrieval*. Cambridge, 2008.（倒排索引、Jaccard）
5. Leskovec, J., Rajaraman, A., Ullman, J. D. *Mining of Massive Datasets*. Cambridge, 2014.（协同过滤、共现矩阵）
6. Graham, P. *How Hacker News Ranking Really Works*. 2013.（重力衰减公式）
