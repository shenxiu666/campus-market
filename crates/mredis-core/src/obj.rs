//! 值对象：Redis 的五种基础类型。

use crate::dict::Dict;
use crate::zset::ZSet;
use std::collections::VecDeque;

/// 一个键对应的值。
#[derive(Debug)]
pub enum Obj {
    Str(Vec<u8>),
    Hash(Dict<Vec<u8>>),
    List(VecDeque<Vec<u8>>),
    Set(Dict<()>),
    ZSet(ZSet),
}

impl Obj {
    /// `TYPE` 命令返回值。
    pub fn type_name(&self) -> &'static str {
        match self {
            Obj::Str(_) => "string",
            Obj::Hash(_) => "hash",
            Obj::List(_) => "list",
            Obj::Set(_) => "set",
            Obj::ZSet(_) => "zset",
        }
    }

    /// 元素个数（字符串为字节长度）。
    pub fn len(&self) -> usize {
        match self {
            Obj::Str(s) => s.len(),
            Obj::Hash(h) => h.len(),
            Obj::List(l) => l.len(),
            Obj::Set(s) => s.len(),
            Obj::ZSet(z) => z.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `OBJECT ENCODING` 风格的编码名，用于 `INFO` 演示。
    pub fn encoding(&self) -> &'static str {
        match self {
            Obj::Str(_) => "raw",
            Obj::Hash(_) => "hashtable",
            Obj::List(_) => "deque",
            Obj::Set(_) => "hashtable",
            Obj::ZSet(_) => "skiplist",
        }
    }
}
