use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arboard::Clipboard;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};
use tauri_plugin_opener::OpenerExt;

// ---------- 配置 ----------

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    pub hotkey: String,          // 例如 "Ctrl+`" / "Ctrl+Shift+V"
    pub autostart: bool,         // 开机自启
    pub silent_start: bool,      // 静默启动（不弹主窗口）
    pub db_dir: Option<String>,  // 数据库目录；None = 启动文件所在目录
    pub theme: String,           // "dark" | "light"
    pub font_family: String,
    pub font_size: u32,
    pub exclude_apps: Vec<String>, // 不记录这些程序里的复制（exe 名，小写）
    pub max_items: i64,          // 最多保留条数（不含置顶）
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "Ctrl+`".into(),
            autostart: false,
            silent_start: true,
            db_dir: None,
            theme: "dark".into(),
            font_family: "Segoe UI, Microsoft YaHei, system-ui, sans-serif".into(),
            font_size: 14,
            exclude_apps: vec![],
            max_items: 500,
        }
    }
}

fn load_config(path: &PathBuf) -> AppConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config_file(path: &PathBuf, cfg: &AppConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

// ---------- 数据模型 ----------

#[derive(Serialize, Clone)]
struct Clip {
    id: i64,
    kind: String,              // "text" | "image" | "file"
    preview: String,           // 文字截断预览 / "[图片 WxH]" / "📄 文件名"
    image_b64: Option<String>, // 图片的 PNG base64
    url: Option<String>,       // 内容中的第一个网址
    pinned: bool,
    created_at: i64,           // 秒级时间戳
}

// 提取文本中的第一个 http(s) 网址
fn first_url(text: &str) -> Option<String> {
    let mut best: Option<(usize, usize)> = None; // (起始位置, 协议长度)
    for pat in ["https://", "http://"] {
        if let Some(pos) = text.find(pat) {
            if best.map_or(true, |(b, _)| pos < b) {
                best = Some((pos, pat.len()));
            }
        }
    }
    let (pos, _) = best?;
    let rest = &text[pos..];
    let end = rest
        .find(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '）' | '】')
        })
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ';', '!', '?', '。', '，', '；']);
    if url.len() > 8 {
        Some(url.to_string())
    } else {
        None
    }
}

#[derive(Serialize)]
struct DbInfo {
    path: String,
    file_size: u64,
    total: i64,
    text_count: i64,
    image_count: i64,
    pinned_count: i64,
    max_items: i64,
}

struct AppState {
    db: Mutex<Connection>,
    config: Mutex<AppConfig>,
    config_file: PathBuf,
    // 删除记录时忽略当前剪贴板内容，防止监听线程把刚删掉的内容再加回来
    ignored_hash: Mutex<Option<u64>>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn hash_bytes(data: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

fn effective_db_path(cfg: &AppConfig) -> PathBuf {
    let dir = match &cfg.db_dir {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    dir.join("lscopy.db")
}

fn init_db(path: &PathBuf) -> Result<Connection, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clips (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL,
            content TEXT,
            image BLOB,
            width INTEGER,
            height INTEGER,
            pinned INTEGER NOT NULL DEFAULT 0,
            hash INTEGER,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_clips_time ON clips(created_at DESC);",
    )
    .map_err(|e| e.to_string())?;
    // 旧版本库迁移：补 pinned 列
    if conn.prepare("SELECT pinned FROM clips LIMIT 1").is_err() {
        conn.execute_batch("ALTER TABLE clips ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0")
            .map_err(|e| e.to_string())?;
    }
    // 旧版本库迁移：补 hash 列并回填已有数据（必须在建 hash 索引之前）
    if conn.prepare("SELECT hash FROM clips LIMIT 1").is_err() {
        conn.execute_batch("ALTER TABLE clips ADD COLUMN hash INTEGER")
            .map_err(|e| e.to_string())?;
        backfill_hashes(&conn);
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_clips_hash ON clips(hash);")
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

// 为旧数据回填内容哈希（用于去重）
fn backfill_hashes(conn: &Connection) {
    let rows: Vec<(i64, Option<String>, Option<Vec<u8>>)> = conn
        .prepare("SELECT id, content, image FROM clips WHERE hash IS NULL")
        .and_then(|mut s| {
            let mapped = s.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            Ok(mapped.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    for (id, content, image) in rows {
        let h = match (&content, &image) {
            (Some(t), _) => hash_bytes(t.as_bytes()),
            (None, Some(b)) => hash_bytes(b),
            _ => continue,
        };
        let _ = conn.execute(
            "UPDATE clips SET hash = ?1 WHERE id = ?2",
            params![h as i64, id],
        );
    }
}

// 写入记录：相同内容已存在时仅把时间更新为现在（移到最前），不重复插入
// 返回 true 表示列表需要刷新
fn store_clip(
    db: &Connection,
    kind: &str,
    content: Option<&str>,
    image: Option<&[u8]>,
    width: Option<u32>,
    height: Option<u32>,
    hash: u64,
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
        return true;
    }
    db.execute(
        "INSERT INTO clips(kind, content, image, width, height, hash, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![kind, content, image, width, height, h64, now_secs()],
    )
    .is_ok()
}

// 超出上限时删除最旧的非置顶记录
fn prune(db: &Connection, max_items: i64) {
    if max_items <= 0 {
        return;
    }
    let _ = db.execute(
        "DELETE FROM clips WHERE pinned = 0 AND id NOT IN (
            SELECT id FROM clips WHERE pinned = 0 ORDER BY created_at DESC, id DESC LIMIT ?1
        )",
        params![max_items],
    );
}

// ---------- 剪贴板监听 ----------

fn png_from_arboard(img: &arboard::ImageData) -> Option<(Vec<u8>, u32, u32)> {
    let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some((buf.into_inner(), img.width as u32, img.height as u32))
}

#[cfg(target_family = "windows")]
fn foreground_exe_name() -> Option<String> {
    use std::os::windows::ffi::OsStringExt;
    type Hwnd = *mut std::ffi::c_void;
    extern "system" {
        fn GetForegroundWindow() -> Hwnd;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Hwnd;
        fn QueryFullProcessImageNameW(h: Hwnd, flags: u32, buf: *mut u16, size: *mut u32) -> i32;
        fn CloseHandle(h: Hwnd) -> i32;
    }
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let mut size: u32 = 260;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let path = std::ffi::OsString::from_wide(&buf[..size as usize]);
        let s = path.to_string_lossy().to_string();
        let name = s.rsplit('\\').next().unwrap_or(&s).to_lowercase();
        Some(name)
    }
}

#[cfg(not(target_family = "windows"))]
fn foreground_exe_name() -> Option<String> {
    None
}

// 读取剪贴板中的文件列表（资源管理器里复制的文件/文件夹，CF_HDROP）
#[cfg(target_family = "windows")]
fn clipboard_files() -> Option<Vec<String>> {
    use std::os::windows::ffi::OsStringExt;
    const CF_HDROP: u32 = 15;
    type Handle = *mut std::ffi::c_void;
    extern "system" {
        fn OpenClipboard(hwnd: Handle) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(fmt: u32) -> Handle;
        fn IsClipboardFormatAvailable(fmt: u32) -> i32;
        fn DragQueryFileW(hdrop: Handle, idx: u32, buf: *mut u16, len: u32) -> u32;
        fn GlobalLock(h: Handle) -> Handle;
        fn GlobalUnlock(h: Handle) -> i32;
    }
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP) == 0 {
            return None;
        }
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let mut result = Vec::new();
        let h = GetClipboardData(CF_HDROP);
        if !h.is_null() {
            let hdrop = GlobalLock(h);
            if !hdrop.is_null() {
                let count = DragQueryFileW(hdrop, 0xFFFFFFFF, std::ptr::null_mut(), 0);
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
                    let mut buf = vec![0u16; (len + 1) as usize];
                    let got = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1);
                    if got > 0 {
                        buf.truncate(got as usize);
                        result.push(std::ffi::OsString::from_wide(&buf).to_string_lossy().to_string());
                    }
                }
                GlobalUnlock(h);
            }
        }
        CloseClipboard();
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }
}

#[cfg(not(target_family = "windows"))]
fn clipboard_files() -> Option<Vec<String>> {
    None
}

fn is_excluded(cfg: &AppConfig) -> bool {
    if cfg.exclude_apps.is_empty() {
        return false;
    }
    match foreground_exe_name() {
        Some(name) => cfg.exclude_apps.iter().any(|e| {
            let e = e.trim().to_lowercase();
            !e.is_empty() && (name == e || name == format!("{e}.exe"))
        }),
        None => false,
    }
}

fn start_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut cb = match Clipboard::new() {
            Ok(c) => c,
            Err(_) => return,
        };
        loop {
            std::thread::sleep(Duration::from_millis(600));
            let state = app.state::<AppState>();

            let (excluded, max_items) = {
                let cfg = state.config.lock().unwrap();
                (is_excluded(&cfg), cfg.max_items)
            };
            if excluded {
                continue;
            }

            // 优先文字，其次图片，最后文件（相同内容走 store_clip 去重，只移到最前）
            if let Ok(text) = cb.get_text() {
                let text = text.trim_end_matches('\0').to_string();
                if !text.is_empty() {
                    let h = hash_bytes(text.as_bytes());
                    if should_skip(&state, h) {
                        continue;
                    }
                    let db = state.db.lock().unwrap();
                    if store_clip(&db, "text", Some(&text), None, None, None, h) {
                        prune(&db, max_items);
                        let _ = app.emit("clip-added", ());
                    }
                    continue;
                }
            }
            if let Ok(img) = cb.get_image() {
                if let Some((png, w, hgt)) = png_from_arboard(&img) {
                    if png.len() <= 20 * 1024 * 1024 {
                        let h = hash_bytes(&png);
                        if should_skip(&state, h) {
                            continue;
                        }
                        let db = state.db.lock().unwrap();
                        if store_clip(&db, "image", None, Some(&png), Some(w), Some(hgt), h) {
                            prune(&db, max_items);
                            let _ = app.emit("clip-added", ());
                        }
                    }
                }
                continue;
            }
            // 资源管理器里复制的文件/文件夹（视频等）
            if let Some(files) = clipboard_files() {
                let joined = files.join("\n");
                let h = hash_bytes(joined.as_bytes());
                if should_skip(&state, h) {
                    continue;
                }
                let db = state.db.lock().unwrap();
                if store_clip(&db, "file", Some(&joined), None, None, None, h) {
                    prune(&db, max_items);
                    let _ = app.emit("clip-added", ());
                }
            }
        }
    });
}

// 删除记录时调用：读取当前系统剪贴板内容哈希并加入忽略列表
fn ignore_current_clipboard(state: &AppState) {
    let h = (|| {
        let mut cb = Clipboard::new().ok()?;
        if let Ok(t) = cb.get_text() {
            let t = t.trim_end_matches('\0');
            if !t.is_empty() {
                return Some(hash_bytes(t.as_bytes()));
            }
        }
        if let Ok(img) = cb.get_image() {
            if let Some((png, _, _)) = png_from_arboard(&img) {
                return Some(hash_bytes(&png));
            }
        }
        clipboard_files().map(|f| hash_bytes(f.join("\n").as_bytes()))
    })();
    *state.ignored_hash.lock().unwrap() = h;
}

// 监听线程写入前调用：被忽略的内容（刚删除的）跳过；剪贴板换成新内容后忽略自动失效
fn should_skip(state: &AppState, h: u64) -> bool {
    let mut ig = state.ignored_hash.lock().unwrap();
    match *ig {
        Some(x) if x == h => true,
        Some(_) => {
            *ig = None;
            false
        }
        None => false,
    }
}

// ---------- 命令 ----------

#[tauri::command]
fn list_clips(state: State<AppState>, keyword: Option<String>) -> Vec<Clip> {
    let db = state.db.lock().unwrap();
    let (sql, kw): (&str, Option<String>) = match &keyword {
        Some(k) if !k.trim().is_empty() => (
            "SELECT id, kind, content, image, width, height, pinned, created_at FROM clips
             WHERE content LIKE ?1
             ORDER BY pinned DESC, created_at DESC LIMIT 500",
            Some(format!("%{}%", k.trim())),
        ),
        _ => (
            "SELECT id, kind, content, image, width, height, pinned, created_at FROM clips
             ORDER BY pinned DESC, created_at DESC LIMIT 500",
            None,
        ),
    };
    let mut stmt = match db.prepare(sql) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let map_row = |row: &rusqlite::Row| -> rusqlite::Result<Clip> {
        let kind: String = row.get(1)?;
        let content: Option<String> = row.get(2)?;
        let img: Option<Vec<u8>> = row.get(3)?;
        let w: Option<u32> = row.get(4)?;
        let h: Option<u32> = row.get(5)?;
        let url = content.as_deref().and_then(first_url);
        let (preview, image_b64) = if kind == "image" {
            (
                format!("[图片 {}x{}]", w.unwrap_or(0), h.unwrap_or(0)),
                img.map(|b| B64.encode(b)),
            )
        } else if kind == "file" {
            // content 为换行分隔的文件路径列表
            let paths: Vec<&str> = content
                .as_deref()
                .unwrap_or("")
                .lines()
                .filter(|l| !l.trim().is_empty())
                .collect();
            let first = paths
                .first()
                .and_then(|p| p.rsplit(['\\', '/']).next())
                .unwrap_or("文件");
            let preview = if paths.len() > 1 {
                format!("📄 {} 等 {} 个文件", first, paths.len())
            } else {
                format!("📄 {}", first)
            };
            (preview, None)
        } else {
            let t = content.unwrap_or_default();
            let preview: String = t.chars().take(300).collect();
            (preview, None)
        };
        Ok(Clip {
            id: row.get(0)?,
            kind,
            preview,
            image_b64,
            url,
            pinned: row.get::<_, i64>(6)? != 0,
            created_at: row.get(7)?,
        })
    };
    let rows = match kw {
        Some(k) => stmt.query_map(params![k], map_row),
        None => stmt.query_map([], map_row),
    };
    match rows {
        Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
        Err(_) => vec![],
    }
}

#[tauri::command]
fn toggle_pin(state: State<AppState>, id: i64) -> Result<bool, String> {
    let db = state.db.lock().unwrap();
    db.execute(
        "UPDATE clips SET pinned = CASE WHEN pinned = 0 THEN 1 ELSE 0 END WHERE id = ?1",
        params![id],
    )
    .map_err(|e| e.to_string())?;
    let pinned: i64 = db
        .query_row("SELECT pinned FROM clips WHERE id=?1", params![id], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    Ok(pinned != 0)
}

fn set_clipboard_by_id(state: &AppState, id: i64) -> Result<(), String> {
    let (kind, content, img) = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT kind, content, image FROM clips WHERE id=?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?
    };
    let mut cb = Clipboard::new().map_err(|e| e.to_string())?;
    if kind == "image" {
        let png = img.ok_or("empty image")?;
        let dynimg = image::load_from_memory(&png)
            .map_err(|e| e.to_string())?
            .to_rgba8();
        let (w, h) = dynimg.dimensions();
        cb.set_image(arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: dynimg.into_raw().into(),
        })
        .map_err(|e| e.to_string())?;
    } else {
        let text = content.unwrap_or_default();
        cb.set_text(text.clone()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn copy_clip(state: State<AppState>, id: i64) -> Result<(), String> {
    set_clipboard_by_id(&state, id)
}

#[tauri::command]
fn paste_clip(state: State<AppState>, app: AppHandle, id: i64) -> Result<(), String> {
    set_clipboard_by_id(&state, id)?;
    // 隐藏面板，把焦点还给目标窗口，再模拟 Ctrl+V（mac 为 Cmd+V）
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
    std::thread::sleep(Duration::from_millis(150));
    std::thread::spawn(|| {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};
        if let Ok(mut enigo) = Enigo::new(&Settings::default()) {
            let modifier = if cfg!(target_os = "macos") {
                Key::Meta
            } else {
                Key::Control
            };
            let _ = enigo.key(modifier, Direction::Press);
            let _ = enigo.key(Key::Unicode('v'), Direction::Click);
            let _ = enigo.key(modifier, Direction::Release);
        }
    });
    Ok(())
}

fn range_where(range: &str, now: i64) -> String {
    match range {
        "1h" => format!("created_at >= {}", now - 3600),
        "7d" => format!("created_at >= {}", now - 7 * 86400),
        "30d" => format!("created_at >= {}", now - 30 * 86400),
        "today" => {
            let offset = local_utc_offset(now);
            let midnight = now - ((now + offset) % 86400);
            format!("created_at >= {midnight}")
        }
        _ => "1=1".to_string(), // all
    }
}

#[tauri::command]
fn count_pinned_in_range(state: State<AppState>, range: String) -> i64 {
    let db = state.db.lock().unwrap();
    let cond = range_where(&range, now_secs());
    db.query_row(
        &format!("SELECT COUNT(*) FROM clips WHERE pinned = 1 AND {cond}"),
        [],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

#[tauri::command]
fn delete_range(state: State<AppState>, range: String, include_pinned: bool) -> Result<i64, String> {
    ignore_current_clipboard(&state);
    let db = state.db.lock().unwrap();
    let cond = range_where(&range, now_secs());
    let pinned_cond = if include_pinned { "1=1" } else { "pinned = 0" };
    let affected = db.execute(
        &format!("DELETE FROM clips WHERE {cond} AND {pinned_cond}"),
        [],
    );
    affected.map(|n| n as i64).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_clip(state: State<AppState>, id: i64) -> Result<(), String> {
    ignore_current_clipboard(&state);
    let db = state.db.lock().unwrap();
    db.execute("DELETE FROM clips WHERE id=?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn open_clip_with_system(
    state: State<AppState>,
    app: AppHandle,
    id: i64,
) -> Result<(), String> {
    let (kind, content, img) = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT kind, content, image FROM clips WHERE id=?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?
    };

    // 文字和图片先落盘到临时目录，文件直接用原路径
    let path = match kind.as_str() {
        "image" => {
            let dir = std::env::temp_dir().join("lscopy");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let p = dir.join(format!("clip_{id}.png"));
            std::fs::write(&p, img.ok_or("图片数据为空")?).map_err(|e| e.to_string())?;
            p
        }
        "file" => {
            let first = content
                .unwrap_or_default()
                .lines()
                .find(|l| !l.trim().is_empty())
                .ok_or("文件路径为空")?
                .to_string();
            PathBuf::from(first)
        }
        _ => {
            let dir = std::env::temp_dir().join("lscopy");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let p = dir.join(format!("clip_{id}.txt"));
            std::fs::write(&p, content.unwrap_or_default()).map_err(|e| e.to_string())?;
            p
        }
    };

    if !path.exists() {
        return Err(format!("文件不存在: {}", path.display()));
    }
    app.opener()
        .open_path(path.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_config(state: State<AppState>) -> AppConfig {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
fn save_config(app: AppHandle, state: State<AppState>, config: AppConfig) -> Result<(), String> {
    // 1. 数据库目录变更：打开新库并切换（旧库文件保留不删）
    let old_dir = state.config.lock().unwrap().db_dir.clone();
    if old_dir != config.db_dir {
        let new_path = effective_db_path(&config);
        let conn = init_db(&new_path)?;
        let mut db = state.db.lock().unwrap();
        *db = conn;
    }

    // 2. 重注册全局热键
    register_hotkey(&app, &config.hotkey)?;

    // 3. 应用开机自启
    apply_autostart(&app, config.autostart);

    // 4. 持久化 + 广播
    save_config_file(&state.config_file, &config)?;
    *state.config.lock().unwrap() = config.clone();
    let _ = app.emit("config-changed", config);
    Ok(())
}

#[tauri::command]
fn get_db_info(state: State<AppState>) -> DbInfo {
    let cfg = state.config.lock().unwrap().clone();
    let path = effective_db_path(&cfg);
    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let db = state.db.lock().unwrap();
    let q = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    DbInfo {
        path: path.to_string_lossy().to_string(),
        file_size,
        total: q("SELECT COUNT(*) FROM clips"),
        text_count: q("SELECT COUNT(*) FROM clips WHERE kind='text'"),
        image_count: q("SELECT COUNT(*) FROM clips WHERE kind='image'"),
        pinned_count: q("SELECT COUNT(*) FROM clips WHERE pinned=1"),
        max_items: cfg.max_items,
    }
}

#[derive(Serialize, Deserialize)]
struct ExportClip {
    kind: String,
    content: Option<String>,
    image_b64: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pinned: bool,
    created_at: i64,
}

#[derive(Serialize, Deserialize)]
struct ExportFile {
    version: u32,
    clips: Vec<ExportClip>,
}

#[tauri::command]
fn export_clips(state: State<AppState>, path: String) -> Result<i64, String> {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT kind, content, image, width, height, pinned, created_at FROM clips ORDER BY created_at")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let img: Option<Vec<u8>> = r.get(2)?;
            Ok(ExportClip {
                kind: r.get(0)?,
                content: r.get(1)?,
                image_b64: img.map(|b| B64.encode(b)),
                width: r.get(3)?,
                height: r.get(4)?,
                pinned: r.get::<_, i64>(5)? != 0,
                created_at: r.get(6)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let clips: Vec<ExportClip> = rows.filter_map(|r| r.ok()).collect();
    let count = clips.len() as i64;
    let file = ExportFile { version: 1, clips };
    let json = serde_json::to_string(&file).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(count)
}

#[tauri::command]
fn import_clips(state: State<AppState>, path: String) -> Result<i64, String> {
    let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let file: ExportFile = serde_json::from_str(&json).map_err(|e| format!("文件格式不正确: {e}"))?;
    let max_items = state.config.lock().unwrap().max_items;
    let db = state.db.lock().unwrap();
    let mut count = 0i64;
    for c in &file.clips {
        let img = match &c.image_b64 {
            Some(b64) => B64.decode(b64).ok(),
            None => None,
        };
        let h = match (&c.content, &img) {
            (Some(t), _) => hash_bytes(t.as_bytes()),
            (None, Some(b)) => hash_bytes(b),
            _ => 0,
        };
        // 相同内容已存在则跳过，避免导入产生重复
        let exists = db
            .query_row(
                "SELECT 1 FROM clips WHERE hash = ?1 LIMIT 1",
                params![h as i64],
                |_| Ok(()),
            )
            .is_ok();
        if exists {
            continue;
        }
        let r = db.execute(
            "INSERT INTO clips(kind, content, image, width, height, pinned, hash, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![c.kind, c.content, img, c.width, c.height, c.pinned as i64, h as i64, c.created_at],
        );
        if r.is_ok() {
            count += 1;
        }
    }
    prune(&db, max_items);
    Ok(count)
}

#[tauri::command]
fn list_system_fonts() -> Vec<String> {
    #[cfg(target_family = "windows")]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;
        let mut fonts = std::collections::BTreeSet::new();
        for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            if let Ok(key) = RegKey::predef(root)
                .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts")
            {
                for (name, _) in key.enum_values().filter_map(|r| r.ok()) {
                    let n = name
                        .trim_end_matches(" (TrueType)")
                        .trim_end_matches(" (OpenType)")
                        .trim_end_matches(" (All res)")
                        .trim()
                        .to_string();
                    if !n.is_empty() {
                        fonts.insert(n);
                    }
                }
            }
        }
        return fonts.into_iter().collect();
    }
    #[cfg(not(target_family = "windows"))]
    {
        vec![]
    }
}

#[tauri::command]
fn open_settings(app: AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

// 本地 UTC 偏移（秒），用于"今天"的零点计算
fn local_utc_offset(_now: i64) -> i64 {
    #[cfg(target_family = "windows")]
    unsafe {
        #[repr(C)]
        struct SYSTEMTIME {
            w_year: u16,
            w_month: u16,
            w_dow: u16,
            w_day: u16,
            w_hour: u16,
            w_min: u16,
            w_sec: u16,
            w_ms: u16,
        }
        #[repr(C)]
        struct TIME_ZONE_INFORMATION {
            bias: i32,
            std_name: [u16; 32],
            std_date: SYSTEMTIME,
            std_bias: i32,
            day_name: [u16; 32],
            day_date: SYSTEMTIME,
            day_bias: i32,
        }
        extern "system" {
            fn GetTimeZoneInformation(tzi: *mut TIME_ZONE_INFORMATION) -> u32;
        }
        let mut tzi: TIME_ZONE_INFORMATION = std::mem::zeroed();
        GetTimeZoneInformation(&mut tzi);
        return -(tzi.bias as i64) * 60;
    }
    #[cfg(not(target_family = "windows"))]
    {
        0
    }
}

// ---------- 热键解析 ----------

fn code_from_name(name: &str) -> Option<Code> {
    let n = name.trim();
    let lower = n.to_lowercase();
    match lower.as_str() {
        "`" | "~" | "backquote" => return Some(Code::Backquote),
        "space" => return Some(Code::Space),
        "tab" => return Some(Code::Tab),
        "enter" | "return" => return Some(Code::Enter),
        "esc" | "escape" => return Some(Code::Escape),
        "up" | "arrowup" => return Some(Code::ArrowUp),
        "down" | "arrowdown" => return Some(Code::ArrowDown),
        "left" | "arrowleft" => return Some(Code::ArrowLeft),
        "right" | "arrowright" => return Some(Code::ArrowRight),
        "backspace" => return Some(Code::Backspace),
        "delete" | "del" => return Some(Code::Delete),
        "home" => return Some(Code::Home),
        "end" => return Some(Code::End),
        "pageup" => return Some(Code::PageUp),
        "pagedown" => return Some(Code::PageDown),
        "-" | "minus" => return Some(Code::Minus),
        "=" | "equal" => return Some(Code::Equal),
        "," | "comma" => return Some(Code::Comma),
        "." | "period" => return Some(Code::Period),
        "/" | "slash" => return Some(Code::Slash),
        "\\" | "backslash" => return Some(Code::Backslash),
        ";" | "semicolon" => return Some(Code::Semicolon),
        "'" | "quote" => return Some(Code::Quote),
        "[" | "bracketleft" => return Some(Code::BracketLeft),
        "]" | "bracketright" => return Some(Code::BracketRight),
        _ => {}
    }
    if lower.len() == 1 {
        let c = lower.chars().next().unwrap();
        if ('a'..='z').contains(&c) {
            return Some(match c {
                'a' => Code::KeyA, 'b' => Code::KeyB, 'c' => Code::KeyC, 'd' => Code::KeyD,
                'e' => Code::KeyE, 'f' => Code::KeyF, 'g' => Code::KeyG, 'h' => Code::KeyH,
                'i' => Code::KeyI, 'j' => Code::KeyJ, 'k' => Code::KeyK, 'l' => Code::KeyL,
                'm' => Code::KeyM, 'n' => Code::KeyN, 'o' => Code::KeyO, 'p' => Code::KeyP,
                'q' => Code::KeyQ, 'r' => Code::KeyR, 's' => Code::KeyS, 't' => Code::KeyT,
                'u' => Code::KeyU, 'v' => Code::KeyV, 'w' => Code::KeyW, 'x' => Code::KeyX,
                'y' => Code::KeyY, 'z' => Code::KeyZ,
                _ => unreachable!(),
            });
        }
        if ('0'..='9').contains(&c) {
            return Some(match c {
                '0' => Code::Digit0, '1' => Code::Digit1, '2' => Code::Digit2,
                '3' => Code::Digit3, '4' => Code::Digit4, '5' => Code::Digit5,
                '6' => Code::Digit6, '7' => Code::Digit7, '8' => Code::Digit8,
                '9' => Code::Digit9,
                _ => unreachable!(),
            });
        }
    }
    if lower.len() >= 2 && lower.starts_with('f') {
        if let Ok(n) = lower[1..].parse::<u32>() {
            return Some(match n {
                1 => Code::F1, 2 => Code::F2, 3 => Code::F3, 4 => Code::F4,
                5 => Code::F5, 6 => Code::F6, 7 => Code::F7, 8 => Code::F8,
                9 => Code::F9, 10 => Code::F10, 11 => Code::F11, 12 => Code::F12,
                _ => return None,
            });
        }
    }
    None
}

fn parse_hotkey(s: &str) -> Option<Shortcut> {
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;
    for part in s.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" => mods |= Modifiers::ALT,
            "win" | "super" | "meta" | "cmd" => mods |= Modifiers::SUPER,
            other => code = code_from_name(other),
        }
    }
    code.map(|c| Shortcut::new(Some(mods), c))
}

fn register_hotkey(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let sc = parse_hotkey(hotkey).ok_or_else(|| format!("无法识别的快捷键: {hotkey}"))?;
    gs.register(sc)
        .map_err(|e| format!("注册快捷键失败（可能与其他软件冲突）: {e}"))
}

fn apply_autostart(app: &AppHandle, enable: bool) {
    let mgr = app.autolaunch();
    let enabled = mgr.is_enabled().unwrap_or(false);
    if enable && !enabled {
        let _ = mgr.enable();
    } else if !enable && enabled {
        let _ = mgr.disable();
    }
}

// ---------- 窗口/托盘 ----------

fn toggle_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
            let _ = app.emit("panel-shown", ());
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 单实例：重复启动时提示并聚焦已有窗口
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            app.dialog()
                .message("剪贴板管家已经在运行中，可通过托盘图标或快捷键呼出。")
                .title("提示")
                .blocking_show();
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        toggle_window(app);
                    }
                })
                .build(),
        )
        .setup(|app| {
            // 配置文件固定在系统配置目录（数据库位置可在设置里改）
            let config_file = app
                .path()
                .app_config_dir()
                .map_err(|e| e.to_string())?
                .join("lscopy-config.json");
            let config = load_config(&config_file);
            let db = init_db(&effective_db_path(&config))?;
            app.manage(AppState {
                db: Mutex::new(db),
                config: Mutex::new(config.clone()),
                config_file,
                ignored_hash: Mutex::new(None),
            });

            // 托盘
            let show = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &settings, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .tooltip(&format!("剪贴板管家 ({})", config.hotkey))
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => toggle_window(app),
                    "settings" => {
                        if let Some(w) = app.get_webview_window("settings") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 只响应左键松开；右键留给系统弹菜单，避免窗口焦点变化把菜单闪掉
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // 全局热键（默认 Ctrl+`）
            register_hotkey(app.handle(), &config.hotkey)?;

            // 开机自启状态同步
            apply_autostart(app.handle(), config.autostart);

            // 主窗口：关闭改隐藏；失焦自动关闭（点其他位置即关闭）
            if let Some(win) = app.get_webview_window("main") {
                let w = win.clone();
                win.on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                    WindowEvent::Focused(false) => {
                        let _ = w.hide();
                    }
                    _ => {}
                });
                // 非静默启动时显示主窗口
                if !config.silent_start {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }

            // 设置窗口：关闭改隐藏，便于再次快速打开
            if let Some(win) = app.get_webview_window("settings") {
                let w = win.clone();
                win.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            // 启动剪贴板监听线程
            start_watcher(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_clips,
            toggle_pin,
            copy_clip,
            paste_clip,
            count_pinned_in_range,
            delete_range,
            delete_clip,
            get_config,
            save_config,
            get_db_info,
            export_clips,
            import_clips,
            open_settings,
            list_system_fonts,
            open_clip_with_system
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
