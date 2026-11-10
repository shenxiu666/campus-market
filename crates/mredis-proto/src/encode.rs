//! RESP2 编码。

use crate::value::Resp;

/// 把 `resp` 追加编码进 `out`。
pub fn encode(resp: &Resp, out: &mut Vec<u8>) {
    match resp {
        Resp::Simple(s) => {
            out.push(b'+');
            out.extend_from_slice(s.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        Resp::Error(s) => {
            out.push(b'-');
            out.extend_from_slice(s.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        Resp::Int(i) => {
            out.push(b':');
            out.extend_from_slice(i.to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        Resp::Bulk(b) => {
            out.push(b'$');
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(b);
            out.extend_from_slice(b"\r\n");
        }
        Resp::Null => out.extend_from_slice(b"$-1\r\n"),
        Resp::Array(items) => {
            out.push(b'*');
            out.extend_from_slice(items.len().to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
            for item in items {
                encode(item, out);
            }
        }
    }
}

/// 把一条命令编码成 RESP 数组帧（客户端发送格式，AOF 也复用）。
pub fn encode_command(args: &[Vec<u8>], out: &mut Vec<u8>) {
    out.push(b'*');
    out.extend_from_slice(args.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for arg in args {
        out.push(b'$');
        out.extend_from_slice(arg.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_all_kinds() {
        let mut out = Vec::new();
        encode(
            &Resp::Array(vec![
                Resp::Simple("OK".into()),
                Resp::Int(42),
                Resp::Bulk(b"hi".to_vec()),
                Resp::Null,
                Resp::Error("ERR bad".into()),
            ]),
            &mut out,
        );
        assert_eq!(
            String::from_utf8_lossy(&out),
            "*5\r\n+OK\r\n:42\r\n$2\r\nhi\r\n$-1\r\n-ERR bad\r\n"
        );
    }

    #[test]
    fn encode_chinese_bulk_keeps_utf8_bytes() {
        let mut out = Vec::new();
        encode(&Resp::Bulk("自行车".as_bytes().to_vec()), &mut out);
        assert!(out.starts_with(b"$9\r\n"));
    }

    #[test]
    fn encode_command_matches_resp_array() {
        let mut out = Vec::new();
        encode_command(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()], &mut out);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n"
        );
    }
}
