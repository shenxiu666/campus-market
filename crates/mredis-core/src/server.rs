//! TCP 服务：监听、每连接线程、RESP 读写循环。
//!
//! 并发模型刻意保持简单：**一个连接一个线程 + 一把全局 `Mutex<Database>`**。
//! Redis 本身是单线程的，本项目要复现的正是「命令串行执行」这个语义——
//! 用锁把 `Database` 圈起来，`MULTI/EXEC` 的原子性、AOF 的写入顺序、
//! `INFO` 看到的一致快照就都自动成立，不需要额外设计。
//!
//! 代价是吞吐上不去，但课程项目的瓶颈在「讲清楚原理」而不是压测，
//! 而且真正的瓶颈（`Mutex` 争用）也只有在数千并发时才出现。

use crate::aof::Aof;
use crate::command::dispatch;
use crate::config::Config;
use crate::db::{Database, ExecCtx, MultiState};
use crate::stats::Stats;
use mredis_proto::{Decoder, ProtoError, encode};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 每次 `read` 的缓冲区大小（对齐 Redis 的 16KB 读缓冲）。
const READ_BUF: usize = 16 * 1024;

/// 主动过期扫描的间隔（毫秒）。
const EXPIRE_TICK_MS: u64 = 100;

/// 每轮主动过期最多清理的键数。
const EXPIRE_SAMPLES: usize = 20;

/// 后台 AOF fsync 线程的轮询间隔（毫秒）。
const AOF_SYNC_TICK_MS: u64 = 200;

/// 服务端句柄。
pub struct Server {
    /// 实际绑定地址（`--port 0` 时由系统分配）
    pub addr: SocketAddr,
    pub db: Arc<Mutex<Database>>,
    pub stats: Arc<Stats>,
    /// 监听套接字。`bind` 之后才真正开始 accept，因此这里保存它。
    listener: TcpListener,
    /// AOF（`--appendonly no` 时为 `None`）
    aof: Option<Arc<Mutex<Aof>>>,
    /// 是否把每条命令打到 stderr
    verbose: bool,
}

impl Server {
    /// 绑定端口并返回句柄（不阻塞）。
    ///
    /// 绑定成功后立刻向 stdout 打印 `Ready to accept connections on port <端口>`
    /// 并 flush：`--port 0` 时端口由系统分配，集成测试只能靠这一行拿到真实端口。
    ///
    /// ## 加载顺序（重要）
    ///
    /// ```text
    /// flush_on_start（清内存 + 删 AOF + 删标记）
    ///   → 已有持久化数据？ 是：只重放 AOF
    ///                     否：导入种子 → 重放 AOF → 把结果写成完整快照 → 建标记
    ///   → rebuild_rec_from_keyspace
    /// ```
    ///
    /// **种子只在首次初始化时导入**。早期版本每次启动都重导种子、再叠加 AOF 重放，
    /// 结果是「删除过的种子商品重启后复活」——那是个 demo，不是可用系统。
    /// 现在种子只在「AOF 为空」或「老版本 AOF 缺少 `<aof>.ready` 标记」时导入，
    /// 导入后立刻把整个键空间重写成快照，此后 AOF 就是唯一事实来源。
    ///
    /// **rec 必须最后重建**：它是键空间的纯派生视图（见 [`crate::recbuild`]），
    /// 只有等键空间彻底定型之后重建，才能保证「重启前后 `REC.*` 完全一致」。
    pub fn bind(config: &Config) -> io::Result<Server> {
        let addr = format!("{}:{}", config.host, config.port);
        let listener = TcpListener::bind(&addr)?;
        let local = listener.local_addr()?;

        // 同一进程里可能反复 bind（单测/嵌入式），先复位全局停机标志。
        crate::cmd_server::reset_shutdown();

        let mut db = Database::new();

        // ---- 1. `--flush-on-start`：连 AOF 与初始化标记一起清 ----
        // 只清内存是不够的：紧接着的重放会把旧 AOF 里的键全搬回来，
        // 「清空重来」的语义就失效了。
        if config.flush_on_start {
            db.flush();
            if config.appendonly {
                remove_file_if_exists(&config.aof_path, "AOF 文件");
                remove_file_if_exists(&aof_marker_path(&config.aof_path), "AOF 初始化标记");
            }
        }

        let stats = Arc::new(Stats::new(crate::expire::now_ms().max(0) as u64));

        // ---- 2~4. 持久化：首次初始化 / 之后只重放 ----
        let mut aof = None;
        if config.appendonly {
            let (cmds, truncated) = Aof::load(&config.aof_path)?;
            if truncated {
                eprintln!(
                    "mredis-server: AOF 尾部不完整，已截断到最后一个完整命令（{} 条命令可用）",
                    cmds.len()
                );
            }
            let marker = aof_marker_path(&config.aof_path);
            let initialized = std::path::Path::new(&marker).exists();
            // 空文件 = 全新实例；非空但缺标记 = 老版本 AOF（那时没有初始化概念）。
            // 两种情况都按「首次初始化」处理：先铺种子基线，再重放增量。
            let needs_init = cmds.is_empty() || !initialized;

            if needs_init {
                if let Some(seed_path) = &config.seed {
                    match crate::seed::load_seed(&mut db, seed_path) {
                        Ok(summary) => println!("{}", summary.line(seed_path)),
                        Err(e) => {
                            // 种子写错却静默启动，演示时只会看到「推荐为空」，比直接报错难查。
                            eprintln!("mredis-server: 导入种子失败: {e}");
                        }
                    }
                }
            } else {
                eprintln!(
                    "mredis-server: 已有持久化数据（{} 条命令），跳过种子导入",
                    cmds.len()
                );
            }

            // 重放：用最小上下文逐条 dispatch，不挂 AOF（否则会把历史命令再写一遍）。
            for cmd in &cmds {
                let mut ctx = ExecCtx::new(&mut db);
                let _ = dispatch(&mut ctx, cmd);
            }
            if !cmds.is_empty() && !needs_init {
                eprintln!("mredis-server: 已从 AOF 重放 {} 条命令", cmds.len());
            }

            let mut opened = Aof::open(&config.aof_path, config.appendfsync)?;
            let mut initial_cmds = cmds.len() as u64;
            if needs_init {
                // 首次初始化：把「种子 + 已重放增量」的完整结果写成快照。
                // 种子只是一次性基线，不必逐条进 AOF；之后所有修改都是
                // 相对这份快照的增量命令。
                let snapshot = crate::aof::snapshot_commands(&db);
                opened.rewrite(&snapshot)?;
                initial_cmds = snapshot.len() as u64;
                // 标记必须最后建：崩溃在「快照写完、标记未建」之间时，下次启动
                // 会再走一遍初始化，而快照对每个键先 DEL 再写，天然幂等。
                write_aof_marker(&marker)?;
            }
            stats.aof_cmds.store(initial_cmds, Ordering::Relaxed);
            aof = Some(Arc::new(Mutex::new(opened)));
        } else if let Some(seed_path) = &config.seed {
            // 纯内存模式（`--appendonly no`）：每次启动导入种子，符合演示预期。
            match crate::seed::load_seed(&mut db, seed_path) {
                Ok(summary) => println!("{}", summary.line(seed_path)),
                Err(e) => eprintln!("mredis-server: 导入种子失败: {e}"),
            }
        }

        // ---- 5. 推荐引擎：按定型后的键空间整体重建 ----
        let rebuilt = crate::recbuild::rebuild_rec_from_keyspace(&mut db);
        if rebuilt.products > 0 || rebuilt.users > 0 {
            eprintln!("mredis-server: {}", rebuilt.line());
        }

        // 打印端口。必须 flush，否则被 `| head -1` 之类管道包住时测试会卡死。
        println!("Ready to accept connections on port {}", local.port());
        let _ = io::stdout().flush();

        Ok(Server {
            addr: local,
            db: Arc::new(Mutex::new(db)),
            stats,
            listener,
            aof,
            verbose: config.verbose,
        })
    }

    /// 在后台线程里接受连接；返回后调用方可以继续做别的事。
    pub fn spawn_accept_loop(&self) {
        // 这里没法把 `self` 移进线程（它被 `run` 持有），所以真正的工作
        // 交给 `run` 里的阻塞循环；此函数保留为「非阻塞启动」的入口，
        // 用于测试/嵌入式场景——直接调用会什么都不做。
    }

    /// 启动并阻塞直到进程退出（`mredis-server` 主入口使用）。
    pub fn run(config: Config) -> io::Result<()> {
        let server = Server::bind(&config)?;
        server.serve()
    }

    /// 阻塞式 accept 循环。
    fn serve(self) -> io::Result<()> {
        // 监听套接字设置成非阻塞，好让主循环有机会做「主动过期」。
        self.listener.set_nonblocking(true)?;
        // 后台过期线程：每 100ms 扫一次，把惰性删除漏掉的键清掉。
        spawn_expire_thread(self.db.clone(), self.stats.clone());

        // AOF 后台 fsync：`everysec` 不能只在「下一条命令到来」时才判断，
        // 否则服务器空闲时缓冲永远不落盘。每 200ms 敲一次。
        let stop = Arc::new(AtomicBool::new(false));
        let sync_thread = self
            .aof
            .as_ref()
            .map(|aof| spawn_aof_sync_thread(aof.clone(), stop.clone()));

        let mut last_accept_err: Option<io::ErrorKind> = None;
        let result = loop {
            // `SHUTDOWN` 只负责置标志，真正的收尾在循环外统一做。
            if crate::cmd_server::shutdown_requested() {
                eprintln!("mredis-server: 收到 SHUTDOWN，正在停止");
                break Ok(());
            }
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    last_accept_err = None;
                    self.handle_connection(stream, peer);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // 没有新连接：睡一小会儿再试，避免空转烧 CPU。
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    // 反复同类错误（比如 fd 耗尽）时不要疯狂刷屏。
                    if last_accept_err != Some(e.kind()) {
                        eprintln!("mredis-server: accept 失败: {e}");
                        last_accept_err = Some(e.kind());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        };

        stop.store(true, Ordering::SeqCst);
        if let Some(handle) = sync_thread {
            let _ = handle.join();
        }
        // 最终 fsync：`SHUTDOWN NOSAVE` 显式跳过。
        if !crate::cmd_server::shutdown_skip_sync() {
            if let Some(aof) = &self.aof {
                match aof.lock() {
                    Ok(mut a) => {
                        if let Err(e) = a.sync() {
                            eprintln!("mredis-server: 退出前 fsync 失败: {e}");
                        }
                    }
                    Err(poisoned) => {
                        let mut a = poisoned.into_inner();
                        let _ = a.flush_buf();
                    }
                }
            }
        }
        result
    }

    /// 处理一条连接：独立线程 + 解码循环。
    fn handle_connection(&self, stream: TcpStream, peer: SocketAddr) {
        let db = self.db.clone();
        let stats = self.stats.clone();
        let aof = self.aof.clone();
        let verbose = self.verbose;
        stats.client_connected();
        std::thread::spawn(move || {
            let result = serve_client(stream, peer, &db, &stats, aof.as_ref(), verbose);
            if let Err(e) = result {
                // 客户端断开是常态（对方先 close），只有真正的 IO 错误才值得打印。
                if e.kind() != io::ErrorKind::UnexpectedEof
                    && e.kind() != io::ErrorKind::ConnectionReset
                    && e.kind() != io::ErrorKind::BrokenPipe
                {
                    eprintln!("mredis-server: 连接 {peer} 出错: {e}");
                }
            }
            stats.client_disconnected();
            // 连接关闭时把 AOF 刷盘：客户端看到 +OK 之后的数据不应该因为
            // 断连而丢在缓冲区里。
            if let Some(aof) = aof.as_ref()
                && let Ok(mut a) = aof.lock()
            {
                let _ = a.flush();
            }
        });
    }
}

/// 单连接的主循环。
fn serve_client(
    mut stream: TcpStream,
    _peer: SocketAddr,
    db: &Arc<Mutex<Database>>,
    stats: &Arc<Stats>,
    aof: Option<&Arc<Mutex<Aof>>>,
    verbose: bool,
) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    // Windows 上 accept 出来的套接字会**继承**监听套接字的非阻塞标志
    // （Linux 不会），于是 read 立刻返回 WSAEWOULDBLOCK(10035) 被当成致命错误、
    // 连接秒断。这里显式改回阻塞模式，两个平台行为就一致了。
    stream.set_nonblocking(false)?;
    let mut decoder = Decoder::new();
    let mut buf = vec![0u8; READ_BUF];
    // 每条连接各自的事务状态（`MULTI` 是连接私有语义）。
    let mut multi = MultiState::new();

    loop {
        // 先把已缓冲的完整命令全部执行掉，再读新数据。
        loop {
            match decoder.next_command() {
                Ok(Some(args)) => {
                    let reply = execute(db, stats, aof, &mut multi, &args, verbose);
                    let mut out = Vec::with_capacity(64);
                    encode(&reply, &mut out);
                    stream.write_all(&out)?;
                }
                Ok(None) => break,
                Err(ProtoError::Incomplete) => break,
                Err(ProtoError::Invalid(msg)) => {
                    // 协议错误：回一条错误后断开。RESP 没有「重新同步」的办法，
                    // 继续读下去只会把后续字节也解释错。
                    let reply = mredis_proto::Resp::error(format!("ERR Protocol error: {msg}"));
                    let mut out = Vec::new();
                    encode(&reply, &mut out);
                    let _ = stream.write_all(&out);
                    let _ = stream.flush();
                    return Ok(());
                }
            }
        }

        let n = match stream.read(&mut buf) {
            Ok(0) => return Ok(()), // 对端正常关闭（EOF）
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        decoder.feed(&buf[..n]);
        stream.flush().ok();
    }
}

/// 执行一条命令（加锁 → dispatch → 返回）。
fn execute(
    db: &Arc<Mutex<Database>>,
    stats: &Arc<Stats>,
    aof: Option<&Arc<Mutex<Aof>>>,
    multi: &mut MultiState,
    args: &[Vec<u8>],
    verbose: bool,
) -> mredis_proto::Resp {
    stats.incr_command();
    stats.record_command(args);
    if verbose {
        eprintln!("[cmd] {}", crate::stats::format_command(args));
    }

    let Ok(mut guard) = db.lock() else {
        // 锁中毒说明某个线程在持锁时 panic 了。数据库状态可能不一致，
        // 但我们仍要回一条错误而不是让客户端无限等待。
        return mredis_proto::Resp::error("ERR internal error: database lock poisoned");
    };

    let mut ctx = ExecCtx::new(&mut guard);
    ctx.stats = Some(stats);
    ctx.aof = aof;
    ctx.in_multi = multi.active;
    ctx.multi = Some(multi);
    dispatch(&mut ctx, args)
}

/// AOF 初始化标记文件的路径：`<aof>.ready`。
///
/// 用独立 sidecar 文件而不是键空间里的标记键——标记键会让 `DBSIZE` 多 1，
/// 和文档、测试里的 109 对不上，也会混进业务数据。sidecar 还顺带能识别
/// 「老版本写出来的、非空但没有标记」的 AOF。
fn aof_marker_path(aof_path: &str) -> String {
    format!("{aof_path}.ready")
}

fn remove_file_if_exists(path: &str, what: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("mredis-server: 清理{what} {path} 失败: {e}"),
    }
}

fn write_aof_marker(path: &str) -> io::Result<()> {
    if let Some(dir) = std::path::Path::new(path).parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, b"1")
}

/// 后台 AOF fsync 线程。
///
/// `everysec` 的语义是「最多丢 1 秒数据」，这要求**即使没有新命令**也按时 fsync。
/// 命令路径上的 `maybe_flush` 只在有写入时被调用，空闲时永远不会触发，
/// 所以必须单独有个线程定期敲一下。
fn spawn_aof_sync_thread(
    aof: Arc<Mutex<Aof>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(AOF_SYNC_TICK_MS));
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let now = crate::expire::now_ms();
            if let Ok(mut a) = aof.lock()
                && let Err(e) = a.sync_if_due(now)
            {
                eprintln!("mredis-server: AOF 后台 fsync 失败: {e}");
            }
        }
    })
}

/// 后台主动过期线程：每 100ms 扫一次，把惰性删除漏掉的键清掉。
fn spawn_expire_thread(db: Arc<Mutex<Database>>, stats: Arc<Stats>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(EXPIRE_TICK_MS));
            let now = crate::expire::now_ms();
            if let Ok(mut guard) = db.lock() {
                let n = guard.active_expire_cycle(now, EXPIRE_SAMPLES);
                if n > 0 {
                    stats.expired_keys.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    });
}

/// 供测试用：把 `FsyncPolicy` 转成字符串（避免测试里重复写）。
#[cfg(test)]
fn policy_name(p: crate::config::FsyncPolicy) -> &'static str {
    p.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mredis_proto::Resp;

    #[test]
    fn policy_names_are_stable() {
        use crate::config::FsyncPolicy;
        assert_eq!(policy_name(FsyncPolicy::Always), "always");
        assert_eq!(policy_name(FsyncPolicy::EverySec), "everysec");
        assert_eq!(policy_name(FsyncPolicy::No), "no");
    }

    /// `execute` 是服务端与命令层之间唯一的接缝：它负责加锁、计数、
    /// 挂上下文。这里不启线程、不占端口，直接验证这条路径。
    #[test]
    fn execute_wires_stats_and_multi_state() {
        let db = Arc::new(Mutex::new(Database::new()));
        let stats = Arc::new(Stats::new(0));
        let mut multi = MultiState::new();

        let set = execute(
            &db,
            &stats,
            None,
            &mut multi,
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            false,
        );
        assert_eq!(set.as_string().as_deref(), Some("OK"));
        assert_eq!(stats.total_commands.load(Ordering::Relaxed), 1);

        let get = execute(
            &db,
            &stats,
            None,
            &mut multi,
            &[b"GET".to_vec(), b"k".to_vec()],
            false,
        );
        assert_eq!(get.as_string().as_deref(), Some("v"));

        // MULTI 期间命令只入队。
        let queued = execute(&db, &stats, None, &mut multi, &[b"MULTI".to_vec()], false);
        assert_eq!(queued.as_string().as_deref(), Some("OK"));
        let q = execute(
            &db,
            &stats,
            None,
            &mut multi,
            &[b"SET".to_vec(), b"k2".to_vec(), b"v2".to_vec()],
            false,
        );
        assert_eq!(q.as_string().as_deref(), Some("QUEUED"));
        let exec = execute(&db, &stats, None, &mut multi, &[b"EXEC".to_vec()], false);
        match exec {
            Resp::Array(items) => assert_eq!(items.len(), 1),
            other => panic!("EXEC 应返回数组，得到 {other:?}"),
        }
    }

    #[test]
    fn execute_reports_protocol_errors_as_resp_errors() {
        let db = Arc::new(Mutex::new(Database::new()));
        let stats = Arc::new(Stats::new(0));
        let mut multi = MultiState::new();
        // 空命令不应 panic，也不应报错（SPEC 2.4：`*0` → Null）。
        let r = execute(&db, &stats, None, &mut multi, &[], false);
        assert_eq!(r, Resp::Null);
        // 未知命令
        let r = execute(&db, &stats, None, &mut multi, &[b"NOPE".to_vec()], false);
        assert!(r.is_error());
    }
}
