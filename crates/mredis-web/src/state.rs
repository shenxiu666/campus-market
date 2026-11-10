//! 应用状态：到 RESP 服务端的连接与抽象。
//!
//! ## 为什么抽象出 `Backend`
//!
//! 单元测试要能在**没有真实服务端**的情况下打完整条 HTTP 链路（`tower::ServiceExt::oneshot`
//! 直接打 router），因此把「发一条 RESP 命令」抽成 trait。生产实现 [`RespBackend`]
//! 持有真实 TCP 连接，测试实现是一个内存版的迷你 Redis（见 `crate::fake`）。
//!
//! ## 连接管理的取舍
//!
//! 演示规模下每个请求 `Client::connect` 都新建 TCP 连接太浪费（三次握手 + 每请求一个线程），
//! 所以这里复用**一条**连接并用 `Mutex` 串行化：吞吐受限于单连接，但实现最简单、
//! 也不会出现连接泄漏。生产环境应换成连接池（如 `r2d2` 风格）或每请求一连接 + keep-alive。
//!
//! 由于 `mredis-client` 是**同步阻塞**客户端，所有调用都放进 `tokio::task::spawn_blocking`，
//! 避免阻塞异步运行时的工作线程。

use mredis_client::Client;
use mredis_proto::Resp;
use std::sync::{Arc, Mutex};

use crate::error::ApiError;

/// 一条到存储层的命令通道。
pub trait Backend: Send + Sync + 'static {
    /// 发送一条命令并读取回包。传输层失败返回 `Err`，服务端业务错误返回 `Ok(Resp::Error)`。
    fn call(&self, args: &[&str]) -> Result<Resp, String>;

    /// 批量发送命令（pipeline），按顺序返回回包。
    ///
    /// 默认实现退化为逐条发送；真实实现会一次写出、一次读回，省掉 N-1 次 RTT。
    fn pipeline(&self, cmds: &[Vec<Vec<u8>>]) -> Result<Vec<Resp>, String> {
        let mut out = Vec::with_capacity(cmds.len());
        for cmd in cmds {
            let refs: Vec<&str> = cmd
                .iter()
                .map(|b| std::str::from_utf8(b).unwrap_or_default())
                .collect();
            out.push(self.call(&refs)?);
        }
        Ok(out)
    }
}

/// 真实后端：复用一个 `mredis_client::Client`。
pub struct RespBackend {
    addr: String,
    client: Mutex<Option<Client>>,
}

impl RespBackend {
    /// 创建一个指向 `addr` 的后端，首次调用时才真正建立连接。
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            client: Mutex::new(None),
        }
    }

    fn with_client<T>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = self
            .client
            .lock()
            .map_err(|_| "客户端互斥锁已中毒".to_string())?;
        if guard.is_none() {
            let client = Client::connect(&self.addr).map_err(|e| e.to_string())?;
            *guard = Some(client);
        }
        let client = guard.as_mut().ok_or_else(|| "客户端缺失".to_string())?;
        match f(client) {
            Ok(v) => Ok(v),
            Err(e) => {
                // 连接已损坏，丢弃后让下一次调用重连
                *guard = None;
                Err(e)
            }
        }
    }
}

impl Backend for RespBackend {
    fn call(&self, args: &[&str]) -> Result<Resp, String> {
        self.with_client(|client| client.call(args).map_err(|e| e.to_string()))
    }

    fn pipeline(&self, cmds: &[Vec<Vec<u8>>]) -> Result<Vec<Resp>, String> {
        self.with_client(|client| client.pipeline(cmds).map_err(|e| e.to_string()))
    }
}

/// 共享状态。
#[derive(Clone)]
pub struct AppState {
    /// RESP 服务端地址，如 `127.0.0.1:6380`
    pub redis_addr: Arc<String>,
    /// 复用的客户端连接（`Mutex` 串行化，演示规模足够）
    pub client: Arc<Mutex<Option<mredis_client::Client>>>,
    /// 命令通道（生产是 [`RespBackend`]，测试是内存假实现）
    backend: Arc<dyn Backend>,
}

impl AppState {
    /// 构造指向真实服务端的状态。
    pub fn new(redis_addr: impl Into<String>) -> Self {
        let addr = redis_addr.into();
        let client = Arc::new(Mutex::new(None));
        Self {
            redis_addr: Arc::new(addr.clone()),
            client,
            backend: Arc::new(RespBackend::new(addr)),
        }
    }

    /// 用自定义后端构造状态（测试用）。
    pub fn with_backend(redis_addr: impl Into<String>, backend: Arc<dyn Backend>) -> Self {
        Self {
            redis_addr: Arc::new(redis_addr.into()),
            client: Arc::new(Mutex::new(None)),
            backend,
        }
    }

    /// 底层命令通道。
    pub fn backend(&self) -> Arc<dyn Backend> {
        self.backend.clone()
    }

    /// 取一个可用客户端；连接断开时自动重连。
    ///
    /// 注意：这条路绕过了 `spawn_blocking`，只适合在同步上下文（测试、脚本）里用。
    pub fn with_client<T>(
        &self,
        f: impl FnOnce(&mut mredis_client::Client) -> Result<T, mredis_client::ClientError>,
    ) -> Result<T, mredis_client::ClientError> {
        let mut guard = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(mredis_client::Client::connect(&self.redis_addr)?);
        }
        let client = guard.as_mut().expect("just ensured");
        match f(client) {
            Ok(v) => Ok(v),
            Err(mredis_client::ClientError::Io(_) | mredis_client::ClientError::Proto(_)) => {
                *guard = None;
                Err(mredis_client::ClientError::Server(
                    "storage reconnecting".into(),
                ))
            }
            Err(e) => Err(e),
        }
    }

    /// 异步发送一条命令，参数是字符串切片。
    ///
    /// 传输层失败（连不上、连接断开）映射成 503；服务端返回的 `-ERR` 原样交给调用方判断。
    pub async fn call_str(&self, args: &[&str]) -> Result<Resp, ApiError> {
        let backend = self.backend.clone();
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
            backend.call(&refs)
        })
        .await
        .map_err(|_| ApiError::unavailable())?
        .map_err(|_| ApiError::unavailable())
    }

    /// 异步发送一条命令（内部 `spawn_blocking`）。
    pub async fn call(&self, args: Vec<String>) -> Result<Resp, ApiError> {
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            backend.call(&refs)
        })
        .await
        .map_err(|_| ApiError::unavailable())?
        .map_err(|_| ApiError::unavailable())
    }

    /// 异步发送一组 pipeline 命令。
    pub async fn pipeline(&self, cmds: Vec<Vec<Vec<u8>>>) -> Result<Vec<Resp>, ApiError> {
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.pipeline(&cmds))
            .await
            .map_err(|_| ApiError::unavailable())?
            .map_err(|_| ApiError::unavailable())
    }
}
