//! 局域网互传文件。
//!
//! 与剪贴板同步共用同一 HTTP 服务，新增一个端点：
//! - `POST /recv?name=<url编码文件名>&size=<字节数>`：接收文件，body 为文件内容（可加密）
//!
//! 设计要点：
//! - **免配对**：同一局域网设备可直接互发（扫描选择或手动输入 IP），不要求先配对；
//!   黑名单仍然生效（被拉黑设备的请求直接拒绝）
//! - **接收确认**：默认每次收到文件都弹窗手动确认（60 秒无操作自动拒绝）；
//!   开启「自动接收」（transfer_auto_accept）后直接收，不弹窗
//! - 接收文件保存到局域网同步的「文件存储路径」（download_dir）；未设置时回落系统下载目录
//! - 传输全程流式读写（64KB 块），不把文件载入内存；发送方持有对方配对码（已配对）时
//!   用配对码做 XOR 流加密，未配对/手动 IP 时明文发送；接收方按 X-Enc-Nonce 头判断是否解密
//! - 收发双方都会把结果写入 SQLite transfers 表（保留最近 200 条），并向前端发
//!   transfer-progress / transfer-changed / transfer-incoming 事件

use std::fs::File;
use std::io::{Read, Write};

use super::*;

/// 流式传输块大小（必须是 8 的倍数，保证 XOR 密钥流的 8 字节块对齐）
const TRANSFER_CHUNK: usize = 64 * 1024;
/// 传输记录最多保留条数
const HISTORY_KEEP: i64 = 200;
/// 进度事件节流间隔
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// 接收确认等待超时（秒）
const CONFIRM_TIMEOUT_SECS: u64 = 60;
/// 拒绝接收时排空请求体的上限（防止对端虚报超大 Content-Length 导致一直读）
const DRAIN_CAP: u64 = 64 * 1024 * 1024;

// ---------- 进度事件 ----------

#[derive(Serialize, Clone)]
struct TransferProgress {
    direction: String, // "send" | "recv"
    peer: String,
    name: String,
    index: usize, // 当前第几个文件（1 起）
    count: usize,
    sent: u64, // 当前文件已传输字节
    total: u64,
}

/// 节流发送进度事件（完成帧强制发送）
fn emit_progress(app: &AppHandle, last: &mut Instant, p: &TransferProgress, force: bool) {
    if !force && last.elapsed() < PROGRESS_INTERVAL {
        return;
    }
    *last = Instant::now();
    let _ = app.emit("transfer-progress", p.clone());
}

// ---------- 传输记录 ----------

#[derive(Serialize)]
pub struct TransferDto {
    id: i64,
    direction: String,
    peer: String,
    file_name: String,
    path: Option<String>,
    size: i64,
    ok: bool,
    msg: Option<String>,
    created_at: i64,
}

#[allow(clippy::too_many_arguments)]
fn record_transfer(
    state: &AppState,
    direction: &str,
    peer: &str,
    file_name: &str,
    path: Option<&str>,
    size: u64,
    ok: bool,
    msg: Option<&str>,
) {
    let db = state.db.lock().unwrap();
    let _ = db.execute(
        "INSERT INTO transfers(direction, peer, file_name, path, size, ok, msg, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            direction,
            peer,
            file_name,
            path,
            size as i64,
            ok as i64,
            msg,
            now_secs()
        ],
    );
    let _ = db.execute(
        "DELETE FROM transfers WHERE id NOT IN (
            SELECT id FROM transfers ORDER BY id DESC LIMIT ?1
        )",
        params![HISTORY_KEEP],
    );
}

// ---------- 文件名 / 保存路径 ----------

/// 清洗远端传来的文件名：只取文件名部分，去掉 Windows 非法字符与控制字符（防目录穿越）
fn sanitize_file_name(name: &str) -> String {
    let base = name.rsplit(['\\', '/']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') && !c.is_control())
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim().to_string();
    if cleaned.is_empty() {
        format!("recv_{}", now_secs())
    } else {
        cleaned
    }
}

/// 目标文件已存在时自动追加 " (n)" 后缀
fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let dest = dir.join(name);
    if !dest.exists() {
        return dest;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 1..1000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}_{}{ext}", now_secs()))
}

/// 接收文件的保存目录：沿用局域网同步的「文件存储路径」，未设置时用系统下载目录
fn recv_dir(app: &AppHandle) -> Option<PathBuf> {
    let state = app.state::<AppState>();
    let configured = state
        .lan
        .settings
        .lock()
        .unwrap()
        .download_dir
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    configured.or_else(|| app.path().download_dir().ok())
}

// ---------- 接收端：POST /recv ----------

/// 接收确认：互传窗口弹窗等待用户答复（最多 CONFIRM_TIMEOUT_SECS 秒），无人应答返回 None
fn ask_recv_approval(app: &AppHandle, peer: &str, name: &str, size: u64, host: &str) -> Option<bool> {
    // 收到文件请求时自动弹出互传窗口，否则窗口关着时请求只会等到超时
    if let Some(w) = app.get_webview_window("transfer") {
        let _ = w.show();
        let _ = w.set_focus();
    }
    // macOS 整个 App 可能被隐藏（NSApplication.hide），需要先唤回
    #[cfg(target_os = "macos")]
    let _ = app.show();
    let req_id = format!("{:016x}", rand_u64());
    let (tx, rx) = mpsc::channel();
    app.state::<AppState>()
        .lan
        .pending_recvs
        .lock()
        .unwrap()
        .insert(req_id.clone(), tx);
    let _ = app.emit(
        "transfer-incoming",
        json!({
            "request_id": req_id,
            "peer": peer,
            "name": name,
            "size": size,
            "host": host,
        }),
    );
    let r = rx
        .recv_timeout(Duration::from_secs(CONFIRM_TIMEOUT_SECS))
        .ok();
    app.state::<AppState>()
        .lan
        .pending_recvs
        .lock()
        .unwrap()
        .remove(&req_id);
    r
}

/// 拒绝接收后排空请求体（最多 DRAIN_CAP 字节），让发送方能读到我们的错误响应而不是写失败
fn drain_body(stream: &mut TcpStream, already: usize, total: u64) {
    let mut remaining = total.saturating_sub(already as u64).min(DRAIN_CAP);
    let mut tmp = [0u8; 8192];
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    while remaining > 0 {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => remaining = remaining.saturating_sub(n as u64),
            Err(_) => break,
        }
    }
}

pub fn handle_recv(
    app: &AppHandle,
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
    client_ip: &str,
    body_start: Vec<u8>,
) {
    // 黑名单设备直接拒绝（不读 body，对方写失败也无所谓）
    if reject_if_blocked(app, stream, headers) {
        return;
    }

    let raw_name = query.get("name").map(String::as_str).unwrap_or("");
    let name = sanitize_file_name(raw_name);
    let total: u64 = match headers
        .get("content-length")
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(v) => v,
        None => {
            respond_json(stream, 400, r#"{"error":"bad_request"}"#);
            return;
        }
    };
    // 发送方仅在持有本机配对码（已配对）时加密；密钥 = 本机配对码
    let enc_nonce = headers
        .get("x-enc-nonce")
        .and_then(|v| u64::from_str_radix(v, 16).ok());
    let (my_token, auto_accept) = {
        let state = app.state::<AppState>();
        let s = state.lan.settings.lock().unwrap();
        (s.pairing_token.clone(), s.transfer_auto_accept)
    };

    // 对端显示名：请求头设备名 → 配对记录名 → IP
    let peer = {
        let state = app.state::<AppState>();
        let s = state.lan.settings.lock().unwrap();
        headers
            .get("x-device-name")
            .map(|n| url_decode(n))
            .filter(|n| !n.is_empty())
            .or_else(|| {
                headers
                    .get("x-device-id")
                    .and_then(|id| s.paired.get(id))
                    .map(|d| d.name.clone())
            })
            .unwrap_or_else(|| client_ip.to_string())
    };

    // 默认手动确认：弹窗等待用户答复（开启「自动接收」则跳过）
    if !auto_accept {
        let (accepted, err_code) = match ask_recv_approval(app, &peer, &name, total, client_ip) {
            Some(true) => (true, 0),
            Some(false) => (false, 403),
            None => (false, 409),
        };
        if !accepted {
            let body = match err_code {
                403 => r#"{"error":"rejected"}"#.to_string(),
                _ => r#"{"error":"confirm_timeout"}"#.to_string(),
            };
            respond_json(stream, err_code, &body);
            // 排空请求体，让发送方完整发出后读到上面的错误响应
            drain_body(stream, body_start.len(), total);
            // 拒绝/超时不入传输记录（记录只保留接收成功的条目）
            return;
        }
    }

    let Some(dir) = recv_dir(app) else {
        respond_json(stream, 500, r#"{"error":"no_dir"}"#);
        drain_body(stream, body_start.len(), total);
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let body = format!(r#"{{"error":"{e}"}}"#);
        respond_json(stream, 500, &body);
        drain_body(stream, body_start.len(), total);
        return;
    }
    let dest = unique_path(&dir, &name);
    let saved_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| name.clone());

    // 大文件流式接收：放宽读写超时（两次 IO 之间的间隔，不是总时长）
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(120)));

    let progress = |sent: u64, last: &mut Instant, force: bool| {
        emit_progress(
            app,
            last,
            &TransferProgress {
                direction: "recv".into(),
                peer: peer.clone(),
                name: saved_name.clone(),
                index: 1,
                count: 1,
                sent,
                total,
            },
            force,
        );
    };

    let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
    let result = recv_body(
        stream, body_start, total, enc_nonce.as_ref(), &my_token, &dest, &progress, &mut last_emit,
    );
    match result {
        Ok(written) => {
            let state = app.state::<AppState>();
            record_transfer(
                &state,
                "recv",
                &peer,
                &saved_name,
                Some(&dest.to_string_lossy()),
                written,
                true,
                None,
            );
            let _ = app.emit("transfer-changed", ());
            let body = json!({ "result": "ok", "savedAs": saved_name }).to_string();
            respond_json(stream, 200, &body);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&dest);
            // 接收失败不入传输记录（记录只保留接收成功的条目）
            let body = format!(r#"{{"error":"{e}"}}"#);
            respond_json(stream, 500, &body);
        }
    }
}

/// 把请求体流式解密落盘；pending 缓冲保证每次解密都从 8 字节对齐的全局偏移开始
#[allow(clippy::too_many_arguments)]
fn recv_body(
    stream: &mut TcpStream,
    body_start: Vec<u8>,
    total: u64,
    enc_nonce: Option<&u64>,
    token: &str,
    dest: &std::path::Path,
    progress: &dyn Fn(u64, &mut Instant, bool),
    last_emit: &mut Instant,
) -> Result<u64, String> {
    let mut file = File::create(dest).map_err(|e| format!("创建文件失败: {e}"))?;
    let mut pending = body_start;
    let mut written: u64 = 0; // 已写盘字节数
    let mut chunk_base: usize = 0; // 下一个待解密字节的全局 8 字节块序号
    let mut tmp = [0u8; TRANSFER_CHUNK];
    loop {
        let buffered = written + pending.len() as u64;
        if buffered < total {
            let n = stream
                .read(&mut tmp)
                .map_err(|e| format!("连接中断: {e}"))?;
            if n == 0 {
                return Err("连接提前关闭，文件不完整".into());
            }
            pending.extend_from_slice(&tmp[..n]);
        }
        let buffered = written + pending.len() as u64;
        let complete = buffered >= total;
        // 未到末尾时只处理 8 对齐部分，余下不足 8 字节的留给下一轮
        let mut process = if complete {
            pending.len()
        } else {
            pending.len() & !7
        };
        // 写盘总量不超过声明的 Content-Length（对端多发时截断）
        process = process.min((total - written) as usize);
        if process > 0 {
            let mut chunk: Vec<u8> = pending.drain(..process).collect();
            if let Some(nonce) = enc_nonce {
                xor_crypt_at(token, *nonce, chunk_base, &mut chunk);
            }
            file.write_all(&chunk)
                .map_err(|e| format!("写入文件失败: {e}"))?;
            chunk_base += process / 8;
            written += process as u64;
            progress(written, last_emit, complete && written >= total);
        }
        if complete && written >= total {
            break;
        }
    }
    file.flush().map_err(|e| format!("写入文件失败: {e}"))?;
    Ok(written)
}

// ---------- 发送端 ----------

/// 发送目标：主机 + 端口 + 显示名 + 可选配对码（有则加密传输）
struct SendTarget {
    host: String,
    port: u16,
    name: String,
    token: Option<String>,
}

/// 按设备 id 解析目标：在线发现的最新地址优先，配对记录兜底地址与配对码
fn resolve_by_id(app: &AppHandle, device_id: &str) -> Result<SendTarget, String> {
    let state = app.state::<AppState>();
    {
        let s = state.lan.settings.lock().unwrap();
        if s.blocked.contains_key(device_id) || s.blocked_by.contains(device_id) {
            return Err("该设备在黑名单中".to_string());
        }
    }
    let discovered = state.lan.discovered.lock().unwrap();
    let s = state.lan.settings.lock().unwrap();
    let d = discovered
        .get(device_id)
        .map(|d| d.device.clone())
        .or_else(|| s.paired.get(device_id).cloned())
        .ok_or_else(|| "设备不在线".to_string())?;
    let host = d.host.ok_or_else(|| "设备离线".to_string())?;
    let token = s.paired.get(device_id).and_then(|p| p.token.clone());
    Ok(SendTarget {
        host,
        port: d.port,
        name: d.name,
        token,
    })
}

/// 按手动输入的 IP 解析目标：依次探测配置端口与默认端口的 /info
fn resolve_by_ip(app: &AppHandle, ip: &str) -> Result<SendTarget, String> {
    let state = app.state::<AppState>();
    let port = state.lan.settings.lock().unwrap().server_port;
    let ports: Vec<u16> = vec![port, DEFAULT_PORT]
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for p in ports {
        match fetch_info(ip, p, Duration::from_secs(2)) {
            Ok(info) => {
                let device_id = info
                    .get("deviceId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let s = state.lan.settings.lock().unwrap();
                if !device_id.is_empty() {
                    if device_id == s.device_id {
                        return Err("不能发送给本机".to_string());
                    }
                    if s.blocked.contains_key(&device_id) {
                        return Err("该设备在黑名单中".to_string());
                    }
                }
                // 对方恰好是已配对设备时带上配对码（加密传输）
                let token = s.paired.get(&device_id).and_then(|d| d.token.clone());
                let name = info
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(ip)
                    .to_string();
                return Ok(SendTarget {
                    host: ip.to_string(),
                    port: p,
                    name,
                    token,
                });
            }
            Err(LanErr::Blocked(_)) => return Err("你已被对方拉黑，无法发送".to_string()),
            Err(_) => continue,
        }
    }
    Err("未找到设备：请确认 IP 正确且对方应用已启动".to_string())
}

/// 发送方读取响应（响应体很小，简化版读取）
fn read_simple_response(stream: &mut TcpStream) -> Result<HttpResp, LanErr> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
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
    // 读完剩余响应体（直到连接关闭；响应体都很小）
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
        if body.len() > 1024 * 1024 {
            break;
        }
    }
    Ok(HttpResp {
        code,
        body,
        enc_nonce: None,
    })
}

/// 发送单个文件；返回发送的字节数
fn send_one(
    app: &AppHandle,
    target: &SendTarget,
    file_path: &std::path::Path,
    name: &str,
    index: usize,
    count: usize,
) -> Result<u64, LanErr> {
    let mut file = File::open(file_path).map_err(|e| LanErr::Net(format!("打开文件失败: {e}")))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);

    let addr = format!("{}:{}", target.host, target.port)
        .as_str()
        .to_socket_addrs()
        .map_err(|e| LanErr::Net(e.to_string()))?
        .next()
        .ok_or_else(|| LanErr::Net("地址解析失败".into()))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| LanErr::Net(e.to_string()))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(300)));

    let (my_id, my_name) = {
        let state = app.state::<AppState>();
        let s = state.lan.settings.lock().unwrap();
        (s.device_id.clone(), s.device_name.clone())
    };
    // 已配对（持有对方配对码）时加密传输；未配对/手动 IP 明文
    let nonce = target.token.as_deref().map(gen_nonce);
    let mut req = format!(
        "POST /recv?name={}&size={total} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nContent-Type: application/octet-stream\r\nContent-Length: {total}\r\nX-Device-Id: {my_id}\r\nX-Device-Name: {}\r\nX-Device-Model: {}\r\n",
        url_encode(name),
        target.host,
        target.port,
        url_encode(&my_name),
        url_encode(&device_model()),
    );
    if let Some(n) = nonce {
        req.push_str(&format!("X-Enc: 1\r\nX-Enc-Nonce: {n:016x}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| LanErr::Net(e.to_string()))?;

    // 流式发送（需要时加密；块大小是 8 的倍数，密钥流全局对齐）
    let mut sent: u64 = 0;
    let mut chunk_base: usize = 0;
    let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    loop {
        let n = file.read(&mut buf).map_err(|e| LanErr::Net(e.to_string()))?;
        if n == 0 {
            break;
        }
        let chunk = &mut buf[..n];
        if let (Some(t), Some(nn)) = (target.token.as_deref(), nonce) {
            xor_crypt_at(t, nn, chunk_base, chunk);
        }
        stream
            .write_all(chunk)
            .map_err(|e| LanErr::Net(e.to_string()))?;
        chunk_base += n / 8;
        sent += n as u64;
        emit_progress(
            app,
            &mut last_emit,
            &TransferProgress {
                direction: "send".into(),
                peer: target.name.clone(),
                name: name.to_string(),
                index,
                count,
                sent,
                total,
            },
            sent >= total,
        );
    }
    let _ = stream.flush();

    let resp = read_simple_response(&mut stream)?;
    match resp.code {
        200 => Ok(sent),
        // 对方是旧版 lscopy 或安卓端（没有 /recv 端点）
        404 => Err(LanErr::Net("对方应用版本过旧，不支持接收文件".into())),
        409 => Err(LanErr::Net("等待对方确认超时".into())),
        403 => {
            let err = serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from));
            match err.as_deref() {
                Some("rejected") => Err(LanErr::Net("对方拒绝了接收".into())),
                Some("blocked") | Some("unpaired") => {
                    Err(LanErr::Net("对方拒绝接收（可能已被对方拉黑）".into()))
                }
                _ => Err(LanErr::Net("对方拒绝接收".into())),
            }
        }
        c => {
            let msg = serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or_else(|| format!("HTTP {c}"));
            Err(LanErr::Net(format!("发送失败: {msg}")))
        }
    }
}

/// 向目标发送一批文件（逐个推送，单个失败不中断后续），返回汇总消息。
/// 发送不写传输记录：记录只保留「接收成功」的条目（发送结果通过弹窗/进度反馈）
fn send_paths_to(app: &AppHandle, target: &SendTarget, paths: Vec<String>) -> String {
    let count = paths.len();
    let mut ok_count = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let path = PathBuf::from(p);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| p.clone());
        if path.is_dir() {
            failures.push(format!("{name}：暂不支持发送文件夹"));
            continue;
        }
        match send_one(app, target, &path, &name, i + 1, count) {
            Ok(_) => ok_count += 1,
            Err(e) => failures.push(format!("{name}：{e}")),
        }
    }
    if failures.is_empty() {
        format!("已发送 {ok_count} 个文件到「{}」", target.name)
    } else {
        format!("成功 {ok_count}/{count} 个：\n{}", failures.join("\n"))
    }
}

// ---------- 命令 ----------

/// 向扫描到的设备发送文件（免配对；已配对设备自动加密）
#[tauri::command]
pub async fn transfer_send(
    app: AppHandle,
    device_id: String,
    paths: Vec<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if paths.is_empty() {
            return Err("未选择文件".to_string());
        }
        let target = resolve_by_id(&app, &device_id)?;
        Ok(send_paths_to(&app, &target, paths))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 向手动输入的 IP 发送文件（自动探测端口，免配对）
#[tauri::command]
pub async fn transfer_send_ip(
    app: AppHandle,
    ip: String,
    paths: Vec<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if paths.is_empty() {
            return Err("未选择文件".to_string());
        }
        let ip = ip.trim().to_string();
        if ip.is_empty() {
            return Err("请输入对方 IP".to_string());
        }
        let target = resolve_by_ip(&app, &ip)?;
        Ok(send_paths_to(&app, &target, paths))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 接收确认弹窗的答复
#[tauri::command]
pub fn transfer_respond_recv(state: State<AppState>, request_id: String, accept: bool) {
    if let Some(tx) = state
        .lan
        .pending_recvs
        .lock()
        .unwrap()
        .remove(&request_id)
    {
        let _ = tx.send(accept);
    }
}

/// 传输记录（最近 100 条，新的在前；只保留接收成功的条目）
#[tauri::command]
pub fn transfer_history(state: State<AppState>) -> Vec<TransferDto> {
    let db = state.db.lock().unwrap();
    let mut stmt = match db.prepare(
        "SELECT id, direction, peer, file_name, path, size, ok, msg, created_at
         FROM transfers WHERE direction='recv' AND ok=1 ORDER BY id DESC LIMIT 100",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let rows = stmt.query_map([], |r| {
        Ok(TransferDto {
            id: r.get(0)?,
            direction: r.get(1)?,
            peer: r.get(2)?,
            file_name: r.get(3)?,
            path: r.get(4)?,
            size: r.get(5)?,
            ok: r.get::<_, i64>(6)? != 0,
            msg: r.get(7)?,
            created_at: r.get(8)?,
        })
    });
    match rows {
        Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
        Err(_) => vec![],
    }
}

/// 只删除「接收保存」的文件：发送记录里的路径是用户源文件，绝不能碰
fn remove_recv_file(direction: &str, path: Option<&str>) {
    if direction != "recv" {
        return;
    }
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

/// 删除单条传输记录；delete_file 为 true 时连同已接收的文件一起删除
#[tauri::command]
pub fn transfer_delete(state: State<AppState>, id: i64, delete_file: bool) -> Result<(), String> {
    let db = state.db.lock().unwrap();
    if delete_file {
        if let Ok((direction, path)) = db.query_row(
            "SELECT direction, path FROM transfers WHERE id=?1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        ) {
            remove_recv_file(&direction, path.as_deref());
        }
    }
    db.execute("DELETE FROM transfers WHERE id=?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 清空传输记录；delete_file 为 true 时连同所有已接收的文件一起删除
#[tauri::command]
pub fn transfer_clear_history(state: State<AppState>, delete_file: bool) -> Result<(), String> {
    let db = state.db.lock().unwrap();
    if delete_file {
        let rows: Vec<(String, Option<String>)> = db
            .prepare("SELECT direction, path FROM transfers")
            .and_then(|mut s| {
                let mapped = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(mapped.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();
        for (direction, path) in rows {
            remove_recv_file(&direction, path.as_deref());
        }
    }
    db.execute("DELETE FROM transfers", [])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 在系统文件管理器中定位文件（Windows 选中文件；macOS 访达中显示）
#[tauri::command]
pub fn transfer_reveal(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err("文件不存在（可能已被移动或删除）".to_string());
    }
    #[cfg(target_family = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(format!("/select,{path}"))
            .spawn()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err("当前平台不支持".to_string())
}
