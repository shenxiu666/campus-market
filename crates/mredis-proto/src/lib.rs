//! RESP2 协议编解码。
//!
//! 只依赖标准库：负责把 TCP 字节流切成命令，并把服务端结果编码成 RESP 帧。
//! 对齐 `redis/src/networking.c` 的语义：支持数组帧与 inline 命令两种输入形式。

mod decode;
mod encode;
mod value;

pub use decode::{Decoder, ProtoError};
pub use encode::{encode, encode_command};
pub use value::Resp;

/// RESP 协议硬上限，对齐 Redis 的实现约束。
pub const PROTO_INLINE_MAX_SIZE: usize = 1024 * 64;
pub const PROTO_MAX_BULK_LEN: usize = 512 * 1024 * 1024;
pub const PROTO_MAX_MULTIBULK_LEN: usize = 1024 * 1024;
