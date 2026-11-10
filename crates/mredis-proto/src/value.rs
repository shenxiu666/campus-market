//! RESP2 值模型。

/// 一个 RESP2 帧。
///
/// `Null` 统一表示 `$-1\r\n` 与 `*-1\r\n` 两种空值；编码时按上下文输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resp {
    /// `+OK\r\n`
    Simple(String),
    /// `-ERR ...\r\n`
    Error(String),
    /// `:123\r\n`
    Int(i64),
    /// `$5\r\nhello\r\n`
    Bulk(Vec<u8>),
    /// `$-1\r\n`
    Null,
    /// `*2\r\n...`
    Array(Vec<Resp>),
}

impl Resp {
    /// `+OK`
    pub fn ok() -> Resp {
        Resp::Simple("OK".to_string())
    }

    /// `+<s>`
    pub fn simple(s: impl Into<String>) -> Resp {
        Resp::Simple(s.into())
    }

    /// `-ERR <s>`
    pub fn error(s: impl Into<String>) -> Resp {
        Resp::Error(s.into())
    }

    /// `$<len>`，入参按 UTF-8 字节写入。
    pub fn bulk(s: impl Into<Vec<u8>>) -> Resp {
        Resp::Bulk(s.into())
    }

    /// 把任意字节串按 UTF-8（有损）转成字符串，供 JSON 输出使用。
    pub fn as_string(&self) -> Option<String> {
        match self {
            Resp::Simple(s) | Resp::Error(s) => Some(s.clone()),
            Resp::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Resp::Int(i) => Some(i.to_string()),
            Resp::Null => None,
            Resp::Array(_) => None,
        }
    }

    /// 取整数值；`Bulk` 会尝试解析成整数（Redis 的 `INCR` 回包即 `:n`）。
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Resp::Int(i) => Some(*i),
            Resp::Bulk(b) => std::str::from_utf8(b).ok()?.parse().ok(),
            Resp::Simple(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// 是否为错误帧。
    pub fn is_error(&self) -> bool {
        matches!(self, Resp::Error(_))
    }
}
