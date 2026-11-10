//! # mredis-client —— mini-redis 的 RESP 客户端
//!
//! 一个极简但完整的 Redis 客户端：连接、发命令、读回包、pipeline。
//! `mredis-cli`、`mredis-web`、`mredis-seed` 和集成测试都基于它。

use mredis_proto::{ProtoError, Resp};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// 客户端错误。
#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Proto(ProtoError),
    /// 服务端返回了 `-ERR ...` 帧
    Server(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "io error: {e}"),
            ClientError::Proto(e) => write!(f, "protocol error: {e}"),
            ClientError::Server(msg) => write!(f, "server error: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

impl From<ProtoError> for ClientError {
    fn from(e: ProtoError) -> Self {
        ClientError::Proto(e)
    }
}

/// 同步客户端。
pub struct Client {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    pending: Vec<u8>,
}

impl Client {
    /// 连接服务端。
    pub fn connect(addr: &str) -> Result<Client, ClientError> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client {
            stream,
            reader,
            pending: Vec::new(),
        })
    }

    /// 发送一条命令并读取一个回包。
    pub fn call(&mut self, args: &[&str]) -> Result<Resp, ClientError> {
        let owned: Vec<Vec<u8>> = args.iter().map(|s| s.as_bytes().to_vec()).collect();
        self.call_bytes(&owned)
    }

    /// 发送二进制安全的命令。
    pub fn call_bytes(&mut self, args: &[Vec<u8>]) -> Result<Resp, ClientError> {
        let mut buf = Vec::new();
        mredis_proto::encode_command(args, &mut buf);
        self.stream.write_all(&buf)?;
        self.stream.flush()?;
        self.read_reply()
    }

    /// pipeline：一次发送多条命令，按顺序读回包。
    pub fn pipeline(&mut self, cmds: &[Vec<Vec<u8>>]) -> Result<Vec<Resp>, ClientError> {
        let mut buf = Vec::new();
        for cmd in cmds {
            mredis_proto::encode_command(cmd, &mut buf);
        }
        self.stream.write_all(&buf)?;
        self.stream.flush()?;
        let mut out = Vec::with_capacity(cmds.len());
        for _ in 0..cmds.len() {
            out.push(self.read_reply()?);
        }
        Ok(out)
    }

    /// 便捷方法：把回包当字符串列表返回（`Array<Bulk>` → `Vec<String>`）。
    pub fn call_strings(&mut self, args: &[&str]) -> Result<Vec<String>, ClientError> {
        let resp = self.call(args)?;
        Ok(to_strings(&resp))
    }

    /// 便捷方法：执行命令并把回包解析成 JSON。
    pub fn call_json(&mut self, args: &[&str]) -> Result<serde_json::Value, ClientError> {
        let resp = self.call(args)?;
        Ok(resp_to_json(&resp))
    }

    /// 从底层流读取一个完整 RESP 回包（递归解析嵌套数组）。
    fn read_reply(&mut self) -> Result<Resp, ClientError> {
        loop {
            match self.parse_one()? {
                Some(resp) => return Ok(resp),
                None => {
                    // 数据不够，再读一批
                    let mut chunk = [0u8; 8192];
                    let n = self.reader.read(&mut chunk)?;
                    if n == 0 {
                        return Err(ClientError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "connection closed by server",
                        )));
                    }
                    self.pending.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }

    fn parse_one(&mut self) -> Result<Option<Resp>, ClientError> {
        let (resp, consumed) = match parse_resp(&self.pending)? {
            Some(v) => v,
            None => return Ok(None),
        };
        self.pending.drain(..consumed);
        Ok(Some(resp))
    }

    /// 发送 inline 命令（用于测试 inline 解析）。
    pub fn call_inline(&mut self, line: &str) -> Result<Resp, ClientError> {
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\r\n")?;
        self.stream.flush()?;
        self.read_reply()
    }

    /// 逐行读取原始字节（调试用）。
    pub fn read_line_raw(&mut self) -> io::Result<String> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        Ok(line)
    }
}

/// 把 `Resp` 转成 `Vec<String>`：数组逐项转，标量包成单元素。
pub fn to_strings(resp: &Resp) -> Vec<String> {
    match resp {
        Resp::Array(items) => items.iter().filter_map(|i| i.as_string()).collect(),
        Resp::Null => Vec::new(),
        other => other.as_string().into_iter().collect(),
    }
}

/// 把 `Resp` 转成 JSON，便于 Web 层直接返回。
pub fn resp_to_json(resp: &Resp) -> serde_json::Value {
    match resp {
        Resp::Simple(s) | Resp::Error(s) => serde_json::Value::String(s.clone()),
        Resp::Int(i) => serde_json::Value::Number((*i).into()),
        Resp::Bulk(b) => serde_json::Value::String(String::from_utf8_lossy(b).into_owned()),
        Resp::Null => serde_json::Value::Null,
        Resp::Array(items) => serde_json::Value::Array(items.iter().map(resp_to_json).collect()),
    }
}

/// 解析一个 RESP 帧，返回 `(值, 消耗字节数)`；数据不足返回 `Ok(None)`。
fn parse_resp(buf: &[u8]) -> Result<Option<(Resp, usize)>, ClientError> {
    if buf.is_empty() {
        return Ok(None);
    }
    let (line, mut pos) = match read_line_at(buf, 0) {
        Some(v) => v,
        None => return Ok(None),
    };
    let Some((&prefix, rest)) = line.split_first() else {
        return Err(ClientError::Proto(ProtoError::Invalid("empty line".into())));
    };
    let text = String::from_utf8_lossy(rest).into_owned();
    match prefix {
        b'+' => Ok(Some((Resp::Simple(text), pos))),
        b'-' => Ok(Some((Resp::Error(text), pos))),
        b':' => {
            let n = text
                .parse::<i64>()
                .map_err(|_| ClientError::Proto(ProtoError::Invalid("bad integer".into())))?;
            Ok(Some((Resp::Int(n), pos)))
        }
        b'$' => {
            let len = text
                .parse::<i64>()
                .map_err(|_| ClientError::Proto(ProtoError::Invalid("bad bulk len".into())))?;
            if len < 0 {
                return Ok(Some((Resp::Null, pos)));
            }
            let len = len as usize;
            if buf.len() < pos + len + 2 {
                return Ok(None);
            }
            let body = buf[pos..pos + len].to_vec();
            pos += len + 2;
            Ok(Some((Resp::Bulk(body), pos)))
        }
        b'*' => {
            let count = text
                .parse::<i64>()
                .map_err(|_| ClientError::Proto(ProtoError::Invalid("bad array len".into())))?;
            if count < 0 {
                return Ok(Some((Resp::Null, pos)));
            }
            let count = count as usize;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                match parse_resp(&buf[pos..])? {
                    Some((item, used)) => {
                        pos += used;
                        items.push(item);
                    }
                    None => return Ok(None),
                }
            }
            Ok(Some((Resp::Array(items), pos)))
        }
        _ => Err(ClientError::Proto(ProtoError::Invalid(
            "unknown reply type".into(),
        ))),
    }
}

/// 从 `start` 开始读一行（不含 CRLF），返回 `(行, 下一位置)`。
fn read_line_at(buf: &[u8], start: usize) -> Option<(&[u8], usize)> {
    let mut i = start;
    while i < buf.len() {
        if buf[i] == b'\n' {
            let mut end = i;
            if end > start && buf[end - 1] == b'\r' {
                end -= 1;
            }
            return Some((&buf[start..end], i + 1));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_string() {
        let (resp, used) = parse_resp(b"+OK\r\n").unwrap().unwrap();
        assert_eq!(resp, Resp::Simple("OK".into()));
        assert_eq!(used, 5);
    }

    #[test]
    fn parse_nested_array() {
        let raw = b"*2\r\n*2\r\n$1\r\na\r\n:1\r\n$-1\r\n";
        let (resp, used) = parse_resp(raw).unwrap().unwrap();
        assert_eq!(used, raw.len());
        assert_eq!(
            resp,
            Resp::Array(vec![
                Resp::Array(vec![Resp::Bulk(b"a".to_vec()), Resp::Int(1)]),
                Resp::Null
            ])
        );
    }

    #[test]
    fn parse_incomplete_returns_none() {
        assert!(parse_resp(b"*2\r\n$3\r\nab").unwrap().is_none());
    }

    #[test]
    fn to_strings_flattens() {
        let r = Resp::Array(vec![Resp::Bulk(b"a".to_vec()), Resp::Int(2)]);
        assert_eq!(to_strings(&r), vec!["a".to_string(), "2".to_string()]);
    }

    #[test]
    fn resp_to_json_types() {
        let r = Resp::Array(vec![Resp::Int(1), Resp::Null, Resp::Simple("x".into())]);
        let j = resp_to_json(&r);
        assert_eq!(j[0], serde_json::json!(1));
        assert!(j[1].is_null());
        assert_eq!(j[2], serde_json::json!("x"));
    }
}
