//! AOF 持久化：追加写 + 重放 + 重写。
//!
//! # 为什么 AOF 存的是「命令」而不是「快照」
//!
//! 快照（RDB）写入快、恢复快，但两次快照之间崩溃就会丢数据；AOF 每写一条命令
//! 就追加一帧，崩溃最多丢掉「最后一条还没刷盘的命令」。本项目是教学项目，
//! 用 AOF 能把「命令 → 磁盘 → 重放」这条链路讲清楚，代价（文件会越来越大）
//! 则用 `BGREWRITEAOF` 补上。
//!
//! # 文件格式
//!
//! 直接复用 RESP 的多批量数组帧：`*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n`。
//! 好处是**重放时可以直接用协议解码器**，不需要再写一套解析逻辑。
//!
//! # 尾部半帧容错
//!
//! 进程被 kill 时，最后一条命令可能只写了一半（比如只落了 `*3\r\n$3\r\nSET\r\n`）。
//! 重放时遇到 `ProtoError::Incomplete` 就停止，并把文件截断到「最后一个完整命令」
//! 的末尾——否则下次启动还会读到同一段垃圾，且新追加的命令会接在垃圾后面。

use crate::config::FsyncPolicy;
use mredis_proto::{Decoder, encode_command};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};

/// AOF 写入器。
///
/// 内部是 `BufWriter<File>`：命令先攒在用户态缓冲里，按策略 `flush`/`sync_all`。
/// `buf` 复用同一块 `Vec<u8>` 编码命令，避免每条命令都分配。
#[derive(Debug)]
pub struct Aof {
    /// 目标文件路径（重写时用来写临时文件再改名）
    path: String,
    /// 带缓冲的写入器
    writer: BufWriter<File>,
    /// 刷盘策略
    policy: FsyncPolicy,
    /// 已追加的命令条数
    cmd_count: u64,
    /// 已写入的字节数（含尚未 flush 的缓冲）
    size: u64,
    /// 上次刷盘时刻（Unix 毫秒），`everysec` 用
    last_flush_ms: i64,
    /// 编码复用缓冲
    buf: Vec<u8>,
}

impl Aof {
    /// 打开（不存在则创建）AOF 文件。
    ///
    /// 以 `append` 模式打开：即使文件里已有历史命令，新命令也接在末尾，
    /// 不会覆盖掉需要重放的内容。
    pub fn open(path: &str, policy: FsyncPolicy) -> io::Result<Aof> {
        if let Some(dir) = std::path::Path::new(path).parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Aof {
            path: path.to_string(),
            writer: BufWriter::new(file),
            policy,
            cmd_count: 0,
            size,
            last_flush_ms: 0,
            buf: Vec::with_capacity(128),
        })
    }

    /// 追加一条命令（RESP 数组帧）。
    ///
    /// # 写穿（write-through）
    ///
    /// 编码后立即 `flush`，把字节从 `BufWriter` 的用户态缓冲交给操作系统。
    /// 这样即使进程紧接着被强杀（`stop.ps1` 的兜底 `Kill()`、任务管理器、
    /// 崩溃），已经回给客户端的命令也不会留在用户态缓冲里凭空消失——
    /// 这是「重启后数据还在」的底线。
    ///
    /// 代价是每条写命令多一次系统调用；真正决定断电是否丢数据的是
    /// `fsync`（见 [`Self::sync`]），由 `appendfsync` 策略与后台线程控制。
    pub fn append(&mut self, args: &[Vec<u8>]) -> io::Result<()> {
        if args.is_empty() {
            return Ok(());
        }
        self.buf.clear();
        encode_command(args, &mut self.buf);
        self.writer.write_all(&self.buf)?;
        // 写穿：不能只交给 BufWriter，否则空闲时缓冲永不落盘。
        self.writer.flush()?;
        self.cmd_count += 1;
        self.size += self.buf.len() as u64;
        Ok(())
    }

    /// 按策略刷盘；`everysec` 内部判断距上次 fsync 是否超过 1 秒。
    pub fn maybe_flush(&mut self, now_ms: i64) -> io::Result<()> {
        match self.policy {
            // always：每条命令都 fsync，最安全也最慢。
            FsyncPolicy::Always => self.sync(),
            // everysec：最多丢 1 秒数据，性能与安全的折中（Redis 默认）。
            FsyncPolicy::EverySec => {
                if now_ms - self.last_flush_ms >= 1000 {
                    self.sync()
                } else {
                    Ok(())
                }
            }
            // no：交给操作系统，进程崩溃不丢、机器断电会丢。
            FsyncPolicy::No => Ok(()),
        }
    }

    /// 供后台定时线程调用：只做「该 fsync 了没有」的判断。
    ///
    /// 与 [`Self::maybe_flush`] 的区别是它不依赖「下一条命令到来」——
    /// 服务器空闲时也必须有人把缓冲刷到磁盘，否则 `everysec` 名不副实。
    pub fn sync_if_due(&mut self, now_ms: i64) -> io::Result<()> {
        match self.policy {
            // `append` 之后 `mark_dirty` 已经 sync 过，这里无需重复。
            FsyncPolicy::Always => Ok(()),
            FsyncPolicy::EverySec => {
                if now_ms - self.last_flush_ms >= 1000 {
                    self.sync()
                } else {
                    Ok(())
                }
            }
            FsyncPolicy::No => Ok(()),
        }
    }

    /// 把用户态缓冲推给操作系统（不保证落盘）。
    pub fn flush_buf(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    /// 把缓冲写到操作系统并 `fsync`。
    pub fn sync(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        // `sync_all` 会真正落盘（fsync）；失败时也把时间戳推进，
        // 避免每次命令都重复尝试一个大概率失败的 fsync 拖垮性能。
        let res = self.writer.get_ref().sync_all();
        self.last_flush_ms = crate::expire::now_ms();
        res
    }

    /// [`Self::sync`] 的旧名字，保留以兼容既有调用点。
    pub fn flush(&mut self) -> io::Result<()> {
        self.sync()
    }

    /// 当前文件大小（字节）。
    pub fn size(&self) -> u64 {
        self.size
    }

    /// 已追加的命令条数。
    pub fn cmd_count(&self) -> u64 {
        self.cmd_count
    }

    /// 刷盘策略。
    pub fn policy(&self) -> FsyncPolicy {
        self.policy
    }

    /// AOF 文件路径。
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 读取整个 AOF 文件为命令列表，用于启动重放。
    ///
    /// 返回 `(命令列表, 是否遇到损坏的尾部)`。遇到半帧时**不会**报错，
    /// 而是把文件截断到最后一个完整命令的末尾后返回 `true`。
    pub fn load(path: &str) -> io::Result<(Vec<Vec<Vec<u8>>>, bool)> {
        let mut file = match File::open(path) {
            Ok(f) => f,
            // 文件不存在等价于「没有历史命令」，不是错误。
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let mut decoder = Decoder::new();
        decoder.feed(&bytes);
        let mut cmds: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut good_end = 0usize;
        let mut truncated = false;

        loop {
            match decoder.next_command() {
                Ok(Some(cmd)) => {
                    // `buffered()` 是解码器里「还没消费的字节数」，
                    // 用总长减去它，就得到这条命令结束时的文件偏移。
                    let rest = decoder.buffered();
                    good_end = bytes.len() - rest;
                    if !cmd.is_empty() {
                        cmds.push(cmd);
                    }
                }
                Ok(None) => break,
                Err(mredis_proto::ProtoError::Incomplete) => break,
                Err(mredis_proto::ProtoError::Invalid(_)) => {
                    truncated = true;
                    break;
                }
            }
        }

        // 尾部有半帧或垃圾：截断到最后一个完整命令，让下次追加不会接在垃圾后面。
        if good_end < bytes.len() {
            truncated = true;
            let mut f = OpenOptions::new().write(true).open(path)?;
            f.set_len(good_end as u64)?;
            f.seek(SeekFrom::Start(good_end as u64))?;
            f.sync_all()?;
        }
        Ok((cmds, truncated))
    }

    /// 用一批命令重写 AOF（`BGREWRITEAOF` 的实现）。
    ///
    /// 先写到 `<path>.rewrite` 再原子 `rename`：即使重写到一半崩溃，
    /// 原文件依然完整可用，重启后不会读到半截的新文件。
    ///
    /// 调用方负责先 flush 旧文件，并把「重写期间新产生的命令」补写进来
    /// （本项目单线程执行命令，重写期间不会并发写入，因此直接整体替换即可）。
    pub fn rewrite(&mut self, cmds: &[Vec<Vec<u8>>]) -> io::Result<()> {
        let tmp = format!("{}.rewrite", self.path);
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            let mut buf = Vec::new();
            for cmd in cmds {
                buf.clear();
                encode_command(cmd, &mut buf);
                f.write_all(&buf)?;
            }
            f.sync_all()?;
        }
        self.writer.flush()?;
        std::fs::rename(&tmp, &self.path)?;
        // 重新打开，让写入器指向新文件。
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)?;
        self.writer = BufWriter::new(file);
        self.size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        self.cmd_count = cmds.len() as u64;
        self.last_flush_ms = crate::expire::now_ms();
        Ok(())
    }
}

/// 把当前键空间「翻译」成一串能重建它的命令，供 `BGREWRITEAOF` 使用。
///
/// 这是 AOF 重写的核心思想：文件里不需要保留命令的历史，只需要保留
/// 「用最少的命令还原当前状态」的结果。一条 `RPUSH k a b c` 就能代替
/// 历史上三次 `RPUSH`，文件体积随数据量而非操作次数增长。
///
/// TTL 用 `PEXPIREAT`（绝对时刻）而不是 `PEXPIRE`：重放可能发生在很久以后，
/// 相对时间会让所有键「重新开始倒计时」。
///
/// ## 为什么每个键前面都要先发一条 `DEL`
///
/// 重写出的 AOF 是一份**全量快照**，它表达的语义必须是「让这个键变成这样」，
/// 而不是「在这个键上再叠加点什么」。但很多写命令天生是**合并语义**：
///
/// * `RPUSH k a b c` 是追加到已有列表尾部，不是替换列表；
/// * `HSET k f v` 只覆盖指定字段，原有其他字段仍然保留；
/// * `SADD` / `ZADD` 只增不减。
///
/// 而重放发生在**种子基线之后**（见 [`crate::server::Server::bind`] 的加载顺序：
/// 先导入种子，再重放 AOF），此时键空间里已经有种子写进去的数据。
/// 若直接重放这些合并语义的命令，`user:<uid>:events` 这类列表就会翻倍——
/// 实测 76 条事件重放后变成 152 条，`REC.STATS.events` 直接爆掉。
///
/// 所以每个键先发一条 `DEL`，把语义拉回「覆盖」。代价是命令数翻倍，
/// 但 AOF 重写是低频操作，正确性远比这点开销重要。
///
/// 唯一例外是 `Str`：`SET` 本身就是覆盖语义，再发 `DEL` 纯属浪费。
pub fn snapshot_commands(db: &crate::db::Database) -> Vec<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(db.data.len() * 2);
    let mut ttl_cmds: Vec<Vec<Vec<u8>>> = Vec::new();

    for (key, obj) in db.data.iter() {
        let key = key.clone();
        match obj {
            crate::obj::Obj::Str(s) => {
                // `SET` 覆盖语义，无需 DEL。
                out.push(vec![b"SET".to_vec(), key.clone(), s.clone()]);
            }
            crate::obj::Obj::Hash(h) => {
                if h.is_empty() {
                    continue;
                }
                out.push(vec![b"DEL".to_vec(), key.clone()]);
                // 一次 HSET 写多个字段，比逐条 HSET 更紧凑。
                let mut cmd = vec![b"HSET".to_vec(), key.clone()];
                for (f, v) in h.iter() {
                    cmd.push(f.clone());
                    cmd.push(v.clone());
                }
                out.push(cmd);
            }
            crate::obj::Obj::List(l) => {
                if l.is_empty() {
                    continue;
                }
                out.push(vec![b"DEL".to_vec(), key.clone()]);
                let mut cmd = vec![b"RPUSH".to_vec(), key.clone()];
                cmd.extend(l.iter().cloned());
                out.push(cmd);
            }
            crate::obj::Obj::Set(s) => {
                if s.is_empty() {
                    continue;
                }
                out.push(vec![b"DEL".to_vec(), key.clone()]);
                let mut cmd = vec![b"SADD".to_vec(), key.clone()];
                cmd.extend(s.keys().cloned());
                out.push(cmd);
            }
            crate::obj::Obj::ZSet(z) => {
                if z.is_empty() {
                    continue;
                }
                out.push(vec![b"DEL".to_vec(), key.clone()]);
                let mut cmd = vec![b"ZADD".to_vec(), key.clone()];
                for (member, score) in z.iter() {
                    cmd.push(crate::db::format_float(score).into_bytes());
                    cmd.push(member.clone());
                }
                out.push(cmd);
            }
        }

        if let Some(&deadline) = db.expires.get(&key) {
            ttl_cmds.push(vec![
                b"PEXPIREAT".to_vec(),
                key,
                deadline.to_string().into_bytes(),
            ]);
        }
    }

    // TTL 统一放到最后：先建值再设过期，重放中途失败也不会丢掉数据。
    out.extend(ttl_cmds);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> String {
        crate::testutil::temp_file(&format!("aof-{name}"), "appendonly.aof")
    }

    fn cmd(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn append_then_load_roundtrip() {
        let path = tmp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        {
            let mut aof = Aof::open(&path, FsyncPolicy::Always).expect("打开");
            aof.append(&cmd(&["SET", "k", "v"])).expect("写");
            aof.append(&cmd(&["RPUSH", "l", "a", "b"])).expect("写");
            aof.flush().expect("刷盘");
            assert_eq!(aof.cmd_count(), 2);
            assert!(aof.size() > 0);
        }
        let (cmds, truncated) = Aof::load(&path).expect("读");
        assert!(!truncated);
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0][0], b"SET");
        assert_eq!(cmds[1][2], b"a");
    }

    #[test]
    fn load_missing_file_is_empty() {
        let path = tmp_path("missing");
        let _ = std::fs::remove_file(&path);
        let (cmds, truncated) = Aof::load(&path).expect("读");
        assert!(cmds.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn load_truncates_half_written_tail() {
        let path = tmp_path("halftail");
        let _ = std::fs::remove_file(&path);
        {
            let mut aof = Aof::open(&path, FsyncPolicy::Always).expect("打开");
            aof.append(&cmd(&["SET", "a", "1"])).expect("写");
            aof.flush().expect("刷盘");
        }
        // 手工追加半帧，模拟进程被 kill 时只写了半条命令。
        {
            let mut f = OpenOptions::new().append(true).open(&path).expect("打开");
            f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nb\r\n")
                .expect("写半帧");
            f.sync_all().expect("刷盘");
        }
        let (cmds, truncated) = Aof::load(&path).expect("读");
        assert!(truncated, "应报告尾部损坏");
        assert_eq!(cmds.len(), 1, "只保留完整的那条");
        // 文件已被截断到最后一个完整命令。
        let (again, truncated2) = Aof::load(&path).expect("再读");
        assert!(!truncated2, "截断后不应再报损坏");
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn everysec_policy_batches_flushes() {
        let path = tmp_path("everysec");
        let _ = std::fs::remove_file(&path);
        let mut aof = Aof::open(&path, FsyncPolicy::EverySec).expect("打开");
        aof.append(&cmd(&["SET", "k", "v"])).expect("写");
        // 首次 maybe_flush 距 last_flush_ms(0) 很远，会真的 fsync。
        aof.maybe_flush(10_000).expect("刷盘");
        aof.append(&cmd(&["SET", "k2", "v2"])).expect("写");
        // 同一秒内不再 fsync（但数据已经写穿到操作系统）。
        aof.maybe_flush(10_100).expect("跳过");
        aof.flush().expect("强制刷盘");
        let (cmds, _) = Aof::load(&path).expect("读");
        assert_eq!(cmds.len(), 2);
    }

    /// 写穿：`append` 之后不调用任何 flush/sync，数据也必须已经出现在
    /// 操作系统可见的文件里。否则空闲时强杀进程就会丢数据（曾经的 bug）。
    #[test]
    fn append_writes_through_without_explicit_flush() {
        let path = tmp_path("writethrough");
        let _ = std::fs::remove_file(&path);
        let mut aof = Aof::open(&path, FsyncPolicy::EverySec).expect("打开");
        aof.append(&cmd(&["SET", "k", "v"])).expect("写");
        let len = std::fs::metadata(&path).expect("metadata").len();
        assert!(len > 0, "append 后应立即写穿到操作系统");
        let (cmds, truncated) = Aof::load(&path).expect("读");
        assert!(!truncated);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0][0], b"SET");
    }

    /// 后台线程用的 `sync_if_due`：`everysec` 只在距上次 fsync ≥1s 时动手。
    #[test]
    fn sync_if_due_waits_one_second_for_everysec() {
        let path = tmp_path("syncdue");
        let _ = std::fs::remove_file(&path);
        let mut aof = Aof::open(&path, FsyncPolicy::EverySec).expect("打开");
        aof.append(&cmd(&["SET", "k", "v"])).expect("写");
        aof.sync_if_due(500).expect("未到期");
        assert_eq!(aof.last_flush_ms, 0, "未满 1 秒不应 fsync");
        aof.sync_if_due(1_500).expect("到期后 fsync");
        assert!(aof.last_flush_ms > 0, "到期后应更新 fsync 时间戳");
    }

    #[test]
    fn sync_if_due_never_fsyncs_under_no_policy() {
        let path = tmp_path("syncdue-no");
        let _ = std::fs::remove_file(&path);
        let mut aof = Aof::open(&path, FsyncPolicy::No).expect("打开");
        aof.append(&cmd(&["SET", "k", "v"])).expect("写");
        aof.sync_if_due(10_000).expect("no 策略不 fsync");
        assert_eq!(aof.last_flush_ms, 0);
    }

    #[test]
    fn no_policy_never_fsyncs_but_flush_writes() {
        let path = tmp_path("nopolicy");
        let _ = std::fs::remove_file(&path);
        let mut aof = Aof::open(&path, FsyncPolicy::No).expect("打开");
        aof.append(&cmd(&["INCR", "n"])).expect("写");
        aof.maybe_flush(1).expect("no 策略不刷盘");
        aof.flush().expect("强制刷盘");
        let (cmds, _) = Aof::load(&path).expect("读");
        assert_eq!(cmds.len(), 1);
    }

    #[test]
    fn rewrite_replaces_file_atomically() {
        let path = tmp_path("rewrite");
        let _ = std::fs::remove_file(&path);
        let mut aof = Aof::open(&path, FsyncPolicy::Always).expect("打开");
        for i in 0..50 {
            aof.append(&cmd(&["INCRBY", "n", "1"])).expect("写");
            let _ = i;
        }
        aof.flush().expect("刷盘");
        // 重写成单条 SET，体积应显著变小。
        aof.rewrite(&[cmd(&["SET", "n", "50"])]).expect("重写");
        assert_eq!(aof.cmd_count(), 1);
        let (cmds, truncated) = Aof::load(&path).expect("读");
        assert!(!truncated);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0][1], b"n");
        assert!(
            !std::path::Path::new(&format!("{path}.rewrite")).exists(),
            "临时文件应已删除"
        );
    }

    #[test]
    fn append_empty_command_is_noop() {
        let path = tmp_path("empty");
        let _ = std::fs::remove_file(&path);
        let mut aof = Aof::open(&path, FsyncPolicy::Always).expect("打开");
        aof.append(&[]).expect("空命令应被忽略");
        assert_eq!(aof.cmd_count(), 0);
    }
}
