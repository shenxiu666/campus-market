//! RESP2 增量解码。
//!
//! 使用方式：`feed()` 追加字节，循环调用 `next_command()` 直到返回 `Ok(None)`。
//! 一次 `feed` 中可以包含多条命令（pipeline），也可以只包含半条命令——
//! 解析状态机保存在解码器内部，因此跨多次 `feed` 的半帧也能正确续上。

use crate::{PROTO_INLINE_MAX_SIZE, PROTO_MAX_BULK_LEN, PROTO_MAX_MULTIBULK_LEN};

/// 解码错误。`Incomplete` 不是错误，只是「数据还不够」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// 缓冲区里还没有完整的一帧，需要继续 `feed`。
    Incomplete,
    /// 协议违规，调用方应回复错误并关闭连接。
    Invalid(String),
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::Incomplete => write!(f, "incomplete frame"),
            ProtoError::Invalid(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// 解析状态机。
#[derive(Debug, Default)]
enum State {
    /// 等待一条新命令的开头
    #[default]
    Idle,
    /// 正在读取一个 multibulk 帧的各个 bulk 参数
    Args {
        remaining: i64,
        args: Vec<Vec<u8>>,
        /// 已读到 `$len` 头但正文还没到齐时，记录期望长度
        pending_len: Option<usize>,
    },
}

/// 增量 RESP 解码器。
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// 已消费到的位置，避免每帧都做 `drain` 搬移。
    pos: usize,
    state: State,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加新到达的字节。
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// 缓冲区中尚未消费的字节数。
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// 取出下一条命令；`Ok(None)` 表示数据不足，需要继续 `feed`。
    ///
    /// 返回的命令是一串二进制安全的参数，首元素为命令名（原样，未转大写）。
    pub fn next_command(&mut self) -> Result<Option<Vec<Vec<u8>>>, ProtoError> {
        match self.parse_command() {
            Ok(cmd) => {
                self.compact();
                Ok(Some(cmd))
            }
            Err(ProtoError::Incomplete) => {
                self.compact();
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn compact(&mut self) {
        if self.pos == 0 {
            return;
        }
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos > 8192 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }

    fn parse_command(&mut self) -> Result<Vec<Vec<u8>>, ProtoError> {
        loop {
            match std::mem::take(&mut self.state) {
                State::Idle => {
                    if self.buffered() == 0 {
                        return Err(ProtoError::Incomplete);
                    }
                    if self.buf[self.pos] != b'*' {
                        return self.parse_inline();
                    }
                    let line = self.read_line()?;
                    let count = parse_int(&line[1..], "invalid multibulk length")?;
                    if count <= 0 {
                        // Redis 对 `*0` / `*-1` 视为空命令，直接忽略继续读下一条。
                        return Ok(Vec::new());
                    }
                    if count as usize > PROTO_MAX_MULTIBULK_LEN {
                        return Err(ProtoError::Invalid("invalid multibulk length".into()));
                    }
                    self.state = State::Args {
                        remaining: count,
                        args: Vec::with_capacity(count as usize),
                        pending_len: None,
                    };
                }
                State::Args {
                    mut remaining,
                    mut args,
                    pending_len,
                } => {
                    let len = match pending_len {
                        Some(len) => len,
                        None => {
                            if self.buffered() == 0 {
                                self.state = State::Args {
                                    remaining,
                                    args,
                                    pending_len: None,
                                };
                                return Err(ProtoError::Incomplete);
                            }
                            if self.buf[self.pos] != b'$' {
                                return Err(ProtoError::Invalid(
                                    "expected '$', got something else".into(),
                                ));
                            }
                            let line = match self.read_line() {
                                Ok(l) => l,
                                Err(e) => {
                                    self.state = State::Args {
                                        remaining,
                                        args,
                                        pending_len: None,
                                    };
                                    return Err(e);
                                }
                            };
                            let parsed = match parse_int(&line[1..], "invalid bulk length") {
                                Ok(v) => v,
                                Err(e) => {
                                    self.state = State::Args {
                                        remaining,
                                        args,
                                        pending_len: None,
                                    };
                                    return Err(e);
                                }
                            };
                            if parsed < 0 || parsed as usize > PROTO_MAX_BULK_LEN {
                                return Err(ProtoError::Invalid("invalid bulk length".into()));
                            }
                            parsed as usize
                        }
                    };
                    // 需要 len 字节正文 + 结尾 CRLF。
                    if self.buffered() < len + 2 {
                        self.state = State::Args {
                            remaining,
                            args,
                            pending_len: Some(len),
                        };
                        return Err(ProtoError::Incomplete);
                    }
                    let start = self.pos;
                    let body = self.buf[start..start + len].to_vec();
                    if &self.buf[start + len..start + len + 2] != b"\r\n" {
                        return Err(ProtoError::Invalid(
                            "bulk payload not terminated by CRLF".into(),
                        ));
                    }
                    self.pos = start + len + 2;
                    args.push(body);
                    remaining -= 1;
                    if remaining == 0 {
                        return Ok(args);
                    }
                    self.state = State::Args {
                        remaining,
                        args,
                        pending_len: None,
                    };
                }
            }
        }
    }

    /// inline 命令：一行以空格分隔的参数，行尾 `\r\n` 或 `\n`。
    fn parse_inline(&mut self) -> Result<Vec<Vec<u8>>, ProtoError> {
        let start = self.pos;
        let mut end = None;
        let mut i = start;
        while i < self.buf.len() {
            if self.buf[i] == b'\n' {
                end = Some(i);
                break;
            }
            i += 1;
        }
        let Some(end) = end else {
            if self.buffered() > PROTO_INLINE_MAX_SIZE {
                return Err(ProtoError::Invalid("too big inline request".into()));
            }
            return Err(ProtoError::Incomplete);
        };
        let mut line_end = end;
        if line_end > start && self.buf[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        let line = self.buf[start..line_end].to_vec();
        self.pos = end + 1;
        if line.len() > PROTO_INLINE_MAX_SIZE {
            return Err(ProtoError::Invalid("too big inline request".into()));
        }
        Ok(split_inline(&line))
    }

    /// 读取到下一个 `\n`（含），返回不含行尾的行内容；不足则 `Incomplete`。
    fn read_line(&mut self) -> Result<Vec<u8>, ProtoError> {
        let start = self.pos;
        let mut i = start;
        while i < self.buf.len() {
            if self.buf[i] == b'\n' {
                let mut line_end = i;
                if line_end > start && self.buf[line_end - 1] == b'\r' {
                    line_end -= 1;
                }
                let line = self.buf[start..line_end].to_vec();
                self.pos = i + 1;
                return Ok(line);
            }
            i += 1;
        }
        if self.buffered() > PROTO_INLINE_MAX_SIZE {
            return Err(ProtoError::Invalid("too big inline request".into()));
        }
        Err(ProtoError::Incomplete)
    }
}

fn parse_int(bytes: &[u8], msg: &str) -> Result<i64, ProtoError> {
    let s = std::str::from_utf8(bytes).map_err(|_| ProtoError::Invalid(msg.to_string()))?;
    s.parse::<i64>()
        .map_err(|_| ProtoError::Invalid(msg.to_string()))
}

/// 按空白切分 inline 命令，支持 Redis 的 `"quoted string"` 与 `\x` 转义。
fn split_inline(line: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = line.len();
    while i < n {
        while i < n && line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let mut arg = Vec::new();
        if line[i] == b'"' {
            i += 1;
            while i < n {
                match line[i] {
                    b'"' => {
                        i += 1;
                        break;
                    }
                    b'\\' if i + 1 < n => {
                        i += 1;
                        arg.push(match line[i] {
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            b'b' => 0x08,
                            b'a' => 0x07,
                            b'x' if i + 2 < n => {
                                let hex = std::str::from_utf8(&line[i + 1..i + 3]).ok();
                                let byte = hex
                                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                                    .unwrap_or(b'x');
                                i += 2;
                                byte
                            }
                            other => other,
                        });
                        i += 1;
                    }
                    c => {
                        arg.push(c);
                        i += 1;
                    }
                }
            }
        } else if line[i] == b'\'' {
            i += 1;
            while i < n {
                match line[i] {
                    b'\'' => {
                        i += 1;
                        break;
                    }
                    b'\\' if i + 1 < n && line[i + 1] == b'\'' => {
                        arg.push(b'\'');
                        i += 2;
                    }
                    c => {
                        arg.push(c);
                        i += 1;
                    }
                }
            }
        } else {
            while i < n && !line[i].is_ascii_whitespace() {
                arg.push(line[i]);
                i += 1;
            }
        }
        out.push(arg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(args: &[&str]) -> Vec<Vec<u8>> {
        args.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn parse_single_multibulk() {
        let mut d = Decoder::new();
        d.feed(b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n");
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["GET", "key"])));
        assert_eq!(d.next_command().unwrap(), None);
    }

    #[test]
    fn parse_pipeline_of_three() {
        let mut d = Decoder::new();
        d.feed(b"*1\r\n$4\r\nPING\r\n*2\r\n$3\r\nGET\r\n$1\r\nk\r\n*1\r\n$4\r\nPING\r\n");
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["PING"])));
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["GET", "k"])));
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["PING"])));
        assert_eq!(d.next_command().unwrap(), None);
    }

    #[test]
    fn parse_split_across_feeds() {
        let mut d = Decoder::new();
        d.feed(b"*3\r\n$3\r\nSET\r\n$1");
        assert_eq!(d.next_command().unwrap(), None);
        d.feed(b"\r\nk\r\n$3\r\nval");
        assert_eq!(d.next_command().unwrap(), None);
        d.feed(b"\r\n");
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["SET", "k", "val"])));
    }

    #[test]
    fn parse_split_inside_header() {
        let mut d = Decoder::new();
        d.feed(b"*2\r\n$3\r\nGET");
        assert_eq!(d.next_command().unwrap(), None);
        d.feed(b"\r\n$3\r\nke");
        assert_eq!(d.next_command().unwrap(), None);
        d.feed(b"y\r\n");
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["GET", "key"])));
    }

    #[test]
    fn parse_byte_at_a_time() {
        let raw = b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n";
        let mut d = Decoder::new();
        let mut got = None;
        for b in raw {
            d.feed(&[*b]);
            if let Some(c) = d.next_command().unwrap() {
                got = Some(c);
            }
        }
        assert_eq!(got, Some(cmd(&["GET", "key"])));
    }

    #[test]
    fn parse_inline() {
        let mut d = Decoder::new();
        d.feed(b"PING\r\nGET key\r\n");
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["PING"])));
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["GET", "key"])));
    }

    #[test]
    fn parse_inline_quoted() {
        let mut d = Decoder::new();
        d.feed(b"SET k \"hello world\"\r\n");
        assert_eq!(
            d.next_command().unwrap(),
            Some(cmd(&["SET", "k", "hello world"]))
        );
    }

    #[test]
    fn parse_empty_multibulk_is_skipped() {
        let mut d = Decoder::new();
        d.feed(b"*0\r\n*1\r\n$4\r\nPING\r\n");
        assert_eq!(d.next_command().unwrap(), Some(vec![]));
        assert_eq!(d.next_command().unwrap(), Some(cmd(&["PING"])));
    }

    #[test]
    fn chinese_payload_roundtrip() {
        let payload = "自行车".as_bytes();
        let mut raw = Vec::new();
        raw.extend_from_slice(format!("*2\r\n$3\r\nGET\r\n${}\r\n", payload.len()).as_bytes());
        raw.extend_from_slice(payload);
        raw.extend_from_slice(b"\r\n");
        let mut d = Decoder::new();
        d.feed(&raw);
        let got = d.next_command().unwrap().unwrap();
        assert_eq!(got[1], payload);
    }

    #[test]
    fn reject_bad_prefix() {
        let mut d = Decoder::new();
        d.feed(b"*2\r\n+PING\r\n$1\r\nk\r\n");
        assert!(matches!(d.next_command(), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn reject_oversized_bulk() {
        let mut d = Decoder::new();
        d.feed(b"*1\r\n$999999999999\r\n");
        assert!(matches!(d.next_command(), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn reject_bad_length_text() {
        let mut d = Decoder::new();
        d.feed(b"*abc\r\n");
        assert!(matches!(d.next_command(), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn bulk_not_terminated_by_crlf() {
        let mut d = Decoder::new();
        d.feed(b"*1\r\n$2\r\nabXX");
        assert!(matches!(d.next_command(), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn many_commands_no_buffer_growth() {
        let mut d = Decoder::new();
        for _ in 0..1000 {
            d.feed(b"*1\r\n$4\r\nPING\r\n");
            assert!(d.next_command().unwrap().is_some());
        }
        assert_eq!(d.buffered(), 0);
    }
}
