//! 局域网多设备同步。
//!
//! 协议与安卓端「剪贴板管家（ClipDitto）」完全对齐，可与安卓 / Windows / macOS 互相同步：
//! - 本机 HTTP 服务（默认端口 8765）：GET /info、/pair、/unpair、/unblocked、/blocked、
//!   /clips?since=、/file?id=
//! - 设备发现：UDP 广播 beacon（8766）+ 组播 beacon（239.255.60.60:8767），每 3 秒一次
//! - 配对码（6 位数字）鉴权；请求方带 X-My-Token 实现"一方配对，双方生效"的自动反向配对
//! - 环回防护：记录入库时带 remote_device_id / remote_id，不回传本来就来自对方的记录
//! - 线上时间戳为毫秒（对齐安卓端），本机内部为秒，出入线时换算
//!
//! 为不引入新依赖，HTTP 服务/客户端均用 std 裸 socket 实现（与安卓端做法一致）。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{encode_png, exe_dir, hash_bytes, now_secs, AppState};

// ---------- 常量 ----------

pub const DEFAULT_PORT: u16 = 8765;
const BEACON_PORT: u16 = 8766;
const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 60, 60);
const MULTICAST_PORT: u16 = 8767;
const BEACON_INTERVAL: Duration = Duration::from_secs(3);
const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(30);
/// 设备在线判定窗口：beacon 周期 3s，放宽到 12s
const ONLINE_WINDOW: Duration = Duration::from_secs(12);
/// 拉取远端文件的大小上限（防止超大文件打爆内存）
const MAX_REMOTE_FILE: u64 = 100 * 1024 * 1024;
/// 单次 /clips 响应的最大条数
const CLIPS_PAGE_LIMIT: i64 = 500;

// 线上记录类型（与安卓端 ClipType 一致）
const WIRE_TEXT: i64 = 0;
const WIRE_IMAGE: i64 = 1;
// 2/3/4（文件/视频/音频）统一按文件处理（import_clip 的 fallthrough 分支）

// ---------- 数据模型 ----------

/// 局域网中的一台设备。可能来自实时发现（在线），也可能来自本地配对记录（可能离线）。
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct LanDevice {
    pub device_id: String,
    pub name: String,
    pub model: Option<String>,
    pub host: Option<String>,
    pub port: u16,
    /// 对方是否开启了剪贴板共享
    pub sharing: bool,
    /// 是否已与本机完成配对（本机持有对方配对码）
    pub paired: bool,
    /// 本机保存的对方配对码
    pub token: Option<String>,
}

/// 黑名单条目（存名称/配对码/地址，方便管理页展示和解除时通知对方）
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct BlockedInfo {
    pub name: String,
    pub model: Option<String>,
    pub token: Option<String>,
    pub host: Option<String>,
    pub port: u16,
}

/// 局域网同步的全部持久化状态（独立文件 lscopy-lan.json，与 AppConfig 分开）
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct LanSettings {
    /// 可被其他设备扫描到（控制 beacon 广播）
    pub discoverable: bool,
    /// 允许其他已配对设备拉取本机剪贴板
    pub sharing: bool,
    /// 自动把已配对设备的内容同步到本机（30s 轮询）
    pub auto_sync: bool,
    /// 自动同意配对请求：关闭时（默认）每次配对需在设置页手动确认
    pub auto_accept_pair: bool,
    /// 本机设备唯一标识，首次启动生成后固定不变
    pub device_id: String,
    /// 本机设备名，显示在别人的设备列表里
    pub device_name: String,
    /// 配对码（6 位数字）：其他设备拉取本机内容时需提供
    pub pairing_token: String,
    /// 本机 HTTP 服务端口
    pub server_port: u16,
    /// 已配对设备（含离线），key = deviceId
    pub paired: HashMap<String, LanDevice>,
    /// 黑名单（被本机拉黑的设备）
    pub blocked: HashMap<String, BlockedInfo>,
    /// 「拉黑我的」设备集合：扫描过滤 + 操作拦截用
    pub blocked_by: HashSet<String>,
    /// 每台设备上次同步到的毫秒时间戳（增量拉取游标）
    pub last_sync: HashMap<String, i64>,
}

impl Default for LanSettings {
    fn default() -> Self {
        Self {
            discoverable: false,
            sharing: false,
            auto_sync: false,
            auto_accept_pair: false,
            device_id: String::new(),
            device_name: String::new(),
            pairing_token: String::new(),
            server_port: DEFAULT_PORT,
            paired: HashMap::new(),
            blocked: HashMap::new(),
            blocked_by: HashSet::new(),
            last_sync: HashMap::new(),
        }
    }
}

/// 运行时发现的一台设备及其最后一次 beacon 时间
#[derive(Clone)]
pub(crate) struct Discovered {
    device: LanDevice,
    last_seen: Instant,
}

/// LAN 模块的共享状态（挂在 AppState 上）
pub struct LanShared {
    pub file: PathBuf,
    pub settings: Mutex<LanSettings>,
    /// 在线发现的设备，key = deviceId
    pub discovered: Mutex<HashMap<String, Discovered>>,
    /// 等待用户确认的配对请求：deviceId → 答复通道
    pub pending_pairs: Mutex<HashMap<String, mpsc::Sender<bool>>>,
    pub server_running: AtomicBool,
    /// 实际监听端口（配置端口被占用时由系统分配）
    pub actual_port: AtomicU64,
    /// 正在同步中的设备 id 集合（UI 显示用）
    pub syncing: Mutex<HashSet<String>>,
    /// 「状态变化」事件节流
    last_emit: Mutex<Instant>,
}

impl LanShared {
    pub fn new(file: PathBuf, settings: LanSettings) -> Self {
        Self {
            file,
            settings: Mutex::new(settings),
            discovered: Mutex::new(HashMap::new()),
            pending_pairs: Mutex::new(HashMap::new()),
            server_running: AtomicBool::new(false),
            actual_port: AtomicU64::new(0),
            syncing: Mutex::new(HashSet::new()),
            last_emit: Mutex::new(Instant::now() - Duration::from_secs(10)),
        }
    }
}

// ---------- 随机数 / 默认值 ----------

fn rand_u64() -> u64 {
    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::thread::current().id().hash(&mut h);
    let marker = 0u8;
    (&marker as *const u8 as usize).hash(&mut h);
    h.finish()
}

fn new_device_id() -> String {
    let a = rand_u64();
    let b = rand_u64();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a as u16,
        (b >> 48) as u16,
        b & 0xFFFFFFFFFFFF
    )
}

fn new_token() -> String {
    format!("{:06}", rand_u64() % 900_000 + 100_000)
}

fn default_device_name() -> String {
    #[cfg(target_family = "windows")]
    {
        if let Ok(n) = std::env::var("COMPUTERNAME") {
            if !n.trim().is_empty() {
                return n.trim().to_string();
            }
        }
        "Windows 电脑".into()
    }
    #[cfg(target_os = "macos")]
    {
        "Mac".into()
    }
    #[cfg(all(not(target_family = "windows"), not(target_os = "macos")))]
    {
        "我的电脑".into()
    }
}

fn device_model() -> String {
    #[cfg(target_family = "windows")]
    {
        "Windows PC".into()
    }
    #[cfg(target_os = "macos")]
    {
        "Mac".into()
    }
    #[cfg(all(not(target_family = "windows"), not(target_os = "macos")))]
    {
        "PC".into()
    }
}

// ---------- 持久化 ----------

pub fn settings_file() -> PathBuf {
    exe_dir().join("lscopy-lan.json")
}

pub fn load_settings(path: &PathBuf) -> LanSettings {
    let mut s: LanSettings = std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();
    // 首次启动：补全设备标识 / 名称 / 配对码
    let mut dirty = false;
    if s.device_id.is_empty() {
        s.device_id = new_device_id();
        dirty = true;
    }
    if s.device_name.is_empty() {
        s.device_name = default_device_name();
        dirty = true;
    }
    if s.pairing_token.is_empty() {
        s.pairing_token = new_token();
        dirty = true;
    }
    if s.server_port == 0 {
        s.server_port = DEFAULT_PORT;
        dirty = true;
    }
    if dirty {
        let _ = save_settings_file(path, &s);
    }
    s
}

fn save_settings_file(path: &PathBuf, s: &LanSettings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

fn save_settings(state: &AppState) {
    let s = state.lan.settings.lock().unwrap();
    let _ = save_settings_file(&state.lan.file, &s);
}

// ---------- 状态变化事件（节流 1s） ----------

pub fn emit_state_changed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut last = state.lan.last_emit.lock().unwrap();
    if last.elapsed() < Duration::from_secs(1) {
        return;
    }
    *last = Instant::now();
    drop(last);
    let _ = app.emit("lan-state-changed", ());
}

// ---------- 工具：URL 编解码 ----------

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                out.push(v);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for part in q.split('&') {
        if let Some(idx) = part.find('=') {
            if idx > 0 {
                m.insert(url_decode(&part[..idx]), url_decode(&part[idx + 1..]));
            }
        }
    }
    m
}

// ---------- 极简 HTTP 服务 ----------

fn http_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Error",
    }
}

fn respond(stream: &mut TcpStream, code: u16, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        code,
        http_reason(code),
        content_type,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond_json(stream: &mut TcpStream, code: u16, body: &str) {
    respond(stream, code, "application/json", body.as_bytes());
}

/// 读取 HTTP 请求行 + 头部（只支持 GET，无 body）。返回 (path, query, headers)
fn read_request(stream: &mut TcpStream) -> Option<(String, String, HashMap<String, String>)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    // 读到头部结束标记 \r\n\r\n 为止
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" {
        return Some(("__method_not_allowed__".into(), String::new(), HashMap::new()));
    }
    let target = parts.next()?;
    let path = target.split('?').next().unwrap_or("").to_string();
    let query = target.split('?').nth(1).unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some(idx) = line.find(':') {
            headers.insert(
                line[..idx].trim().to_lowercase(),
                line[idx + 1..].trim().to_string(),
            );
        }
    }
    Some((path, query, headers))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn start_server(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.lan.server_running.swap(true, Ordering::SeqCst) {
        return;
    }
    let port = state.lan.settings.lock().unwrap().server_port;
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[lan] 端口 {port} 被占用（{e}），改用系统分配端口");
            match TcpListener::bind(("0.0.0.0", 0)) {
                Ok(l) => l,
                Err(e2) => {
                    eprintln!("[lan] 服务启动失败: {e2}");
                    state.lan.server_running.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
    };
    let actual = listener.local_addr().map(|a| a.port() as u64).unwrap_or(0);
    state.lan.actual_port.store(actual, Ordering::SeqCst);
    eprintln!("[lan] 局域网服务已启动，端口 {actual}");
    let app2 = app.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let st = app2.state::<AppState>();
            if !st.lan.server_running.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(s) = stream {
                let app3 = app2.clone();
                std::thread::spawn(move || handle_conn(app3, s));
            }
        }
    });
}

fn stop_server(app: &AppHandle) {
    let state = app.state::<AppState>();
    if !state.lan.server_running.swap(false, Ordering::SeqCst) {
        return;
    }
    // 自连接唤醒阻塞中的 accept，让服务线程检查退出标记
    let port = state.lan.actual_port.load(Ordering::SeqCst) as u16;
    if port > 0 {
        let _ = TcpStream::connect(("127.0.0.1", port));
    }
    state.lan.actual_port.store(0, Ordering::SeqCst);
}

/// 开关变化后调用：按需启停服务
pub fn apply_state(app: &AppHandle) {
    let (sharing, discoverable) = {
        let state = app.state::<AppState>();
        let s = state.lan.settings.lock().unwrap();
        (s.sharing, s.discoverable)
    };
    // 共享或可被发现时都需要服务（被发现后对方会请求 /info）
    if sharing || discoverable {
        start_server(app);
    } else {
        stop_server(app);
    }
    emit_state_changed(app);
}

fn handle_conn(app: AppHandle, mut stream: TcpStream) {
    let client_ip = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let Some((path, query, headers)) = read_request(&mut stream) else {
        return;
    };
    if path == "__method_not_allowed__" {
        respond_json(&mut stream, 405, "Method Not Allowed");
        return;
    }
    let query = parse_query(&query);
    match path.as_str() {
        "/info" => handle_info(&app, &mut stream, &headers),
        "/pair" => handle_pair(&app, &mut stream, &headers, &client_ip),
        "/unpair" => handle_unpair(&app, &mut stream, &headers),
        "/unblocked" => handle_unblocked(&app, &mut stream, &headers),
        "/blocked" => handle_blocked_notice(&app, &mut stream, &headers, &client_ip),
        "/clips" => handle_clips(&app, &mut stream, &headers, &query, &client_ip),
        "/file" => handle_file(&app, &mut stream, &headers, &query, &client_ip),
        _ => respond_json(&mut stream, 404, "Not Found"),
    }
}

// ---------- 服务端：鉴权与公共逻辑 ----------

/// 校验共享开关 + 配对码；失败时已写出错误响应并返回 false
fn authorize(app: &AppHandle, stream: &mut TcpStream, headers: &HashMap<String, String>) -> bool {
    let state = app.state::<AppState>();
    let s = state.lan.settings.lock().unwrap();
    if !s.sharing {
        respond_json(stream, 403, r#"{"error":"sharing_off"}"#);
        return false;
    }
    let token = headers.get("x-token").map(|t| t.as_str());
    if token != Some(s.pairing_token.as_str()) {
        respond_json(stream, 401, r#"{"error":"bad_token"}"#);
        return false;
    }
    true
}

/// 被本机拉黑的设备：拒绝并明确告知（对方收到后自动解除本地配对）
fn reject_if_blocked(
    app: &AppHandle,
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
) -> bool {
    let Some(id) = headers.get("x-device-id") else {
        return false;
    };
    let blocked = app
        .state::<AppState>()
        .lan
        .settings
        .lock()
        .unwrap()
        .blocked
        .contains_key(id);
    if !blocked {
        return false;
    }
    respond_json(stream, 403, r#"{"error":"unpaired"}"#);
    true
}

/// 未配对设备不允许拉取内容（必须在 maybe_auto_pair 之前检查）
fn reject_if_unpaired(
    app: &AppHandle,
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
) -> bool {
    let id = headers.get("x-device-id");
    let state = app.state::<AppState>();
    let paired = state.lan.settings.lock().unwrap();
    if let Some(id) = id {
        if paired.paired.contains_key(id) {
            return false;
        }
    }
    drop(paired);
    respond_json(stream, 403, r#"{"error":"unpaired"}"#);
    true
}

/// 从请求头构造请求方设备信息
fn build_requester(
    app: &AppHandle,
    headers: &HashMap<String, String>,
    client_ip: &str,
) -> Option<LanDevice> {
    let my_id = app.state::<AppState>().lan.settings.lock().unwrap().device_id.clone();
    let device_id = headers.get("x-device-id")?.clone();
    if device_id == my_id {
        return None;
    }
    let their_token = headers.get("x-my-token")?.clone();
    let name = headers
        .get("x-device-name")
        .map(|n| url_decode(n))
        .unwrap_or_else(|| "未知设备".into());
    let model = headers.get("x-device-model").map(|m| url_decode(m));
    let port: u16 = headers.get("x-my-port")?.parse().ok()?;
    Some(LanDevice {
        device_id,
        name,
        model,
        host: Some(client_ip.to_string()),
        port,
        sharing: true,
        paired: true,
        token: Some(their_token),
    })
}

/// 自动反向配对：对方带有效配对码请求时，把它存为已配对设备
fn maybe_auto_pair(app: &AppHandle, headers: &HashMap<String, String>, client_ip: &str) {
    let Some(requester) = build_requester(app, headers, client_ip) else {
        return;
    };
    let state = app.state::<AppState>();
    let mut s = state.lan.settings.lock().unwrap();
    if s.blocked.contains_key(&requester.device_id) {
        return; // 已拉黑，不自动恢复
    }
    let unchanged = s
        .paired
        .get(&requester.device_id)
        .map(|e| {
            e.token == requester.token && e.name == requester.name && e.port == requester.port
        })
        .unwrap_or(false);
    if unchanged {
        return;
    }
    s.paired.insert(requester.device_id.clone(), requester);
    drop(s);
    save_settings(&state);
    emit_state_changed(app);
}

// ---------- 服务端：路由处理 ----------

fn handle_info(app: &AppHandle, stream: &mut TcpStream, headers: &HashMap<String, String>) {
    let s = app.state::<AppState>().lan.settings.lock().unwrap().clone();
    // 黑名单设备连 /info 都拿不到，但告知"被拉黑"让对方在其本地隐藏本机
    if let Some(requester) = headers.get("x-device-id") {
        if s.blocked.contains_key(requester) {
            let body = json!({
                "error": "blocked",
                "deviceId": s.device_id,
                "name": s.device_name,
                "model": device_model(),
            });
            respond_json(stream, 403, &body.to_string());
            return;
        }
    }
    let body = json!({
        "deviceId": s.device_id,
        "name": s.device_name,
        "model": device_model(),
        "sharing": s.sharing,
        "autoAccept": s.auto_accept_pair,
        "version": 1,
    });
    respond_json(stream, 200, &body.to_string());
}

fn handle_pair(
    app: &AppHandle,
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    client_ip: &str,
) {
    // 共享开关必须先开启；配对码校验视「自动同意配对」而定（见下）
    let (sharing_on, auto_accept, my_token) = {
        let state = app.state::<AppState>();
        let s = state.lan.settings.lock().unwrap();
        (s.sharing, s.auto_accept_pair, s.pairing_token.clone())
    };
    if !sharing_on {
        respond_json(stream, 403, r#"{"error":"sharing_off"}"#);
        return;
    }
    let Some(requester) = build_requester(app, headers, client_ip) else {
        respond_json(stream, 400, r#"{"error":"bad_request"}"#);
        return;
    };
    let blocked = app
        .state::<AppState>()
        .lan
        .settings
        .lock()
        .unwrap()
        .blocked
        .contains_key(&requester.device_id);
    let finish = |accepted: bool| {
        let state = app.state::<AppState>();
        let mut s = state.lan.settings.lock().unwrap();
        s.blocked.remove(&requester.device_id);
        s.paired.insert(requester.device_id.clone(), requester.clone());
        drop(s);
        save_settings(&state);
        emit_state_changed(app);
        let _ = accepted;
    };
    // 配对成功响应里带上本机配对码：免码配对（自动同意）的请求方拿到码后，
    // 后续 /clips / /file 仍走正常配对码鉴权，内容与安卓端协议保持一致
    let ok_body = |token: &str| format!(r#"{{"result":"ok","token":"{token}"}}"#);

    // 自动同意配对：免配对码直接通过（被拉黑设备除外，必须手动确认）
    if !blocked && auto_accept {
        finish(true);
        respond_json(stream, 200, &ok_body(&my_token));
        return;
    }
    // 其余情况需校验配对码
    let token = headers.get("x-token").map(|t| t.as_str());
    if token != Some(my_token.as_str()) {
        respond_json(stream, 401, r#"{"error":"bad_token"}"#);
        return;
    }
    // 需要用户手动确认：发事件给设置页，阻塞等待答复（最多 30 秒）
    let answer = ask_pair_approval(app, &requester);
    match answer {
        Some(true) => {
            finish(true);
            respond_json(stream, 200, &ok_body(&my_token));
        }
        Some(false) => respond_json(stream, 409, r#"{"error":"rejected"}"#),
        None => respond_json(stream, 409, r#"{"error":"need_confirm"}"#),
    }
}

/// 配对确认：设置页在前台时弹窗等待用户确认（最多 30 秒），无人应答返回 None
fn ask_pair_approval(app: &AppHandle, requester: &LanDevice) -> Option<bool> {
    let (tx, rx) = mpsc::channel();
    app.state::<AppState>()
        .lan
        .pending_pairs
        .lock()
        .unwrap()
        .insert(requester.device_id.clone(), tx);
    let _ = app.emit(
        "lan-pair-request",
        json!({
            "device_id": requester.device_id,
            "name": requester.name,
            "model": requester.model,
            "host": requester.host,
        }),
    );
    let r = rx.recv_timeout(Duration::from_secs(30)).ok();
    app.state::<AppState>()
        .lan
        .pending_pairs
        .lock()
        .unwrap()
        .remove(&requester.device_id);
    r
}

fn handle_unpair(app: &AppHandle, stream: &mut TcpStream, headers: &HashMap<String, String>) {
    // 只校验配对码（共享开关不影响解除配对）
    let ok = {
        let state = app.state::<AppState>();
        let s = state.lan.settings.lock().unwrap();
        headers.get("x-token").map(|t| t.as_str()) == Some(s.pairing_token.as_str())
    };
    if !ok {
        respond_json(stream, 401, r#"{"error":"bad_token"}"#);
        return;
    }
    let state = app.state::<AppState>();
    let my_id = state.lan.settings.lock().unwrap().device_id.clone();
    let Some(device_id) = headers.get("x-device-id").cloned() else {
        respond_json(stream, 400, r#"{"error":"bad_request"}"#);
        return;
    };
    if device_id.is_empty() || device_id == my_id {
        respond_json(stream, 400, r#"{"error":"bad_request"}"#);
        return;
    }
    state.lan.settings.lock().unwrap().paired.remove(&device_id);
    save_settings(&state);
    emit_state_changed(app);
    respond_json(stream, 200, r#"{"result":"ok"}"#);
}

fn handle_unblocked(app: &AppHandle, stream: &mut TcpStream, headers: &HashMap<String, String>) {
    let ok = {
        let state = app.state::<AppState>();
        let s = state.lan.settings.lock().unwrap();
        headers.get("x-token").map(|t| t.as_str()) == Some(s.pairing_token.as_str())
    };
    if !ok {
        respond_json(stream, 401, r#"{"error":"bad_token"}"#);
        return;
    }
    let state = app.state::<AppState>();
    let my_id = state.lan.settings.lock().unwrap().device_id.clone();
    let Some(device_id) = headers.get("x-device-id").cloned() else {
        respond_json(stream, 400, r#"{"error":"bad_request"}"#);
        return;
    };
    if device_id.is_empty() || device_id == my_id {
        respond_json(stream, 400, r#"{"error":"bad_request"}"#);
        return;
    }
    state.lan.settings.lock().unwrap().blocked_by.remove(&device_id);
    save_settings(&state);
    emit_state_changed(app);
    respond_json(stream, 200, r#"{"result":"ok"}"#);
}

/// 对方通知"你已被我拉黑"：回连对方 /info 验证确实被拒后才生效，防止伪造
fn handle_blocked_notice(
    app: &AppHandle,
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    client_ip: &str,
) {
    let state = app.state::<AppState>();
    let my_id = state.lan.settings.lock().unwrap().device_id.clone();
    let device_id = headers.get("x-device-id").cloned().unwrap_or_default();
    let port: u16 = headers
        .get("x-my-port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    if device_id.is_empty() || device_id == my_id || port == 0 {
        respond_json(stream, 400, r#"{"error":"bad_request"}"#);
        return;
    }
    let verified = matches!(fetch_info(client_ip, port, Duration::from_secs(2)), Err(LanErr::Blocked(_)));
    if verified {
        let mut s = state.lan.settings.lock().unwrap();
        s.paired.remove(&device_id);
        s.blocked_by.insert(device_id.clone());
        drop(s);
        save_settings(&state);
        state.lan.discovered.lock().unwrap().remove(&device_id);
        emit_state_changed(app);
    }
    let body = format!(r#"{{"result":"ok","verified":{verified}}}"#);
    respond_json(stream, 200, &body);
}

fn handle_clips(
    app: &AppHandle,
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
    client_ip: &str,
) {
    if !authorize(app, stream, headers) {
        return;
    }
    if reject_if_blocked(app, stream, headers) {
        return;
    }
    if reject_if_unpaired(app, stream, headers) {
        return;
    }
    maybe_auto_pair(app, headers, client_ip);

    let since_ms: i64 = query
        .get("since")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let requester = headers.get("x-device-id").cloned().unwrap_or_default();
    let state = app.state::<AppState>();
    let my_id = state.lan.settings.lock().unwrap().device_id.clone();
    let db = state.db.lock().unwrap();
    let mut stmt = match db.prepare(
        "SELECT id, kind, content, LENGTH(image), created_at, remote_device_id, remote_id
         FROM clips WHERE created_at * 1000 > ?1
         ORDER BY created_at LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => {
            respond_json(stream, 200, "[]");
            return;
        }
    };
    let rows = stmt.query_map(params![since_ms, CLIPS_PAGE_LIMIT], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<i64>>(6)?,
        ))
    });
    let mut list = Vec::new();
    if let Ok(mapped) = rows {
        for row in mapped.flatten() {
            let (id, kind, content, img_len, created_at, remote_dev, remote_id) = row;
            // 文件记录是本机路径，对其他设备没有意义，不同步
            if kind == "file" {
                continue;
            }
            // 环回防护：不回传"本来就来自请求方"的记录
            let origin = remote_dev.clone().unwrap_or_else(|| my_id.clone());
            if origin == requester {
                continue;
            }
            let wire_type = if kind == "image" { WIRE_IMAGE } else { WIRE_TEXT };
            let mut o = json!({
                "id": id,
                "type": wire_type,
                "text": content,
                "timestamp": created_at * 1000,
                "remoteDeviceId": origin,
                "remoteId": remote_id.unwrap_or(id),
            });
            if wire_type == WIRE_IMAGE {
                if let Some(len) = img_len {
                    o["fileName"] = json!(format!("clip_{id}.png"));
                    o["fileSize"] = json!(len);
                }
            }
            list.push(o);
        }
    }
    respond_json(stream, 200, &Value::Array(list).to_string());
}

fn handle_file(
    app: &AppHandle,
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
    client_ip: &str,
) {
    if !authorize(app, stream, headers) {
        return;
    }
    if reject_if_blocked(app, stream, headers) {
        return;
    }
    if reject_if_unpaired(app, stream, headers) {
        return;
    }
    maybe_auto_pair(app, headers, client_ip);

    let id: i64 = match query.get("id").and_then(|v| v.parse().ok()) {
        Some(v) => v,
        None => {
            respond_json(stream, 400, r#"{"error":"bad_request"}"#);
            return;
        }
    };
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap();
    let img: Option<Vec<u8>> = db
        .query_row("SELECT image FROM clips WHERE id=?1", params![id], |r| {
            r.get(0)
        })
        .ok()
        .flatten();
    drop(db);
    match img {
        Some(bytes) => respond(stream, 200, "application/octet-stream", &bytes),
        None => respond_json(stream, 404, "File Not Found"),
    }
}

// ---------- 极简 HTTP 客户端 ----------

#[derive(Debug)]
pub enum LanErr {
    Net(String),
    Http(u16),
    /// 配对码错误或缺失
    NeedPairing,
    /// 对方关闭了共享
    SharingOff,
    /// 对方拒绝了配对
    Rejected,
    /// 对方需要手动确认，但无人应答
    NeedConfirm,
    /// 对方已取消与本机的配对
    Unpaired,
    /// 对方把本机加入了黑名单
    Blocked(Option<String>),
}

impl std::fmt::Display for LanErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LanErr::Net(e) => format!("连接失败: {e}"),
            LanErr::Http(c) => format!("HTTP {c}"),
            LanErr::NeedPairing => "配对码错误".into(),
            LanErr::SharingOff => "对方未开启共享".into(),
            LanErr::Rejected => "对方拒绝了配对".into(),
            LanErr::NeedConfirm => "等待对方确认超时".into(),
            LanErr::Unpaired => "对方已解除配对".into(),
            LanErr::Blocked(_) => "已被对方拉黑".into(),
        };
        write!(f, "{s}")
    }
}

struct HttpResp {
    code: u16,
    body: Vec<u8>,
}

/// 发起 GET 请求，自动带本机设备信息头；带 token 时附上本机配对码（供自动反向配对）
fn http_get(
    app: &AppHandle,
    host: &str,
    port: u16,
    path: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Result<HttpResp, LanErr> {
    let (my_id, my_name, my_token, my_port) = {
        let state = app.state::<AppState>();
        let s = state.lan.settings.lock().unwrap();
        (
            s.device_id.clone(),
            s.device_name.clone(),
            s.pairing_token.clone(),
            state.lan.actual_port.load(Ordering::SeqCst) as u16,
        )
    };
    http_get_raw(
        host,
        port,
        path,
        &my_id,
        &my_name,
        my_port,
        token,
        // X-My-Token 始终携带：免码配对（对方开了自动同意）时也要支持自动反向配对
        Some(my_token.as_str()),
        timeout,
    )
}

#[allow(clippy::too_many_arguments)]
fn http_get_raw(
    host: &str,
    port: u16,
    path: &str,
    my_id: &str,
    my_name: &str,
    my_port: u16,
    token: Option<&str>,
    my_token: Option<&str>,
    timeout: Duration,
) -> Result<HttpResp, LanErr> {
    let addr = format!("{host}:{port}")
        .as_str()
        .to_socket_addrs()
        .map_err(|e| LanErr::Net(e.to_string()))?
        .next()
        .ok_or_else(|| LanErr::Net("地址解析失败".into()))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| LanErr::Net(e.to_string()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30).max(timeout)))
        .ok();
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nX-Device-Id: {my_id}\r\nX-Device-Name: {}\r\nX-Device-Model: {}\r\n",
        url_encode(my_name),
        url_encode(&device_model()),
    );
    if my_port > 0 {
        req.push_str(&format!("X-My-Port: {my_port}\r\n"));
    }
    if let Some(t) = token {
        req.push_str(&format!("X-Token: {t}\r\n"));
    }
    if let Some(mt) = my_token {
        req.push_str(&format!("X-My-Token: {mt}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| LanErr::Net(e.to_string()))?;

    // 读响应头
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(LanErr::Net("响应头过大".into()));
        }
        let n = stream.read(&mut tmp).map_err(|e| LanErr::Net(e.to_string()))?;
        if n == 0 {
            return Err(LanErr::Net("连接被关闭".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut body = buf.split_off(head_end + 4);
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut content_len: Option<u64> = None;
    for line in lines {
        if let Some(idx) = line.find(':') {
            if line[..idx].trim().eq_ignore_ascii_case("content-length") {
                content_len = line[idx + 1..].trim().parse().ok();
            }
        }
    }
    // 读响应体
    match content_len {
        Some(len) => {
            let len = len as usize;
            while body.len() < len {
                let n = stream.read(&mut tmp).map_err(|e| LanErr::Net(e.to_string()))?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            body.truncate(len);
        }
        None => loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        },
    }
    Ok(HttpResp { code, body })
}

/// 解析 403 响应体：区分"对方关了共享"/"对方取消了配对"/"对方拉黑了本机"
fn forbidden(resp: &HttpResp) -> LanErr {
    let err = serde_json::from_slice::<Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()));
    match err.as_deref() {
        Some("unpaired") => LanErr::Unpaired,
        Some("blocked") => {
            let id = serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| {
                    v.get("deviceId")
                        .and_then(|d| d.as_str())
                        .map(|s| s.to_string())
                });
            LanErr::Blocked(id)
        }
        _ => LanErr::SharingOff,
    }
}

/// 设备信息（无需配对码）。不经过 AppHandle，供服务端回连验证使用
fn fetch_info(host: &str, port: u16, timeout: Duration) -> Result<Value, LanErr> {
    let resp = http_get_raw(host, port, "/info", "", "", 0, None, None, timeout)?;
    match resp.code {
        200 => serde_json::from_slice(&resp.body)
            .map_err(|e| LanErr::Net(format!("响应解析失败: {e}"))),
        403 => {
            let id = serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| {
                    v.get("deviceId")
                        .and_then(|d| d.as_str())
                        .map(|s| s.to_string())
                });
            Err(LanErr::Blocked(id))
        }
        c => Err(LanErr::Http(c)),
    }
}

/// 增量拉取记录
fn fetch_clips(app: &AppHandle, device: &LanDevice, since: i64) -> Result<Vec<Value>, LanErr> {
    let host = device.host.clone().ok_or_else(|| LanErr::Net("设备离线".into()))?;
    let resp = http_get(
        app,
        &host,
        device.port,
        &format!("/clips?since={since}"),
        device.token.as_deref(),
        Duration::from_secs(5),
    )?;
    match resp.code {
        200 => {
            let v: Value = serde_json::from_slice(&resp.body)
                .map_err(|e| LanErr::Net(format!("响应解析失败: {e}")))?;
            Ok(v.as_array().cloned().unwrap_or_default())
        }
        401 => Err(LanErr::NeedPairing),
        403 => Err(forbidden(&resp)),
        c => Err(LanErr::Http(c)),
    }
}

/// 发起配对请求（对方可能弹窗手动确认，读超时放宽到 35 秒）
/// 发起配对请求（对方可能弹窗手动确认，读超时放宽到 35 秒）。
/// 成功时返回对方在响应里附带的配对码（免码配对场景：拿到码供后续 /clips 鉴权）。
fn request_pair(app: &AppHandle, device: &LanDevice) -> Result<Option<String>, LanErr> {
    let host = device.host.clone().ok_or_else(|| LanErr::Net("设备离线".into()))?;
    let resp = http_get(
        app,
        &host,
        device.port,
        "/pair",
        device.token.as_deref(),
        Duration::from_secs(35),
    )?;
    match resp.code {
        200 => {
            let token = serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(String::from));
            Ok(token)
        }
        401 => Err(LanErr::NeedPairing),
        403 => Err(forbidden(&resp)),
        409 => {
            let err = serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()));
            match err.as_deref() {
                Some("rejected") => Err(LanErr::Rejected),
                _ => Err(LanErr::NeedConfirm),
            }
        }
        c => Err(LanErr::Http(c)),
    }
}

/// 下载媒体文件（内存返回，调用方负责落盘/入库）
fn download_file(app: &AppHandle, device: &LanDevice, remote_id: i64) -> Result<Vec<u8>, LanErr> {
    let host = device.host.clone().ok_or_else(|| LanErr::Net("设备离线".into()))?;
    let resp = http_get(
        app,
        &host,
        device.port,
        &format!("/file?id={remote_id}"),
        device.token.as_deref(),
        Duration::from_secs(5),
    )?;
    match resp.code {
        200 => Ok(resp.body),
        401 => Err(LanErr::NeedPairing),
        403 => Err(forbidden(&resp)),
        c => Err(LanErr::Http(c)),
    }
}

/// 主动通知对方：本机已取消配对（尽力而为，对方不在线时静默失败）
fn notify_unpair(app: &AppHandle, device: &LanDevice) {
    if let Some(host) = &device.host {
        let _ = http_get(
            app,
            host,
            device.port,
            "/unpair",
            device.token.as_deref(),
            Duration::from_secs(3),
        );
    }
}

/// 主动通知对方：本机已把你移出黑名单
fn notify_unblocked(app: &AppHandle, info: &BlockedInfo, device_id: &str) {
    if let (Some(host), Some(token)) = (&info.host, &info.token) {
        let d = LanDevice {
            device_id: device_id.to_string(),
            name: info.name.clone(),
            host: Some(host.clone()),
            port: info.port,
            token: Some(token.clone()),
            ..Default::default()
        };
        let _ = http_get(app, host, d.port, "/unblocked", d.token.as_deref(), Duration::from_secs(3));
    }
}

/// 主动通知对方"你已被本机拉黑"（免配对码，对方回连验证后生效）
fn notify_blocked(app: &AppHandle, host: &str, port: u16) {
    let _ = http_get(app, host, port, "/blocked", None, Duration::from_secs(3));
}

// ---------- UDP beacon 发现 ----------

fn beacon_payload(app: &AppHandle) -> Option<Vec<u8>> {
    let state = app.state::<AppState>();
    let s = state.lan.settings.lock().unwrap();
    if !s.discoverable || !state.lan.server_running.load(Ordering::SeqCst) {
        return None;
    }
    let port = state.lan.actual_port.load(Ordering::SeqCst);
    let body = json!({
        "deviceId": s.device_id,
        "name": s.device_name,
        "model": device_model(),
        "port": port,
        "sharing": s.sharing,
    });
    Some(body.to_string().into_bytes())
}

/// beacon 发送线程：「可被发现」开启时每 3 秒广播一次本机信息（广播 + 组播双发）
fn beacon_sender_loop(app: AppHandle) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[lan] beacon 发送 socket 创建失败: {e}");
            return;
        }
    };
    let _ = socket.set_broadcast(true);
    loop {
        if let Some(payload) = beacon_payload(&app) {
            let targets = [
                ("255.255.255.255", BEACON_PORT),
                ("239.255.60.60", MULTICAST_PORT),
            ];
            for (addr, port) in targets {
                let _ = socket.send_to(&payload, (addr, port));
            }
        }
        std::thread::sleep(BEACON_INTERVAL);
    }
}

/// beacon 接收线程：监听广播（8766）或组播（8767）
fn beacon_receiver_loop(app: AppHandle, multicast: bool) {
    let socket = if multicast {
        let s = match UdpSocket::bind(("0.0.0.0", MULTICAST_PORT)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[lan] 组播 beacon 监听失败: {e}");
                return;
            }
        };
        if let Err(e) = s.join_multicast_v4(&MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED) {
            eprintln!("[lan] 加入组播组失败: {e}");
            return;
        }
        s
    } else {
        match UdpSocket::bind(("0.0.0.0", BEACON_PORT)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[lan] 广播 beacon 监听失败: {e}");
                return;
            }
        }
    };
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Ok(json) = serde_json::from_slice::<Value>(&buf[..n]) else {
            continue;
        };
        let Some(device_id) = json.get("deviceId").and_then(|v| v.as_str()) else {
            continue;
        };
        let state = app.state::<AppState>();
        let my_id = state.lan.settings.lock().unwrap().device_id.clone();
        if device_id == my_id {
            continue; // 自己的广播
        }
        let Some(port) = json.get("port").and_then(|v| v.as_u64()) else {
            continue;
        };
        // 本机黑名单 / "拉黑了本机"的设备不进列表
        {
            let s = state.lan.settings.lock().unwrap();
            if s.blocked.contains_key(device_id) || s.blocked_by.contains(device_id) {
                continue;
            }
        }
        let device = LanDevice {
            device_id: device_id.to_string(),
            name: json
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("未知设备")
                .to_string(),
            model: json.get("model").and_then(|v| v.as_str()).map(String::from),
            host: Some(from.ip().to_string()),
            port: port as u16,
            sharing: json.get("sharing").and_then(|v| v.as_bool()).unwrap_or(false),
            paired: false,
            token: None,
        };
        state
            .lan
            .discovered
            .lock()
            .unwrap()
            .insert(device_id.to_string(), Discovered { device, last_seen: Instant::now() });
        emit_state_changed(&app);
    }
}

// ---------- 设备列表合并 ----------

/// 合并配对设备（可能离线）+ 在线发现，过滤黑名单，按在线优先排序
fn merged_devices(state: &AppState) -> Vec<(LanDevice, bool, i64)> {
    let now = Instant::now();
    let mut discovered = state.lan.discovered.lock().unwrap();
    discovered.retain(|_, d| now.duration_since(d.last_seen) < ONLINE_WINDOW);
    let online: HashMap<String, LanDevice> = discovered
        .iter()
        .map(|(k, d)| (k.clone(), d.device.clone()))
        .collect();
    drop(discovered);

    let mut s = state.lan.settings.lock().unwrap();
    let blocked: HashSet<String> = s
        .blocked
        .keys()
        .chain(s.blocked_by.iter())
        .cloned()
        .collect();
    let mut merged: HashMap<String, (LanDevice, bool)> = HashMap::new();
    for (id, p) in &s.paired {
        if blocked.contains(id) {
            continue;
        }
        merged.insert(id.clone(), (p.clone(), false));
    }
    for (id, o) in &online {
        if blocked.contains(id) {
            continue;
        }
        let entry = merged
            .entry(id.clone())
            .or_insert_with(|| (o.clone(), false));
        entry.1 = true;
        // 在线设备的最新信息回写到配对记录（DHCP 换 IP、对方改名后仍保持最新）
        if let Some(p) = s.paired.get_mut(id) {
            if p.host != o.host || p.port != o.port || p.name != o.name || p.sharing != o.sharing {
                p.host = o.host.clone();
                p.port = o.port;
                p.name = o.name.clone();
                p.model = o.model.clone();
                p.sharing = o.sharing;
            }
        }
        if entry.0.token.is_none() {
            entry.0 = o.clone();
        } else {
            entry.0.host = o.host.clone();
            entry.0.port = o.port;
            entry.0.sharing = o.sharing;
        }
    }
    let syncing = state.lan.syncing.lock().unwrap().clone();
    let _ = syncing;
    let mut out: Vec<(LanDevice, bool, i64)> = merged
        .into_iter()
        .map(|(id, (mut d, online))| {
            d.paired = s.paired.contains_key(&id);
            let last_sync = s.last_sync.get(&id).copied().unwrap_or(0);
            (d, online, last_sync)
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    out
}

// ---------- 同步 ----------

pub struct SyncResult {
    pub added: usize,
    pub skipped: usize,
}

/// 同步一台设备：增量拉取并入库
fn sync_device(app: &AppHandle, device_id: &str) -> Result<SyncResult, LanErr> {
    let state = app.state::<AppState>();
    {
        let s = state.lan.settings.lock().unwrap();
        if s.blocked.contains_key(device_id) || s.blocked_by.contains(device_id) {
            return Err(LanErr::Blocked(None));
        }
    }
    // 用在线发现的最新地址 + 本机保存的配对码
    let device = {
        let discovered = state.lan.discovered.lock().unwrap();
        let s = state.lan.settings.lock().unwrap();
        let token = s
            .paired
            .get(device_id)
            .and_then(|p| p.token.clone())
            .ok_or(LanErr::NeedPairing)?;
        let mut d = discovered
            .get(device_id)
            .map(|d| d.device.clone())
            .or_else(|| s.paired.get(device_id).cloned())
            .ok_or_else(|| LanErr::Net("设备离线".into()))?;
        if d.host.is_none() {
            return Err(LanErr::Net("设备离线".into()));
        }
        d.token = Some(token);
        d
    };
    state.lan.syncing.lock().unwrap().insert(device_id.to_string());
    emit_state_changed(app);
    let result = sync_device_inner(app, device_id, &device);
    state.lan.syncing.lock().unwrap().remove(device_id);
    emit_state_changed(app);
    result
}

fn sync_device_inner(
    app: &AppHandle,
    device_id: &str,
    device: &LanDevice,
) -> Result<SyncResult, LanErr> {
    let state = app.state::<AppState>();
    let since = state
        .lan
        .settings
        .lock()
        .unwrap()
        .last_sync
        .get(device_id)
        .copied()
        .unwrap_or(0);
    let clips = match fetch_clips(app, device, since) {
        Ok(c) => c,
        Err(LanErr::Unpaired) => {
            // 对方已取消与本机的配对：本地同步解除配对状态
            let mut s = state.lan.settings.lock().unwrap();
            s.paired.remove(device_id);
            drop(s);
            save_settings(&state);
            emit_state_changed(app);
            return Err(LanErr::Unpaired);
        }
        Err(e) => return Err(e),
    };
    let mut added = 0;
    let mut skipped = 0;
    let mut max_ts = since;
    for obj in &clips {
        let ts = obj.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        max_ts = max_ts.max(ts);
        match import_clip(app, obj, device) {
            true => added += 1,
            false => skipped += 1,
        }
    }
    {
        let mut s = state.lan.settings.lock().unwrap();
        s.last_sync.insert(device_id.to_string(), max_ts);
    }
    save_settings(&state);
    if added > 0 {
        let _ = app.emit("clip-added", ());
    }
    Ok(SyncResult { added, skipped })
}

/// 导入一条远端记录；返回 true = 新增，false = 去重跳过
fn import_clip(app: &AppHandle, obj: &Value, device: &LanDevice) -> bool {
    let state = app.state::<AppState>();
    let my_id = state.lan.settings.lock().unwrap().device_id.clone();
    let get_str = |key: &str| -> Option<String> {
        obj.get(key)
            .filter(|v| !v.is_null())
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    // 环回防护：内容本来就来自本机
    let origin_device = get_str("remoteDeviceId");
    if origin_device.as_deref() == Some(my_id.as_str()) {
        return false;
    }
    let wire_type = obj.get("type").and_then(|v| v.as_i64()).unwrap_or(WIRE_TEXT);
    let text = get_str("text");
    let timestamp_ms = obj.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
    let created_at = (timestamp_ms / 1000).max(0);
    let remote_id = obj.get("remoteId").and_then(|v| v.as_i64());

    if wire_type == WIRE_TEXT {
        let Some(content) = text else {
            return false;
        };
        if content.is_empty() {
            return false;
        }
        let h = hash_bytes(content.as_bytes());
        let db = state.db.lock().unwrap();
        return store_remote(
            &db,
            "text",
            Some(&content),
            None,
            None,
            None,
            h,
            created_at,
            origin_device.as_deref(),
            remote_id,
        );
    }

    // 图片 / 文件 / 视频 / 音频：先下载，去重后入库
    let Some(file_name) = get_str("fileName") else {
        return false;
    };
    let expected_size = obj.get("fileSize").and_then(|v| v.as_u64()).unwrap_or(0);
    if expected_size > MAX_REMOTE_FILE {
        return false;
    }
    let Some(remote_record_id) = obj.get("id").and_then(|v| v.as_i64()) else {
        return false;
    };
    let bytes = match download_file(app, device, remote_record_id) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if bytes.is_empty() || (expected_size > 0 && bytes.len() as u64 != expected_size) {
        return false;
    }
    let h = hash_bytes(&bytes);
    let db = state.db.lock().unwrap();

    if wire_type == WIRE_IMAGE {
        // 统一转码为 PNG（前端固定按 image/png 渲染），同时取得宽高
        let Ok(img) = image::load_from_memory(&bytes) else {
            return false;
        };
        let Some((png, w, hgt)) = encode_png(img.to_rgba8()) else {
            return false;
        };
        let hp = hash_bytes(&png);
        return store_remote(
            &db,
            "image",
            None,
            Some(&png),
            Some(w),
            Some(hgt),
            hp,
            created_at,
            origin_device.as_deref(),
            remote_id,
        );
    }

    // 文件/视频/音频：落盘到 db 目录下的 lan-files/，以路径形式入库
    let dir = state
        .config
        .lock()
        .unwrap()
        .db_dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(exe_dir)
        .join("lan-files");
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let ext = file_name.rsplit_once('.').map(|(_, e)| e).unwrap_or("bin");
    let dest = dir.join(format!("{}_lan.{}", timestamp_ms, sanitize_ext(ext)));
    if std::fs::write(&dest, &bytes).is_err() {
        return false;
    }
    let path = dest.to_string_lossy().to_string();
    store_remote(
        &db,
        "file",
        Some(&path),
        None,
        None,
        None,
        h,
        created_at,
        origin_device.as_deref(),
        remote_id,
    )
}

fn sanitize_ext(ext: &str) -> String {
    ext.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(10)
        .collect::<String>()
        .to_lowercase()
}

/// 远端记录入库：按内容哈希查重，重复则把时间顶到最前（与安卓端 touch 行为一致）
/// 返回 true = 新增，false = 重复
#[allow(clippy::too_many_arguments)]
fn store_remote(
    db: &rusqlite::Connection,
    kind: &str,
    content: Option<&str>,
    image: Option<&[u8]>,
    width: Option<u32>,
    height: Option<u32>,
    hash: u64,
    created_at: i64,
    remote_device_id: Option<&str>,
    remote_id: Option<i64>,
) -> bool {
    let h64 = hash as i64;
    if let Ok(id) = db.query_row(
        "SELECT id FROM clips WHERE hash = ?1 ORDER BY created_at DESC LIMIT 1",
        params![h64],
        |r| r.get::<_, i64>(0),
    ) {
        let _ = db.execute(
            "UPDATE clips SET created_at = ?2 WHERE id = ?1",
            params![id, now_secs()],
        );
        return false;
    }
    db.execute(
        "INSERT INTO clips(kind, content, image, width, height, hash, created_at, remote_device_id, remote_id)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![kind, content, image, width, height, h64, created_at, remote_device_id, remote_id],
    )
    .is_ok()
}

/// 自动同步线程：30s 一轮，对所有已配对且在线且对方开了共享的设备增量拉取
fn auto_sync_loop(app: AppHandle) {
    loop {
        std::thread::sleep(AUTO_SYNC_INTERVAL);
        let state = app.state::<AppState>();
        if !state.lan.settings.lock().unwrap().auto_sync {
            continue;
        }
        let targets: Vec<String> = merged_devices(&state)
            .into_iter()
            .filter(|(d, online, _)| *online && d.paired && d.sharing)
            .map(|(d, _, _)| d.device_id)
            .collect();
        for id in targets {
            if let Err(e) = sync_device(&app, &id) {
                eprintln!("[lan] 自动同步 {id} 失败: {e}");
            }
        }
    }
}

// ---------- 模块启动 ----------

/// 启动 LAN 模块的全部后台线程（beacon 收发 + 自动同步），并按开关启停服务
pub fn start(app: &AppHandle) {
    let app2 = app.clone();
    std::thread::spawn(move || beacon_receiver_loop(app2, false));
    let app2 = app.clone();
    std::thread::spawn(move || beacon_receiver_loop(app2, true));
    let app2 = app.clone();
    std::thread::spawn(move || beacon_sender_loop(app2));
    let app2 = app.clone();
    std::thread::spawn(move || auto_sync_loop(app2));
    apply_state(app);
}

// ---------- 前端 DTO ----------

#[derive(Serialize)]
pub struct DeviceDto {
    device_id: String,
    name: String,
    model: Option<String>,
    host: Option<String>,
    port: u16,
    sharing: bool,
    paired: bool,
    online: bool,
    last_sync: i64,
    syncing: bool,
}

#[derive(Serialize)]
pub struct BlockedDto {
    device_id: String,
    name: String,
    model: Option<String>,
}

#[derive(Serialize)]
pub struct LanStateDto {
    device_id: String,
    device_name: String,
    pairing_token: String,
    server_port: u16,
    actual_port: u16,
    server_running: bool,
    /// 本机局域网 IP（未联网时为空串）
    local_ip: String,
    discoverable: bool,
    sharing: bool,
    auto_sync: bool,
    auto_accept_pair: bool,
    devices: Vec<DeviceDto>,
    blocked: Vec<BlockedDto>,
}

fn build_state_dto(state: &AppState) -> LanStateDto {
    let s = state.lan.settings.lock().unwrap().clone();
    let syncing = state.lan.syncing.lock().unwrap().clone();
    let devices = merged_devices(state)
        .into_iter()
        .map(|(d, online, last_sync)| DeviceDto {
            syncing: syncing.contains(&d.device_id),
            device_id: d.device_id,
            name: d.name,
            model: d.model,
            host: d.host,
            port: d.port,
            sharing: d.sharing,
            paired: d.paired,
            online,
            last_sync,
        })
        .collect();
    let blocked = s
        .blocked
        .iter()
        .map(|(id, b)| BlockedDto {
            device_id: id.clone(),
            name: b.name.clone(),
            model: b.model.clone(),
        })
        .collect();
    LanStateDto {
        device_id: s.device_id,
        device_name: s.device_name,
        pairing_token: s.pairing_token,
        server_port: s.server_port,
        actual_port: state.lan.actual_port.load(Ordering::SeqCst) as u16,
        server_running: state.lan.server_running.load(Ordering::SeqCst),
        local_ip: local_ip().unwrap_or_default(),
        discoverable: s.discoverable,
        sharing: s.sharing,
        auto_sync: s.auto_sync,
        auto_accept_pair: s.auto_accept_pair,
        devices,
        blocked,
    }
}

// ---------- 命令 ----------

#[tauri::command]
pub fn lan_get_state(state: State<AppState>) -> LanStateDto {
    build_state_dto(&state)
}

#[derive(Deserialize)]
pub struct LanSettingsPatch {
    discoverable: Option<bool>,
    sharing: Option<bool>,
    auto_sync: Option<bool>,
    auto_accept_pair: Option<bool>,
    device_name: Option<String>,
    server_port: Option<u16>,
}

#[tauri::command]
pub fn lan_update_settings(
    app: AppHandle,
    state: State<AppState>,
    patch: LanSettingsPatch,
) -> Result<(), String> {
    let mut port_changed = false;
    {
        let mut s = state.lan.settings.lock().unwrap();
        if let Some(v) = patch.discoverable {
            s.discoverable = v;
        }
        if let Some(v) = patch.sharing {
            s.sharing = v;
        }
        if let Some(v) = patch.auto_sync {
            s.auto_sync = v;
        }
        if let Some(v) = patch.auto_accept_pair {
            s.auto_accept_pair = v;
        }
        if let Some(name) = patch.device_name {
            let name = name.trim().to_string();
            if !name.is_empty() {
                s.device_name = name;
            }
        }
        if let Some(port) = patch.server_port {
            if port > 0 && port != s.server_port {
                s.server_port = port;
                port_changed = true;
            }
        }
    }
    save_settings(&state);
    if port_changed {
        // 端口变更：重启服务
        stop_server(&app);
    }
    apply_state(&app);
    Ok(())
}

/// 重新生成配对码（旧配对码全部失效，已配对设备需用新码重新配对）
#[tauri::command]
pub fn lan_regenerate_token(app: AppHandle, state: State<AppState>) -> String {
    let t = new_token();
    state.lan.settings.lock().unwrap().pairing_token = t.clone();
    save_settings(&state);
    emit_state_changed(&app);
    t
}

/// 手动添加一台设备：直接对指定 IP 请求 /info（依次尝试配置端口与默认端口）
#[tauri::command]
pub async fn lan_add_device(app: AppHandle, ip: String) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ip = ip.trim().to_string();
        if ip.is_empty() {
            return false;
        }
        let port = app.state::<AppState>().lan.settings.lock().unwrap().server_port;
        let ports: Vec<u16> = vec![port, DEFAULT_PORT]
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for p in ports {
            match fetch_info(&ip, p, Duration::from_secs(2)) {
                Ok(info) => {
                    let Some(device_id) = info.get("deviceId").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let state = app.state::<AppState>();
                    let my_id = state.lan.settings.lock().unwrap().device_id.clone();
                    if device_id == my_id {
                        continue;
                    }
                    {
                        let s = state.lan.settings.lock().unwrap();
                        if s.blocked.contains_key(device_id) || s.blocked_by.contains(device_id) {
                            return false;
                        }
                    }
                    let device = LanDevice {
                        device_id: device_id.to_string(),
                        name: info
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&ip)
                            .to_string(),
                        model: info.get("model").and_then(|v| v.as_str()).map(String::from),
                        host: Some(ip.clone()),
                        port: p,
                        sharing: info
                            .get("sharing")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        paired: false,
                        token: None,
                    };
                    state.lan.discovered.lock().unwrap().insert(
                        device_id.to_string(),
                        Discovered { device, last_seen: Instant::now() },
                    );
                    emit_state_changed(&app);
                    return true;
                }
                Err(LanErr::Blocked(Some(id))) => {
                    // 被对方拉黑：记录并从列表移除
                    let state = app.state::<AppState>();
                    if id != state.lan.settings.lock().unwrap().device_id {
                        state.lan.settings.lock().unwrap().blocked_by.insert(id.clone());
                        save_settings(&state);
                        state.lan.discovered.lock().unwrap().remove(&id);
                        emit_state_changed(&app);
                    }
                    return false;
                }
                Err(_) => continue,
            }
        }
        false
    })
    .await
    .map_err(|e| e.to_string())
}

/// 深度扫描：遍历本机所在 /24 网段所有地址，逐个探测 /info
#[tauri::command]
pub async fn lan_sweep(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let Some(my_ip) = local_ip() else {
            return;
        };
        let parts: Vec<&str> = my_ip.split('.').collect();
        if parts.len() != 4 {
            return;
        }
        let prefix = format!("{}.{}.{}", parts[0], parts[1], parts[2]);
        let my_last: u8 = parts[3].parse().unwrap_or(0);
        // 分批探测，避免一次起 254 个连接
        for chunk_start in (1..=254u16).step_by(32) {
            let mut handles = Vec::new();
            for i in chunk_start..=(chunk_start + 31).min(254) {
                if i as u8 == my_last {
                    continue;
                }
                let ip = format!("{prefix}.{i}");
                let app2 = app.clone();
                handles.push(std::thread::spawn(move || {
                    let port = app2.state::<AppState>().lan.settings.lock().unwrap().server_port;
                    let ports: Vec<u16> = vec![port, DEFAULT_PORT]
                        .into_iter()
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect();
                    for p in ports {
                        if let Ok(info) = fetch_info(&ip, p, Duration::from_millis(800)) {
                            let Some(device_id) = info.get("deviceId").and_then(|v| v.as_str())
                            else {
                                break;
                            };
                            let state = app2.state::<AppState>();
                            let skip = {
                                let s = state.lan.settings.lock().unwrap();
                                device_id == s.device_id
                                    || s.blocked.contains_key(device_id)
                                    || s.blocked_by.contains(device_id)
                            };
                            if skip {
                                break;
                            }
                            let device = LanDevice {
                                device_id: device_id.to_string(),
                                name: info
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&ip)
                                    .to_string(),
                                model: info
                                    .get("model")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                host: Some(ip.clone()),
                                port: p,
                                sharing: info
                                    .get("sharing")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false),
                                paired: false,
                                token: None,
                            };
                            state.lan.discovered.lock().unwrap().insert(
                                device_id.to_string(),
                                Discovered { device, last_seen: Instant::now() },
                            );
                            emit_state_changed(&app2);
                            break;
                        }
                    }
                }));
            }
            for h in handles {
                let _ = h.join();
            }
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// 本机局域网 IPv4 地址
fn local_ip() -> Option<String> {
    // 通过 UDP "连接"外网地址的方式拿到本机出口网卡 IP（不会真的发包）
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    let addr = s.local_addr().ok()?;
    let ip = addr.ip().to_string();
    if ip.starts_with("0.") || ip.starts_with("127.") {
        None
    } else {
        Some(ip)
    }
}

/// 用配对码配对一台设备
#[tauri::command]
pub async fn lan_pair(app: AppHandle, device_id: String, token: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        {
            let s = state.lan.settings.lock().unwrap();
            if s.blocked.contains_key(&device_id) {
                return Err("该设备在本机黑名单中，请先移出黑名单".to_string());
            }
            if s.blocked_by.contains(&device_id) {
                return Err("你已被对方拉黑，无法配对".to_string());
            }
        }
        // 用最新发现的设备信息（地址可能已变化）；不在线时先尝试按已知地址刷新
        let device = {
            let discovered = state.lan.discovered.lock().unwrap();
            let s = state.lan.settings.lock().unwrap();
            discovered
                .get(&device_id)
                .map(|d| d.device.clone())
                .or_else(|| s.paired.get(&device_id).cloned())
                .ok_or_else(|| "设备不在线，请先刷新扫描".to_string())?
        };
        let host = device.host.clone().ok_or_else(|| "设备离线".to_string())?;
        // 配对前先探测一次 /info：确认在线，并读取对方是否开了「自动同意配对」
        let info = match fetch_info(&host, device.port, Duration::from_secs(2)) {
            Ok(v) => v,
            Err(e) => return Err(format!("设备不在线，无法连接（{e}）")),
        };
        let auto_accept = info
            .get("autoAccept")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let token = token.trim().to_string();
        // 对方开了自动同意：免配对码；否则必须提供配对码
        if token.is_empty() && !auto_accept {
            return Err("__need_token__".to_string());
        }
        let mut device = device;
        device.token = if token.is_empty() { None } else { Some(token) };
        let granted = request_pair(&app, &device).map_err(|e| e.to_string())?;
        // 免码配对成功时对方会在响应里附带它的配对码，存下来供后续同步鉴权
        if let Some(t) = granted {
            device.token = Some(t);
        }
        // 配对成功：存为已配对设备
        {
            let mut s = state.lan.settings.lock().unwrap();
            s.blocked.remove(&device_id);
            let mut d = device.clone();
            d.paired = true;
            s.paired.insert(device_id.clone(), d);
        }
        save_settings(&state);
        emit_state_changed(&app);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 解除配对：本地解除并尽力通知对方
#[tauri::command]
pub async fn lan_unpair(app: AppHandle, device_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let device = state
            .lan
            .settings
            .lock()
            .unwrap()
            .paired
            .get(&device_id)
            .cloned();
        state.lan.settings.lock().unwrap().paired.remove(&device_id);
        save_settings(&state);
        emit_state_changed(&app);
        if let Some(d) = device {
            notify_unpair(&app, &d);
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// 立即同步一台设备
#[tauri::command]
pub async fn lan_sync_now(app: AppHandle, device_id: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || match sync_device(&app, &device_id) {
        Ok(r) => Ok(format!("同步完成：新增 {} 条，跳过 {} 条", r.added, r.skipped)),
        Err(e) => Err(e.to_string()),
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 把设备加入黑名单：它无法扫描/请求本机，本机也无法操作它
#[tauri::command]
pub async fn lan_block(app: AppHandle, device_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (device, existed) = {
            let discovered = state.lan.discovered.lock().unwrap();
            let mut s = state.lan.settings.lock().unwrap();
            let existed = s.paired.remove(&device_id);
            let d = discovered
                .get(&device_id)
                .map(|d| d.device.clone())
                .or(existed.clone());
            if let Some(d) = &d {
                s.blocked.insert(
                    device_id.clone(),
                    BlockedInfo {
                        name: d.name.clone(),
                        model: d.model.clone(),
                        token: d.token.clone(),
                        host: d.host.clone(),
                        port: d.port,
                    },
                );
            } else {
                s.blocked
                    .insert(device_id.clone(), BlockedInfo::default());
            }
            (d, existed)
        };
        save_settings(&state);
        state.lan.discovered.lock().unwrap().remove(&device_id);
        emit_state_changed(&app);
        // 通知对方：有配对码 → /unpair 让其立即解除本地配对；/blocked 免配对码（对方回连验证后生效）
        if let Some(d) = device.or(existed) {
            if d.token.is_some() {
                notify_unpair(&app, &d);
            }
            if let Some(host) = d.host {
                notify_blocked(&app, &host, d.port);
            }
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// 移出黑名单：本地解除，并尽力通知对方恢复
#[tauri::command]
pub async fn lan_unblock(app: AppHandle, device_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let info = state
            .lan
            .settings
            .lock()
            .unwrap()
            .blocked
            .get(&device_id)
            .cloned();
        state.lan.settings.lock().unwrap().blocked.remove(&device_id);
        save_settings(&state);
        emit_state_changed(&app);
        if let Some(info) = info {
            notify_unblocked(&app, &info, &device_id);
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// 配对确认弹窗的答复
#[tauri::command]
pub fn lan_respond_pair(state: State<AppState>, device_id: String, accept: bool) {
    if let Some(tx) = state.lan.pending_pairs.lock().unwrap().remove(&device_id) {
        let _ = tx.send(accept);
    }
}
