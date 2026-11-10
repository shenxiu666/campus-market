//! 商品列表的筛选与排序（Web 层自己的实现）。
//!
//! 说明：`campus-rec` 里有一套三路快排，那是**算法演示**与 `REC.*` 的内部实现；
//! Web 的列表页需要额外支持「成色排序」这个契约里没有的键，且列表路径必须能
//! 独立工作（算法 crate 未就绪时 Web 也能跑），因此这里保留一份等价的比较逻辑。
//! 两者在 `price_asc` / `price_desc` / `time_desc` / `hot_desc` 上的语义完全一致：
//! 同键时按「创建时间降序 → id 升序」做确定性 tie-break。

use crate::error::ApiError;
use crate::model::ProductJson;
use std::cmp::Ordering;

/// 排序键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// 价格升序
    PriceAsc,
    /// 价格降序
    PriceDesc,
    /// 最新发布在前
    TimeDesc,
    /// 热度从高到低
    HotDesc,
    /// 成色从新到旧
    ConditionDesc,
}

impl SortOrder {
    /// 默认排序：最新。
    pub const DEFAULT: SortOrder = SortOrder::TimeDesc;

    /// 解析查询串里的 `sort` 值。
    pub fn parse(raw: &str) -> Option<SortOrder> {
        match raw {
            "price_asc" => Some(SortOrder::PriceAsc),
            "price_desc" => Some(SortOrder::PriceDesc),
            "time_desc" => Some(SortOrder::TimeDesc),
            "hot_desc" => Some(SortOrder::HotDesc),
            "condition_desc" => Some(SortOrder::ConditionDesc),
            _ => None,
        }
    }

    /// 契约里接受的全部取值（用于错误提示）。
    pub const ALL: [&'static str; 5] = [
        "price_asc",
        "price_desc",
        "time_desc",
        "hot_desc",
        "condition_desc",
    ];
}

/// 两个商品的主键比较（不含 tie-break）。
fn primary_cmp(a: &ProductJson, b: &ProductJson, order: SortOrder) -> Ordering {
    match order {
        SortOrder::PriceAsc => a.price_cents.cmp(&b.price_cents),
        SortOrder::PriceDesc => b.price_cents.cmp(&a.price_cents),
        SortOrder::TimeDesc => b.created_at.cmp(&a.created_at),
        SortOrder::HotDesc => b.hot.partial_cmp(&a.hot).unwrap_or(Ordering::Equal),
        SortOrder::ConditionDesc => b.condition.cmp(&a.condition),
    }
}

/// 完整比较：主键 → 创建时间降序 → id 升序。
pub fn compare(a: &ProductJson, b: &ProductJson, order: SortOrder) -> Ordering {
    primary_cmp(a, b, order)
        .then_with(|| b.created_at.cmp(&a.created_at))
        .then_with(|| a.id.cmp(&b.id))
}

/// 按 `order` 排序（稳定排序，保证同键输出确定）。
pub fn sort(items: &mut [ProductJson], order: SortOrder) {
    items.sort_by(|a, b| compare(a, b, order));
}

/// 列表查询条件。
#[derive(Debug, Clone, Default)]
pub struct ProductFilter {
    /// 必须全部命中的标签（AND 语义）
    pub tags: Vec<String>,
    /// 价格下限（分）
    pub min_price: Option<i64>,
    /// 价格上限（分）
    pub max_price: Option<i64>,
    /// 标题关键字（大小写不敏感的包含匹配）
    pub keyword: Option<String>,
    /// 排序键
    pub order: SortOrder,
}

impl Default for SortOrder {
    fn default() -> Self {
        SortOrder::DEFAULT
    }
}

impl ProductFilter {
    /// 单个商品是否满足筛选条件。
    pub fn matches(&self, product: &ProductJson) -> bool {
        if let Some(min) = self.min_price {
            if product.price_cents < min {
                return false;
            }
        }
        if let Some(max) = self.max_price {
            if product.price_cents > max {
                return false;
            }
        }
        if !self.tags.is_empty()
            && !self
                .tags
                .iter()
                .all(|tag| product.tags.iter().any(|t| t == tag))
        {
            return false;
        }
        if let Some(keyword) = &self.keyword {
            let keyword = keyword.to_lowercase();
            if !keyword.is_empty() && !product.title.to_lowercase().contains(&keyword) {
                return false;
            }
        }
        true
    }

    /// 应用筛选 + 排序，返回过滤后的副本。
    pub fn apply(&self, products: &[ProductJson]) -> Vec<ProductJson> {
        let mut out: Vec<ProductJson> = products
            .iter()
            .filter(|p| self.matches(p))
            .cloned()
            .collect();
        sort(&mut out, self.order);
        out
    }
}

/// 解析 `sort` 参数；空串走默认，非法值报 400。
pub fn parse_sort(raw: Option<&str>) -> Result<SortOrder, ApiError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(SortOrder::DEFAULT),
        Some(value) => SortOrder::parse(value).ok_or_else(|| {
            ApiError::bad_request(
                "sort",
                format!("sort 只能是 {} 之一", SortOrder::ALL.join("/")),
            )
        }),
    }
}

/// 解析价格参数：入参是**元**（可带小数），返回**分**。
pub fn parse_price(raw: Option<&str>, field: &str) -> Result<Option<i64>, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let yuan: f64 = raw
        .parse()
        .map_err(|_| ApiError::bad_request(field, format!("{field} 必须是数字（单位：元）")))?;
    if !yuan.is_finite() || yuan < 0.0 {
        return Err(ApiError::bad_request(field, format!("{field} 不能为负数")));
    }
    Ok(Some((yuan * 100.0).round() as i64))
}

/// 解析正整数参数（带上下限）。
pub fn parse_usize(
    raw: Option<&str>,
    field: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(default);
    };
    let value: usize = raw
        .parse()
        .map_err(|_| ApiError::bad_request(field, format!("{field} 必须是整数")))?;
    if value < min || value > max {
        return Err(ApiError::bad_request(
            field,
            format!("{field} 必须在 {min}..{max} 之间"),
        ));
    }
    Ok(value)
}

/// 解析 `tags` 参数（逗号分隔）。
pub fn parse_tags(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(
        id: u64,
        price: i64,
        created: i64,
        hot: f64,
        condition: u8,
        tags: &[&str],
    ) -> ProductJson {
        ProductJson {
            id,
            title: format!("商品{id}"),
            desc: String::new(),
            price_cents: price,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            seller_id: 1,
            condition,
            campus: "前湖校区".into(),
            created_at: created,
            status: "onsale".into(),
            views: 0,
            clicks: 0,
            favs: 0,
            hot,
        }
    }

    fn sample() -> Vec<ProductJson> {
        vec![
            product(1, 45000, 100, 0.2, 9, &["自行车", "运动"]),
            product(2, 1500, 300, 0.9, 7, &["教材"]),
            product(3, 45000, 200, 0.5, 8, &["自行车"]),
            product(4, 9000, 100, 0.1, 6, &[]),
        ]
    }

    #[test]
    fn parse_sort_accepts_known_values() {
        assert_eq!(parse_sort(Some("price_asc")).unwrap(), SortOrder::PriceAsc);
        assert_eq!(parse_sort(None).unwrap(), SortOrder::TimeDesc);
        assert_eq!(parse_sort(Some("")).unwrap(), SortOrder::TimeDesc);
        assert!(parse_sort(Some("nope")).is_err());
    }

    #[test]
    fn sort_by_price_asc_uses_created_at_tiebreak() {
        let mut items = sample();
        sort(&mut items, SortOrder::PriceAsc);
        // 1 和 3 都是 45000 分，按创建时间降序 → 3（created_at=200）在 1（100）前面
        assert_eq!(
            items.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![2, 4, 3, 1]
        );
    }

    #[test]
    fn sort_falls_back_to_id_asc_when_price_and_time_equal() {
        let mut items = vec![
            product(9, 1000, 500, 0.0, 8, &[]),
            product(5, 1000, 500, 0.0, 8, &[]),
            product(7, 1000, 500, 0.0, 8, &[]),
        ];
        sort(&mut items, SortOrder::PriceAsc);
        assert_eq!(
            items.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![5, 7, 9]
        );
    }

    #[test]
    fn sort_by_time_desc() {
        let mut items = sample();
        sort(&mut items, SortOrder::TimeDesc);
        assert_eq!(
            items.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![2, 3, 1, 4]
        );
    }

    #[test]
    fn sort_by_hot_desc() {
        let mut items = sample();
        sort(&mut items, SortOrder::HotDesc);
        assert_eq!(
            items.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![2, 3, 1, 4]
        );
    }

    #[test]
    fn filter_by_tags_is_and_semantics() {
        let filter = ProductFilter {
            tags: vec!["自行车".into(), "运动".into()],
            ..Default::default()
        };
        let out = filter.apply(&sample());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 1);
    }

    #[test]
    fn filter_by_price_range_and_keyword() {
        let filter = ProductFilter {
            min_price: Some(2000),
            max_price: Some(10000),
            ..Default::default()
        };
        assert_eq!(filter.apply(&sample()).len(), 1);

        let filter = ProductFilter {
            keyword: Some("商品3".into()),
            ..Default::default()
        };
        assert_eq!(filter.apply(&sample())[0].id, 3);
    }

    #[test]
    fn filter_keyword_is_case_insensitive() {
        let mut items = vec![product(1, 100, 1, 0.0, 5, &[])];
        items[0].title = "Giant MTB".into();
        let filter = ProductFilter {
            keyword: Some("giant".into()),
            ..Default::default()
        };
        assert_eq!(filter.apply(&items).len(), 1);
    }

    #[test]
    fn parse_price_converts_yuan_to_cents() {
        assert_eq!(parse_price(Some("450"), "min").unwrap(), Some(45000));
        assert_eq!(parse_price(Some("12.5"), "min").unwrap(), Some(1250));
        assert_eq!(parse_price(Some(""), "min").unwrap(), None);
        assert!(parse_price(Some("abc"), "min").is_err());
        assert!(parse_price(Some("-1"), "min").is_err());
    }

    #[test]
    fn parse_usize_enforces_bounds() {
        assert_eq!(parse_usize(None, "limit", 20, 1, 100).unwrap(), 20);
        assert_eq!(parse_usize(Some("50"), "limit", 20, 1, 100).unwrap(), 50);
        assert!(parse_usize(Some("0"), "limit", 20, 1, 100).is_err());
        assert!(parse_usize(Some("101"), "limit", 20, 1, 100).is_err());
        assert!(parse_usize(Some("x"), "limit", 20, 1, 100).is_err());
    }

    #[test]
    fn parse_tags_trims_and_drops_empties() {
        assert_eq!(
            parse_tags(Some(" 自行车 , 运动 ,, ")),
            vec!["自行车", "运动"]
        );
        assert!(parse_tags(None).is_empty());
    }
}
