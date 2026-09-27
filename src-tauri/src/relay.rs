//! 云端中继客户端（M2，对应 docs/relay-sync-design.md §6）
//!
//! 与 LAN 同步并行存在的第二条通道：
//! - 连接自建中继服务器（relay-server），挑战-响应 HMAC 鉴权（见 §4.5）
//! - 本机新剪贴板（仅文本，图片/文件仅局域网 §7.3）push 到服务器转发
//! - 收到分组内其他设备的 clip 后复用 `lan::store_remote` 入库（内容哈希查重，
//!   双通道重复到达自动判重）
//! - 断线指数退避重连；配置变更（代际号变化）自动断开重连
//!
//! 回环铁律：只有本机剪贴板监听（lib.rs start_watcher）会产生 push；
//! 入库的远程条目绝不外发。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

use crate::{hash_bytes, now_secs, AppState};

// ---------- 常量 ----------

/// 鉴权握手超时（与服务器 HELLO_TIMEOUT_SECS 对应，客户端侧略放宽）
const AUTH_TIMEOUT: Duration = Duration::from_secs(8);
/// 心跳间隔（服务器无强制要求，保活用）
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// 连接断开后重连的基础退避
const RECONNECT_MIN: Duration = Duration::from_secs(3);
/// 重连退避上限
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// 线上记录类型（与 lan.rs / 安卓端一致）
const WIRE_TEXT: i64 = 0;

type HmacSha256 = Hmac<Sha256>;

// ---------- 配置（存 lscopy-config.json 顶层 relay 键） ----------

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct RelaySettings {
    /// 启用云端中继同步
    pub enabled: bool,
    /// 服务器地址：支持 `host:port`、`ws://host:port`、`wss://domain`（自动补 /ws）
    pub server_url: String,
    /// 分组 ID：同一组设备互相同步
    pub group_id: String,
    /// 接入密钥（服务器 RELAY_ACCESS_KEYS 之一）
    pub access_key: String,
    /// 补拉游标：服务器分配的条目序号，连接成功后 pull sinceSeq=last_seq
    pub last_seq: i64,
}

/// 规范化服务器地址：补 ws:// 前缀与 /ws 路径
fn normalize_url(raw: &str) -> String {
    let mut u = raw.trim().to_string();
    if u.is_empty() {
        return u;
    }
    if !u.starts_with("ws://") && !u.starts_with("wss://") {
        u = format!("ws://{u}");
    }
    // 去掉末尾斜杠后，没有路径部分就补 /ws
    let trimmed = u.trim_end_matches('/');
    let after_scheme = &trimmed[trimmed.find("://").map(|i| i + 3).unwrap_or(0)..];
    if !after_scheme.contains('/') {
        u = format!("{trimmed}/ws");
    }
    u
}

// ---------- 共享状态（挂在 AppState 上） ----------

#[derive(Serialize, Clone)]
pub struct PeerDto {
    device_id: String,
    name: String,
}

pub struct RelayShared {
    pub settings: Mutex<RelaySettings>,
    /// 当前连接的出站队列（None = 未连接）
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// 配置代际：每次修改 +1，连接循环发现不一致即断开重连
    gen: AtomicU64,
    /// 连接状态文本（直接给 UI 展示）
    status: Mutex<String>,
    /// 分组内在线设备（服务器 peers 广播）
    peers: Mutex<Vec<PeerDto>>,
}

impl RelayShared {
    pub fn new(settings: RelaySettings) -> Self {
        Self {
            settings: Mutex::new(settings),
            tx: Mutex::new(None),
            gen: AtomicU64::new(0),
            status: Mutex::new("未启用".into()),
            peers: Mutex::new(Vec::new()),
        }
    }

    fn set_status(&self, app: &AppHandle, s: &str) {
        let mut cur = self.status.lock().unwrap();
        if *cur == s {
            return; // 状态没变不重复广播（未启用时主循环每 2s 会走到这里）
        }
        *cur = s.to_string();
        drop(cur);
        let _ = app.emit("relay-state-changed", ());
    }
}

// ---------- 发送链路（本机剪贴板 → 中继） ----------

/// 本机新文本剪贴板入库后由监听线程调用：组包丢进出站队列（未连接/未启用时丢弃）
pub fn push_local_text(state: &AppState, text: &str, hash: u64) {
    let tx = state.relay.tx.lock().unwrap().clone();
    let Some(tx) = tx else { return };
    if !state.relay.settings.lock().unwrap().enabled {
        return;
    }
    let my_id = state.lan.settings.lock().unwrap().device_id.clone();
    // 取刚入库行的本地 id 作为 remoteId（与 LAN /clips 的口径一致）
    let (id, created_at) = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT id, created_at FROM clips WHERE hash = ?1 ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![hash as i64],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap_or((0, now_secs()))
    };
    let msg = json!({
        "op": "push",
        "clip": {
            "type": WIRE_TEXT,
            "text": text,
            "timestamp": created_at * 1000,
            "remoteDeviceId": my_id,
            "remoteId": id,
        }
    })
    .to_string();
    let _ = tx.send(msg);
}

// ---------- 接收链路（中继 → 入库） ----------

/// 处理服务器推来的 clip：环回防护 + 仅文本 + 复用内容哈希查重入库
fn import_relay_clip(app: &AppHandle, clip: &Value) {
    let state = app.state::<AppState>();
    let (my_id, allow_text) = {
        let s = state.lan.settings.lock().unwrap();
        (s.device_id.clone(), s.sync_text)
    };
    // 环回防护：内容本来就来自本机
    let origin = clip.get("remoteDeviceId").and_then(|v| v.as_str());
    if origin == Some(my_id.as_str()) {
        return;
    }
    // M2 仅同步文本（图片/文件仅局域网，见设计文档 §7.3）
    if clip.get("type").and_then(|v| v.as_i64()) != Some(WIRE_TEXT) {
        return;
    }
    if !allow_text {
        return;
    }
    let Some(text) = clip.get("text").and_then(|v| v.as_str()) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    let created_at = clip
        .get("timestamp")
        .and_then(|v| v.as_i64())
        .map(|ms| (ms / 1000).max(0))
        .unwrap_or_else(now_secs);
    let remote_id = clip.get("remoteId").and_then(|v| v.as_i64());
    let h = hash_bytes(text.as_bytes());
    let inserted = {
        let db = state.db.lock().unwrap();
        crate::lan::store_remote(
            &db,
            "text",
            Some(text),
            None,
            None,
            None,
            h,
            created_at,
            origin,
            remote_id,
        )
    };
    if inserted {
        let _ = app.emit("clip-added", ());
    }
}

/// 推进补拉游标（只前进不后退）；游标在内存中实时更新，落盘在连接断开时统一做
fn advance_cursor(state: &AppState, seq: i64) {
    if seq <= 0 {
        return;
    }
    let mut s = state.relay.settings.lock().unwrap();
    if seq > s.last_seq {
        s.last_seq = seq;
    }
}

fn handle_server_msg(app: &AppHandle, text: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return;
    };
    match v.get("op").and_then(|o| o.as_str()) {
        Some("clip") => {
            let state = app.state::<AppState>();
            if let Some(seq) = v.get("seq").and_then(|s| s.as_i64()) {
                advance_cursor(&state, seq);
            }
            if let Some(clip) = v.get("clip") {
                import_relay_clip(app, clip);
            }
        }
        // acked 也携带 seq：自己 push 的条目同样推进游标，避免补拉时拉回自己的
        Some("acked") => {
            let state = app.state::<AppState>();
            if let Some(seq) = v.get("seq").and_then(|s| s.as_i64()) {
                advance_cursor(&state, seq);
            }
        }
        Some("peers") => {
            let state = app.state::<AppState>();
            let my_id = state.lan.settings.lock().unwrap().device_id.clone();
            let mut peers: Vec<PeerDto> = v
                .get("devices")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|d| {
                            let id = d.get("deviceId")?.as_str()?.to_string();
                            if id == my_id {
                                return None; // 名单里不显示自己
                            }
                            let name = d
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            Some(PeerDto {
                                device_id: id,
                                name,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            peers.sort_by(|a, b| a.name.cmp(&b.name));
            *state.relay.peers.lock().unwrap() = peers;
            let _ = app.emit("relay-state-changed", ());
        }
        // acked / pong / error(not_implemented) 等暂不需要处理
        _ => {}
    }
}

// ---------- 连接循环 ----------

fn hmac_hex(key: &str, msg: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC 接受任意长度密钥");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 单次连接：鉴权握手 → 收发循环。返回后由外层决定是否重连。
async fn run_connection(app: &AppHandle, url: &str, group: &str, key: &str, my_gen: u64) {
    use tokio_tungstenite::tungstenite::Message;

    let state = app.state::<AppState>();
    state.relay.set_status(app, "连接中…");

    let (ws, _) = match tokio_tungstenite::connect_async(url).await {
        Ok(v) => v,
        Err(e) => {
            state.relay.set_status(app, &format!("连接失败：{e}"));
            return;
        }
    };
    let (mut sink, mut stream) = ws.split();

    // ---- 鉴权握手：challenge → hello → welcome ----
    let nonce = match tokio::time::timeout(AUTH_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).ok().and_then(|v| {
            if v.get("op").and_then(|o| o.as_str()) == Some("challenge") {
                v.get("nonce").and_then(|n| n.as_str()).map(String::from)
            } else {
                None
            }
        }),
        _ => None,
    };
    let Some(nonce) = nonce else {
        state
            .relay
            .set_status(app, "鉴权失败：服务器未下发挑战（对方是 relay-server 吗？）");
        return;
    };

    let (device_id, device_name) = {
        let s = state.lan.settings.lock().unwrap();
        (s.device_id.clone(), s.device_name.clone())
    };
    let ts = now_ms();
    let auth = hmac_hex(key, &format!("{nonce}{device_id}{ts}"));
    let hello = json!({
        "op": "hello",
        "deviceId": device_id,
        "name": device_name,
        "group": group,
        "ts": ts,
        "auth": auth,
    })
    .to_string();
    if sink.send(Message::Text(hello.into())).await.is_err() {
        state.relay.set_status(app, "连接失败：无法发送握手消息");
        return;
    }

    match tokio::time::timeout(AUTH_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => {
            let v: Value = serde_json::from_str(&t).unwrap_or_default();
            match v.get("op").and_then(|o| o.as_str()) {
                Some("welcome") => {}
                Some("error") => {
                    let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("");
                    let hint = match code {
                        "auth_failed" => "鉴权失败：接入密钥错误或时间偏差过大",
                        _ => "鉴权被服务器拒绝",
                    };
                    state.relay.set_status(app, hint);
                    return;
                }
                _ => {
                    state.relay.set_status(app, "鉴权失败：服务器响应异常");
                    return;
                }
            }
        }
        _ => {
            state.relay.set_status(app, "鉴权失败：等待服务器响应超时");
            return;
        }
    }

    // ---- 鉴权通过：登记出站队列，补拉离线条目，进入收发循环 ----
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    *state.relay.tx.lock().unwrap() = Some(tx.clone());
    state.relay.set_status(app, "🟢 已连接");
    // 补拉离线期间错过的条目（seq 游标，重复到达由内容哈希查重兜住）
    let since = state.relay.settings.lock().unwrap().last_seq;
    let _ = tx.send(json!({ "op": "pull", "sinceSeq": since }).to_string());

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // 跳过第一次立即触发
    loop {
        tokio::select! {
            _ = ping.tick() => {
                // 配置变更（代际号不一致）：断开让外层用新配置重连
                if state.relay.gen.load(Ordering::SeqCst) != my_gen {
                    break;
                }
                if sink.send(Message::Text(json!({"op":"ping"}).to_string().into())).await.is_err() {
                    break;
                }
            }
            out = rx.recv() => {
                match out {
                    Some(m) => {
                        if sink.send(Message::Text(m.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            inc = stream.next() => {
                match inc {
                    Some(Ok(Message::Text(t))) => handle_server_msg(app, &t),
                    Some(Ok(_)) => {} // ping/pong/close 等控制帧
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    // 清理：只有自己还是当前连接时才清空出站队列（防止误清新连接的）
    let mut guard = state.relay.tx.lock().unwrap();
    if let Some(cur) = guard.as_ref() {
        if cur.same_channel(&tx) {
            *guard = None;
        }
    }
    drop(guard);
    *state.relay.peers.lock().unwrap() = Vec::new();
    // 补拉游标落盘（内存中已实时推进，这里统一持久化一次）
    let _ = crate::persist_config(&state);
    state.relay.set_status(app, "连接已断开");
}

/// 中继模块主循环：按配置连接，断线指数退避重连；未启用时空转等待
pub fn start(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut backoff = RECONNECT_MIN;
        let mut last_gen = u64::MAX; // 强制首轮读取
        loop {
            let state = app.state::<AppState>();
            let gen = state.relay.gen.load(Ordering::SeqCst);
            let (enabled, url, group, key) = {
                let s = state.relay.settings.lock().unwrap();
                (
                    s.enabled,
                    normalize_url(&s.server_url),
                    s.group_id.trim().to_string(),
                    s.access_key.clone(),
                )
            };

            if !enabled || url.is_empty() || group.is_empty() || key.is_empty() {
                if enabled {
                    // 已启用但配置不全：提示缺什么
                    state.relay.set_status(&app, "⚪ 配置不完整（需服务器地址 / 分组 ID / 接入密钥）");
                } else {
                    state.relay.set_status(&app, "⚪ 未启用");
                }
                backoff = RECONNECT_MIN;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            if gen != last_gen {
                backoff = RECONNECT_MIN;
                last_gen = gen;
            }

            run_connection(&app, &url, &group, &key, gen).await;

            // 配置在连接期间被改过 → 立即用新配置重连，不退避
            if app.state::<AppState>().relay.gen.load(Ordering::SeqCst) != gen {
                continue;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
    });
}

// ---------- 命令 ----------

#[derive(Serialize)]
pub struct RelayStateDto {
    enabled: bool,
    server_url: String,
    group_id: String,
    access_key: String,
    status: String,
    peers: Vec<PeerDto>,
}

#[tauri::command]
pub fn relay_get_state(state: State<AppState>) -> RelayStateDto {
    let s = state.relay.settings.lock().unwrap().clone();
    RelayStateDto {
        enabled: s.enabled,
        server_url: s.server_url,
        group_id: s.group_id,
        access_key: s.access_key,
        status: state.relay.status.lock().unwrap().clone(),
        peers: state.relay.peers.lock().unwrap().clone(),
    }
}

#[derive(Deserialize)]
pub struct RelaySettingsPatch {
    enabled: Option<bool>,
    server_url: Option<String>,
    group_id: Option<String>,
    access_key: Option<String>,
}

#[tauri::command]
pub fn relay_update_settings(
    app: AppHandle,
    state: State<AppState>,
    patch: RelaySettingsPatch,
) -> Result<(), String> {
    {
        let mut s = state.relay.settings.lock().unwrap();
        if let Some(v) = patch.enabled {
            s.enabled = v;
        }
        if let Some(v) = patch.server_url {
            s.server_url = v.trim().to_string();
        }
        if let Some(v) = patch.group_id {
            s.group_id = v.trim().to_string();
        }
        if let Some(v) = patch.access_key {
            s.access_key = v.trim().to_string();
        }
    }
    // 代际 +1：连接循环感知后自动重连
    state.relay.gen.fetch_add(1, Ordering::SeqCst);
    crate::persist_config(&state)?;
    let _ = app.emit("relay-state-changed", ());
    Ok(())
}
