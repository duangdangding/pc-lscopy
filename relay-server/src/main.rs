//! lscopy 中继服务器（M3）
//!
//! 职责（对应 docs/relay-sync-design.md §4）：
//! - WebSocket 接入（/ws），连接建立后**第一步强制鉴权**，不过即断开
//! - 挑战-响应鉴权：服务器下发 nonce，客户端回 HMAC-SHA256(接入密钥, nonce+deviceId+ts)
//! - 分组（room）内转发 push → clip，成员变化广播 peers
//! - 离线暂存：push 的条目落 SQLite 队列（每条分配递增 seq），TTL 7 天、每组上限 5000；
//!   客户端连接后发 pull {sinceSeq} 补拉错过的条目
//! - 防爆破：按 IP 计失败次数，超限临时封禁；单 IP 并发连接数上限
//!
//! 暂不含：端到端加密（M4，服务器无需感知）。

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    http::StatusCode,
    response::Response,
    routing::get,
    Router,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::{mpsc, Mutex, RwLock};

// ---------- 常量 ----------

/// hello 鉴权超时（秒）：超时未发 hello 直接断开
const HELLO_TIMEOUT_SECS: u64 = 5;
/// 时间戳容忍窗口（毫秒）：±5 分钟，防重放
const TS_WINDOW_MS: i64 = 5 * 60 * 1000;
/// 单条 WS 消息上限（M1 仅文本，8MB 足够；图片走中继是 M5 的事）
const MAX_MSG_BYTES: usize = 8 * 1024 * 1024;
/// 防爆破：连续鉴权失败多少次后临时封禁
const BAN_FAILS: u32 = 5;
/// 防爆破：封禁时长
const BAN_DURATION: Duration = Duration::from_secs(10 * 60);
/// 单 IP 默认并发连接上限
const DEFAULT_MAX_CONN_PER_IP: usize = 20;
/// 离线暂存：条目保留时长（7 天，按服务器接收时间计）
const QUEUE_TTL_MS: i64 = 7 * 24 * 3600 * 1000;
/// 离线暂存：每组最多保留条数，超限淘汰最旧
const QUEUE_MAX_PER_GROUP: i64 = 5000;

type HmacSha256 = Hmac<Sha256>;

// ---------- 配置（环境变量 < 配置文件 < 命令行） ----------

#[derive(Parser, Debug)]
#[command(name = "relay-server", about = "lscopy 自建中继服务器")]
struct Cli {
    /// 监听端口
    #[arg(long)]
    port: Option<u16>,
    /// 接入密钥（可重复多次指定）
    #[arg(long = "access-key")]
    access_keys: Vec<String>,
    /// 数据目录（M3 暂存队列用，M1 仅记录）
    #[arg(long)]
    data_dir: Option<String>,
    /// 配置文件路径
    #[arg(long, default_value = "relay-server.json")]
    config: String,
    /// 单 IP 并发连接上限
    #[arg(long)]
    max_conn_per_ip: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    port: Option<u16>,
    access_keys: Option<Vec<String>>,
    data_dir: Option<String>,
    max_conn_per_ip: Option<usize>,
}

#[derive(Clone)]
struct Config {
    port: u16,
    access_keys: Vec<String>,
    data_dir: String,
    max_conn_per_ip: usize,
}

fn load_config() -> Result<Config, String> {
    let cli = Cli::parse();

    // 第一层：环境变量
    let env_cfg = FileConfig {
        port: std::env::var("RELAY_PORT").ok().and_then(|v| v.parse().ok()),
        access_keys: std::env::var("RELAY_ACCESS_KEYS").ok().map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }),
        data_dir: std::env::var("RELAY_DATA_DIR").ok(),
        max_conn_per_ip: std::env::var("RELAY_MAX_CONN_PER_IP")
            .ok()
            .and_then(|v| v.parse().ok()),
    };

    // 第二层：配置文件（不存在则跳过）
    let cfg_path = std::env::var("RELAY_CONFIG").unwrap_or(cli.config.clone());
    let mut file_cfg = FileConfig::default();
    match std::fs::read_to_string(&cfg_path) {
        Ok(text) => {
            file_cfg = serde_json::from_str(&text)
                .map_err(|e| format!("配置文件 {cfg_path} 解析失败: {e}"))?;
            log(&format!("已加载配置文件 {cfg_path}"));
        }
        Err(_) => {
            if cfg_path != "relay-server.json" {
                return Err(format!("配置文件 {cfg_path} 不存在"));
            }
        }
    }

    // 第三层：命令行（最高优先级）
    let access_keys = if !cli.access_keys.is_empty() {
        cli.access_keys
    } else if let Some(k) = file_cfg.access_keys {
        k
    } else {
        env_cfg.access_keys.unwrap_or_default()
    };
    let access_keys: Vec<String> = access_keys
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if access_keys.is_empty() {
        return Err(
            "未配置任何接入密钥（access_keys）。鉴权是强制的，请通过 RELAY_ACCESS_KEYS、\
             配置文件或 --access-key 至少提供一个。"
                .into(),
        );
    }

    Ok(Config {
        port: cli.port.or(file_cfg.port).or(env_cfg.port).unwrap_or(8780),
        access_keys,
        data_dir: cli
            .data_dir
            .or(file_cfg.data_dir)
            .or(env_cfg.data_dir)
            .unwrap_or_else(|| "data".into()),
        max_conn_per_ip: cli
            .max_conn_per_ip
            .or(file_cfg.max_conn_per_ip)
            .or(env_cfg.max_conn_per_ip)
            .unwrap_or(DEFAULT_MAX_CONN_PER_IP),
    })
}

// ---------- 共享状态 ----------

struct RoomMember {
    name: String,
    tx: mpsc::UnboundedSender<String>,
}

struct BanInfo {
    fails: u32,
    banned_until: Option<Instant>,
}

struct AppState {
    cfg: Config,
    /// 分组 -> (deviceId -> 成员)
    rooms: RwLock<HashMap<String, HashMap<String, RoomMember>>>,
    /// 单 IP 当前连接数
    ip_conns: Mutex<HashMap<IpAddr, usize>>,
    /// 单 IP 鉴权失败计数与封禁状态
    ip_bans: Mutex<HashMap<IpAddr, BanInfo>>,
    /// 离线暂存队列（SQLite，异步上下文用 tokio Mutex）
    queue: Mutex<rusqlite::Connection>,
}

// ---------- 离线暂存队列 ----------

fn open_queue(data_dir: &str) -> Result<rusqlite::Connection, String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("数据目录创建失败: {e}"))?;
    let path = std::path::Path::new(data_dir).join("relay-queue.db");
    let conn = rusqlite::Connection::open(&path).map_err(|e| format!("队列数据库打开失败: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS relay_queue (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            group_id TEXT NOT NULL,
            payload TEXT NOT NULL,
            received_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_queue_group ON relay_queue(group_id, seq);",
    )
    .map_err(|e| format!("队列数据库初始化失败: {e}"))?;
    Ok(conn)
}

impl AppState {
    /// 暂存一条 clip 并返回分配的 seq；同时做 TTL / 条数淘汰
    async fn queue_push(&self, group: &str, payload: &str) -> Option<i64> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let conn = self.queue.lock().await;
        conn.execute(
            "INSERT INTO relay_queue(group_id, payload, received_at) VALUES(?1, ?2, ?3)",
            rusqlite::params![group, payload, now],
        )
        .ok()?;
        let seq = conn.last_insert_rowid();
        // TTL 淘汰（全表，按接收时间）
        let _ = conn.execute(
            "DELETE FROM relay_queue WHERE received_at < ?1",
            rusqlite::params![now - QUEUE_TTL_MS],
        );
        // 每组条数上限：保留最新 QUEUE_MAX_PER_GROUP 条
        let _ = conn.execute(
            "DELETE FROM relay_queue WHERE group_id = ?1 AND seq NOT IN (
                SELECT seq FROM relay_queue WHERE group_id = ?1 ORDER BY seq DESC LIMIT ?2
            )",
            rusqlite::params![group, QUEUE_MAX_PER_GROUP],
        );
        Some(seq)
    }

    /// 补拉：取出分组内 seq > since 的全部暂存条目（升序）
    async fn queue_pull(&self, group: &str, since: i64) -> Vec<(i64, String)> {
        let conn = self.queue.lock().await;
        let Ok(mut stmt) = conn.prepare(
            "SELECT seq, payload FROM relay_queue WHERE group_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map(rusqlite::params![group, since], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        });
        match rows {
            Ok(m) => m.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 连接前检查：被封禁则拒绝
    async fn is_banned(&self, ip: &IpAddr) -> bool {
        let bans = self.ip_bans.lock().await;
        match bans.get(ip) {
            Some(b) => b.banned_until.is_some_and(|t| t > Instant::now()),
            None => false,
        }
    }

    /// 连接数占位（超限返回 false）
    async fn acquire_conn(&self, ip: IpAddr) -> bool {
        let mut m = self.ip_conns.lock().await;
        let n = m.entry(ip).or_insert(0);
        if *n >= self.cfg.max_conn_per_ip {
            return false;
        }
        *n += 1;
        true
    }

    async fn release_conn(&self, ip: &IpAddr) {
        let mut m = self.ip_conns.lock().await;
        if let Some(n) = m.get_mut(ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(ip);
            }
        }
    }

    /// 鉴权失败计数：连续失败达上限则封禁
    async fn record_auth_fail(&self, ip: IpAddr) {
        let mut bans = self.ip_bans.lock().await;
        let b = bans.entry(ip).or_insert(BanInfo {
            fails: 0,
            banned_until: None,
        });
        b.fails += 1;
        if b.fails >= BAN_FAILS {
            b.banned_until = Some(Instant::now() + BAN_DURATION);
            b.fails = 0;
            log(&format!("IP {ip} 连续鉴权失败，封禁 10 分钟"));
        }
    }

    /// 构造分组内成员名单（peers 消息体）
    async fn peers_payload(&self, group: &str) -> String {
        let rooms = self.rooms.read().await;
        let devices: Vec<Value> = rooms
            .get(group)
            .map(|members| {
                members
                    .iter()
                    .map(|(id, m)| json!({"deviceId": id, "name": m.name, "online": true}))
                    .collect()
            })
            .unwrap_or_default();
        json!({ "op": "peers", "devices": devices }).to_string()
    }

    /// 向分组内除 exclude 外的成员发送文本消息
    async fn broadcast(&self, group: &str, exclude: Option<&str>, msg: &str) {
        let txs: Vec<(String, mpsc::UnboundedSender<String>)> = {
            let rooms = self.rooms.read().await;
            rooms
                .get(group)
                .map(|members| {
                    members
                        .iter()
                        .filter(|(id, _)| Some(id.as_str()) != exclude)
                        .map(|(id, m)| (id.clone(), m.tx.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        for (id, tx) in txs {
            if tx.send(msg.to_string()).is_err() {
                // 接收端已断开，由该连接自己的清理逻辑移除，这里仅记录
                log(&format!("向设备 {id} 投递失败（连接可能已断开）"));
            }
        }
    }
}

// ---------- 工具 ----------

fn log(msg: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 简单可读的时间戳（UTC 秒级即可，Docker 日志会自带时间）
    println!("[{now}] {msg}");
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn hmac_sha256(key: &str, msg: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC 接受任意长度密钥");
    mac.update(msg.as_bytes());
    mac.finalize().into_bytes().into()
}

/// 常量时间比较，防时序侧信道
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------- HTTP 路由 ----------

async fn healthz() -> &'static str {
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, StatusCode> {
    let ip = addr.ip();
    if state.is_banned(&ip).await {
        return Err(StatusCode::FORBIDDEN);
    }
    if !state.acquire_conn(ip).await {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let st = state.clone();
    Ok(ws
        .max_message_size(MAX_MSG_BYTES)
        .on_upgrade(move |socket| async move {
            handle_conn(socket, ip, state).await;
            st.release_conn(&ip).await;
        }))
}

// ---------- 单连接生命周期 ----------

async fn handle_conn(socket: WebSocket, ip: IpAddr, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // ---- 第一步：下发挑战 nonce ----
    let nonce = hex::encode(rand::random::<[u8; 32]>());
    if sender
        .send(Message::Text(
            json!({ "op": "challenge", "nonce": nonce }).to_string().into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    // ---- 第二步：限时等待 hello ----
    let hello_raw = match tokio::time::timeout(Duration::from_secs(HELLO_TIMEOUT_SECS), receiver.next()).await
    {
        Ok(Some(Ok(Message::Text(t)))) => t.to_string(),
        _ => {
            log(&format!("{ip} hello 超时或消息非法，断开"));
            let _ = sender
                .send(Message::Text(
                    json!({ "op": "error", "code": "auth_timeout" }).to_string().into(),
                ))
                .await;
            let _ = sender.send(Message::Close(None)).await;
            return;
        }
    };

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Hello {
        device_id: String,
        name: Option<String>,
        group: String,
        ts: i64,
        auth: String,
    }

    let hello: Hello = match serde_json::from_str::<Hello>(&hello_raw) {
        Ok(h) if h.device_id.len() <= 128 && h.group.len() <= 128 => h,
        _ => {
            let _ = sender
                .send(Message::Text(
                    json!({ "op": "error", "code": "bad_hello" }).to_string().into(),
                ))
                .await;
            let _ = sender.send(Message::Close(None)).await;
            return;
        }
    };

    // ---- 第三步：校验时间戳窗口 + HMAC ----
    if (now_ms() - hello.ts).abs() > TS_WINDOW_MS {
        let _ = sender
            .send(Message::Text(
                json!({ "op": "error", "code": "auth_failed", "reason": "timestamp" })
                    .to_string()
                    .into(),
            ))
            .await;
        let _ = sender.send(Message::Close(None)).await;
        state.record_auth_fail(ip).await;
        return;
    }

    let auth_bytes = hex::decode(hello.auth.trim()).unwrap_or_default();
    let payload = format!("{}{}{}", nonce, hello.device_id, hello.ts);
    let ok = state
        .cfg
        .access_keys
        .iter()
        .any(|k| ct_eq(&hmac_sha256(k, &payload), &auth_bytes));

    if !ok {
        log(&format!("{ip} 鉴权失败（deviceId={}）", hello.device_id));
        let _ = sender
            .send(Message::Text(
                json!({ "op": "error", "code": "auth_failed" }).to_string().into(),
            ))
            .await;
        let _ = sender.send(Message::Close(None)).await;
        state.record_auth_fail(ip).await;
        return;
    }

    // ---- 鉴权通过：进入分组 ----
    let device_id = hello.device_id.clone();
    let name = hello.name.clone().unwrap_or_else(|| device_id.clone());
    let group = hello.group.clone();
    log(&format!("设备 {device_id}（{name}）加入分组 {group}，来自 {ip}"));

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    {
        let mut rooms = state.rooms.write().await;
        let members = rooms.entry(group.clone()).or_default();
        // 同 deviceId 重连：替换旧通道，旧连接的写任务会因发送失败自行退出
        members.insert(
            device_id.clone(),
            RoomMember {
                name: name.clone(),
                tx: tx.clone(),
            },
        );
    }

    // 欢迎 + 全量名单（含自己），并向其他人广播最新名单
    let _ = tx.send(json!({ "op": "welcome", "deviceId": device_id, "group": group }).to_string());
    let peers = state.peers_payload(&group).await;
    state.broadcast(&group, None, &peers).await;

    // 写任务：出站队列 -> WS
    let write_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // ---- 读循环 ----
    while let Some(Ok(msg)) = receiver.next().await {
        let Message::Text(text) = msg else { continue };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("op").and_then(|o| o.as_str()) {
            // 剪贴板条目：暂存（分配 seq）后转发给分组内其他成员，ack 携带 seq 供发送方推进游标
            Some("push") => {
                if let Some(clip) = v.get("clip") {
                    let payload = clip.to_string();
                    let Some(seq) = state.queue_push(&group, &payload).await else {
                        let _ = tx.send(
                            json!({ "op": "error", "code": "queue_error" }).to_string(),
                        );
                        continue;
                    };
                    let out = json!({ "op": "clip", "seq": seq, "clip": clip }).to_string();
                    state.broadcast(&group, Some(&device_id), &out).await;
                    let ack = json!({
                        "op": "acked",
                        "remoteId": clip.get("remoteId").cloned().unwrap_or(Value::Null),
                        "seq": seq,
                    })
                    .to_string();
                    let _ = tx.send(ack);
                }
            }
            Some("ping") => {
                let _ = tx.send(json!({ "op": "pong", "ts": now_ms() }).to_string());
            }
            // 离线补拉：把分组内 seq > sinceSeq 的暂存条目逐条下发
            // （与实时推送撞车产生的重复由客户端内容哈希查重兜住）
            Some("pull") => {
                let since = v.get("sinceSeq").and_then(|s| s.as_i64()).unwrap_or(0);
                let backlog = state.queue_pull(&group, since).await;
                if !backlog.is_empty() {
                    log(&format!("设备 {device_id} 补拉 {} 条（sinceSeq={since}）", backlog.len()));
                }
                for (seq, payload) in backlog {
                    let Ok(clip) = serde_json::from_str::<Value>(&payload) else {
                        continue;
                    };
                    if tx
                        .send(json!({ "op": "clip", "seq": seq, "clip": clip }).to_string())
                        .is_err()
                    {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    // ---- 清理：移出分组，广播新名单 ----
    write_task.abort();
    {
        let mut rooms = state.rooms.write().await;
        let mut empty = false;
        if let Some(members) = rooms.get_mut(&group) {
            members.remove(&device_id);
            empty = members.is_empty();
        }
        if empty {
            rooms.remove(&group);
        }
    }
    log(&format!("设备 {device_id} 离开分组 {group}"));
    let peers = state.peers_payload(&group).await;
    state.broadcast(&group, None, &peers).await;
}

// ---------- 入口 ----------

#[tokio::main]
async fn main() {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误：{e}");
            std::process::exit(1);
        }
    };
    let port = cfg.port;
    log(&format!(
        "relay-server 启动：端口 {port}，接入密钥 {} 个，单 IP 连接上限 {}，数据目录 {}",
        cfg.access_keys.len(),
        cfg.max_conn_per_ip,
        cfg.data_dir
    ));

    let queue = match open_queue(&cfg.data_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let state = Arc::new(AppState {
        cfg,
        rooms: RwLock::new(HashMap::new()),
        ip_conns: Mutex::new(HashMap::new()),
        ip_bans: Mutex::new(HashMap::new()),
        queue: Mutex::new(queue),
    });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("端口 {port} 监听失败：{e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        log("收到 Ctrl-C，正在退出");
    })
    .await
    {
        eprintln!("服务异常退出：{e}");
        std::process::exit(1);
    }
}
