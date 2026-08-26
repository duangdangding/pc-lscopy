use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arboard::Clipboard;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
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
    pub max_items: i64,          // 最多保留条数（不含置顶），0 = 无限制
    pub retention_value: u32,    // 数据保留时长数值，0 = 永久保留
    pub retention_unit: String,  // "hours" | "days" | "months" | "years"
    pub enabled: bool,           // 是否开启剪贴板记录
    pub remember_size: bool,     // 记住窗口大小（重启后恢复上次长宽）
    pub window_width: u32,       // 记住的窗口宽度（物理像素）
    pub window_height: u32,      // 记住的窗口高度（物理像素）
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
            max_items: 0,
            retention_value: 0,
            retention_unit: "days".into(),
            enabled: true,
            remember_size: false,
            window_width: 420,
            window_height: 640,
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

// 视频文件扩展名（kind=file 时按第一个文件路径的扩展名归类）
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "m2ts",
    "rmvb", "rm", "3gp", "f4v", "vob",
];

// 办公/文本类文件扩展名
const OFFICE_EXTS: &[&str] = &[
    "txt", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "pdf", "md", "rtf", "wps",
    "et", "dps", "odt", "ods", "odp",
];

// 记录分类："text" 纯文本 | "image" 图片 | "video" 视频文件 | "office" 办公/文本文件 | "file" 其他文件
// 文件类记录按第一个文件路径的扩展名归类（与预览显示的第一个文件一致）
fn category_of(kind: &str, content: Option<&str>) -> &'static str {
    match kind {
        "text" => "text",
        "image" => "image",
        "file" => {
            let first = content
                .unwrap_or("")
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            let ext = first
                .rsplit(['\\', '/'])
                .next()
                .and_then(|name| name.rsplit_once('.'))
                .map(|(_, e)| e.to_lowercase())
                .unwrap_or_default();
            if VIDEO_EXTS.contains(&ext.as_str()) {
                "video"
            } else if OFFICE_EXTS.contains(&ext.as_str()) {
                "office"
            } else {
                "file"
            }
        }
        _ => "file",
    }
}

#[derive(Serialize, Clone)]
struct Clip {
    id: i64,
    kind: String,              // "text" | "image" | "file"
    category: String,          // "text" | "image" | "video" | "office" | "file"
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
    // 已删除/被排除内容的哈希集合：命中即跳过，直到有新内容入库后清空
    ignored_hashes: Mutex<Vec<u64>>,
    // 监听线程已处理过的剪贴板内容哈希，避免轮询重复处理同一内容
    last_seen: Mutex<u64>,
    // 托盘「开启记录」勾选项，用于跨界面同步勾选状态
    tray_toggle: Mutex<Option<tauri::menu::CheckMenuItem<tauri::Wry>>>,
    // 粘贴防抖：连点时合并为一次粘贴
    paste_pending: Mutex<bool>,
    paste_running: AtomicBool,
    // 面板弹出前的前台窗口，粘贴后把焦点还给它
    prev_hwnd: Mutex<isize>,
    // 面板钉住状态：钉住后失焦/粘贴都不自动隐藏（会话内有效，不持久化）
    panel_pinned: AtomicBool,
    // 拖动/缩放进行中：系统模态拖动会造成瞬时失焦，此时不自动隐藏
    dragging: AtomicBool,
    // 主窗口当前是否有焦点（失焦延迟复查用）
    main_focused: AtomicBool,
    // 调整后待落盘的窗口尺寸（Resize 事件频繁，由监听线程统一保存）
    pending_size: Mutex<Option<(u32, u32)>>,
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

// 软件所在目录（配置文件/数据库默认都放这里，便携模式）
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn effective_db_path(cfg: &AppConfig) -> PathBuf {
    let dir = match &cfg.db_dir {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => exe_dir(),
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

// 数据保留时长：计算截止时间戳，0 = 永久保留（None）
fn retention_cutoff(cfg: &AppConfig) -> Option<i64> {
    if cfg.retention_value == 0 {
        return None;
    }
    let v = cfg.retention_value as i64;
    let secs = match cfg.retention_unit.as_str() {
        "hours" => v * 3600,
        "months" => v * 30 * 86400,
        "years" => v * 365 * 86400,
        _ => v * 86400, // days
    };
    Some(now_secs() - secs)
}

// 删除超过保留时长的非置顶记录
fn apply_retention(db: &Connection, cfg: &AppConfig) {
    if let Some(cutoff) = retention_cutoff(cfg) {
        let _ = db.execute(
            "DELETE FROM clips WHERE pinned = 0 AND created_at < ?1",
            params![cutoff],
        );
    }
}

// ---------- 剪贴板监听 ----------

fn png_from_arboard(img: &arboard::ImageData) -> Option<(Vec<u8>, u32, u32)> {
    let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.to_vec())?;
    encode_png(rgba)
}

fn encode_png(rgba: image::RgbaImage) -> Option<(Vec<u8>, u32, u32)> {
    let w = rgba.width();
    let h = rgba.height();
    let mut buf = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some((buf.into_inner(), w, h))
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

// ---------- Windows 原生图片兜底 ----------
// 有些截图软件只写 CF_BITMAP / CF_DIB，arboard 读不到，这里直接走 Win32 取图

// 按掩码提取颜色分量并扩展到 8 位
#[cfg(target_family = "windows")]
fn extract_masked(v: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let raw = (v & mask) >> mask.trailing_zeros();
    let max = (1u32 << mask.count_ones()) - 1;
    ((raw * 255 + max / 2) / max) as u8
}

// 把 DIB 数据解析成 RGBA 像素（支持 16/24/32 位、BI_RGB 与 BI_BITFIELDS）
#[cfg(target_family = "windows")]
fn rgba_from_dib(dib: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if dib.len() < 40 {
        return None;
    }
    let u32_at = |off: usize| -> u32 { u32::from_le_bytes([dib[off], dib[off + 1], dib[off + 2], dib[off + 3]]) };
    let header_size = u32_at(0) as usize;
    // 只支持 BITMAPINFOHEADER(40) 及以上，古老的 BITMAPCOREHEADER(12) 不处理
    if header_size < 40 || dib.len() < header_size {
        return None;
    }
    let width = u32_at(4) as i32;
    let raw_height = u32_at(8) as i32;
    let bit_count = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32_at(16);
    let clr_used = u32_at(32) as usize;
    if width <= 0 || raw_height == 0 {
        return None;
    }
    let w = width as usize;
    let h = raw_height.unsigned_abs() as usize;
    let top_down = raw_height < 0;

    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;

    // 像素数据偏移 = 头 + （40 字节头的位掩码）+ 色表
    let mut off = header_size;
    let (mut r_mask, mut g_mask, mut b_mask) = (0u32, 0u32, 0u32);
    if compression == BI_BITFIELDS {
        if header_size >= 52 {
            // BITMAPV4/V5 头：掩码在头内
            r_mask = u32_at(40);
            g_mask = u32_at(44);
            b_mask = u32_at(48);
        } else {
            if dib.len() < off + 12 {
                return None;
            }
            r_mask = u32_at(off);
            g_mask = u32_at(off + 4);
            b_mask = u32_at(off + 8);
            off += 12;
        }
    } else if compression != BI_RGB {
        return None; // 压缩格式（RLE/JPEG 等）不支持
    }
    if bit_count <= 8 {
        let n = if clr_used > 0 { clr_used } else { 1usize << bit_count };
        off += n * 4;
    }
    let bytes_pp = (bit_count / 8) as usize;
    if bytes_pp < 2 || dib.len() < off {
        return None;
    }
    let pixels = &dib[off..];
    let stride = (w * bytes_pp + 3) & !3; // 行对齐到 4 字节

    let mut rgba = vec![0u8; w * h * 4];
    let mut any_alpha = false;
    for y in 0..h {
        let src_y = if top_down { y } else { h - 1 - y }; // bottom-up 翻正
        let start = src_y * stride;
        let end = start + w * bytes_pp;
        if end > pixels.len() {
            return None;
        }
        let row = &pixels[start..end];
        for x in 0..w {
            let p = &row[x * bytes_pp..];
            let (r, g, b, a) = match bit_count {
                32 => {
                    if compression == BI_BITFIELDS {
                        let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                        (
                            extract_masked(v, r_mask),
                            extract_masked(v, g_mask),
                            extract_masked(v, b_mask),
                            255,
                        )
                    } else {
                        (p[2], p[1], p[0], p[3])
                    }
                }
                24 => (p[2], p[1], p[0], 255),
                16 => {
                    let v = u16::from_le_bytes([p[0], p[1]]) as u32;
                    // BI_RGB 的 16 位是 555，BI_BITFIELDS 按掩码（常见 565）
                    let (rm, gm, bm) = if compression == BI_BITFIELDS {
                        (r_mask, g_mask, b_mask)
                    } else {
                        (0x7C00, 0x03E0, 0x001F)
                    };
                    (
                        extract_masked(v, rm),
                        extract_masked(v, gm),
                        extract_masked(v, bm),
                        255,
                    )
                }
                _ => return None,
            };
            if a != 0 {
                any_alpha = true;
            }
            let o = (y * w + x) * 4;
            rgba[o] = r;
            rgba[o + 1] = g;
            rgba[o + 2] = b;
            rgba[o + 3] = a;
        }
    }
    // 32 位 BI_RGB 的 alpha 通道经常是未定义的全 0，统一设为不透明
    if !any_alpha {
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    Some((rgba, w as u32, h as u32))
}

// CF_BITMAP 是位图句柄而非内存块：用 GetDIBits 转成 32 位 DIB
#[cfg(target_family = "windows")]
fn dib_from_bitmap(hbmp: *mut std::ffi::c_void) -> Option<Vec<u8>> {
    #[repr(C)]
    struct Bmp {
        bm_type: i32,
        bm_width: i32,
        bm_height: i32,
        bm_width_bytes: i32,
        bm_planes: u16,
        bm_bits_pixel: u16,
        bm_bits: *mut std::ffi::c_void,
    }
    type Handle = *mut std::ffi::c_void;
    extern "system" {
        fn GetObjectW(h: Handle, n: i32, v: *mut std::ffi::c_void) -> i32;
        fn GetDC(hwnd: Handle) -> Handle;
        fn ReleaseDC(hwnd: Handle, hdc: Handle) -> i32;
        fn GetDIBits(hdc: Handle, hbmp: Handle, start: u32, lines: u32, bits: *mut u8, bmi: *mut u8, usage: u32) -> i32;
    }
    unsafe {
        let mut b: Bmp = std::mem::zeroed();
        if GetObjectW(hbmp, std::mem::size_of::<Bmp>() as i32, &mut b as *mut Bmp as *mut _) == 0 {
            return None;
        }
        if b.bm_width <= 0 || b.bm_height == 0 {
            return None;
        }
        let w = b.bm_width as usize;
        let h = b.bm_height.unsigned_abs() as usize;
        // BITMAPINFOHEADER(40 字节) + 32 位像素数据
        let mut dib = vec![0u8; 40 + w * h * 4];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&b.bm_width.to_le_bytes());
        dib[8..12].copy_from_slice(&b.bm_height.to_le_bytes()); // 保留原方向
        dib[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
        dib[14..16].copy_from_slice(&32u16.to_le_bytes()); // bpp
        // compression 保持 BI_RGB(0)
        let hdc = GetDC(std::ptr::null_mut());
        if hdc.is_null() {
            return None;
        }
        let got = GetDIBits(hdc, hbmp, 0, h as u32, dib[40..].as_mut_ptr(), dib.as_mut_ptr(), 0);
        ReleaseDC(std::ptr::null_mut(), hdc);
        if got == 0 {
            return None;
        }
        Some(dib)
    }
}

// arboard 读不到图片时的兜底：按 "PNG" → CF_DIBV5 → CF_DIB → CF_BITMAP 顺序取图
#[cfg(target_family = "windows")]
fn clipboard_image_native() -> Option<(Vec<u8>, u32, u32)> {
    const CF_BITMAP: u32 = 2;
    const CF_DIB: u32 = 8;
    const CF_DIBV5: u32 = 17;
    type Handle = *mut std::ffi::c_void;
    extern "system" {
        fn OpenClipboard(hwnd: Handle) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(fmt: u32) -> Handle;
        fn IsClipboardFormatAvailable(fmt: u32) -> i32;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
        fn GlobalLock(h: Handle) -> Handle;
        fn GlobalUnlock(h: Handle) -> i32;
        fn GlobalSize(h: Handle) -> usize;
    }
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let mut out: Option<(Vec<u8>, u32, u32)> = None;

        // 1. "PNG" 自定义格式（QQ、浏览器等），数据本身就是 PNG 文件
        let png_name: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
        let png_fmt = RegisterClipboardFormatW(png_name.as_ptr());
        if png_fmt != 0 && IsClipboardFormatAvailable(png_fmt) != 0 {
            let hnd = GetClipboardData(png_fmt);
            if !hnd.is_null() {
                let size = GlobalSize(hnd);
                let p = GlobalLock(hnd);
                if !p.is_null() && size > 8 {
                    let bytes = std::slice::from_raw_parts(p as *const u8, size).to_vec();
                    GlobalUnlock(hnd);
                    if let Ok(img) = image::load_from_memory(&bytes) {
                        out = Some((bytes, img.width(), img.height()));
                    }
                } else if !p.is_null() {
                    GlobalUnlock(hnd);
                }
            }
        }

        // 2. DIB / DIBV5
        if out.is_none() {
            for fmt in [CF_DIBV5, CF_DIB] {
                if IsClipboardFormatAvailable(fmt) == 0 {
                    continue;
                }
                let hnd = GetClipboardData(fmt);
                if hnd.is_null() {
                    continue;
                }
                let size = GlobalSize(hnd);
                let p = GlobalLock(hnd);
                if p.is_null() || size < 40 {
                    if !p.is_null() {
                        GlobalUnlock(hnd);
                    }
                    continue;
                }
                let bytes = std::slice::from_raw_parts(p as *const u8, size).to_vec();
                GlobalUnlock(hnd);
                if let Some((rgba, w, h)) = rgba_from_dib(&bytes) {
                    if let Some(img) = image::RgbaImage::from_raw(w, h, rgba) {
                        out = encode_png(img);
                    }
                }
                if out.is_some() {
                    break;
                }
            }
        }

        // 3. CF_BITMAP（只写位图句柄的截图软件，如部分系统/第三方截图工具）
        if out.is_none() && IsClipboardFormatAvailable(CF_BITMAP) != 0 {
            let hbmp = GetClipboardData(CF_BITMAP); // GDI 句柄，不能 GlobalLock
            if !hbmp.is_null() {
                if let Some(dib) = dib_from_bitmap(hbmp) {
                    if let Some((rgba, w, h)) = rgba_from_dib(&dib) {
                        if let Some(img) = image::RgbaImage::from_raw(w, h, rgba) {
                            out = encode_png(img);
                        }
                    }
                }
            }
        }

        CloseClipboard();
        out
    }
}

#[cfg(not(target_family = "windows"))]
fn clipboard_image_native() -> Option<(Vec<u8>, u32, u32)> {
    None
}

// 读取剪贴板图片：先走 arboard，读不到再用 Windows 原生兜底
fn read_image(cb: &mut Clipboard) -> Option<(Vec<u8>, u32, u32)> {
    if let Ok(img) = cb.get_image() {
        if let Some(r) = png_from_arboard(&img) {
            return Some(r);
        }
    }
    clipboard_image_native()
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

// 剪贴板候选内容
enum Cand {
    Text(String, u64),
    Image(Vec<u8>, u32, u32, u64),
    File(String, u64),
}

impl Cand {
    fn hash(&self) -> u64 {
        match self {
            Cand::Text(_, h) | Cand::Image(_, _, _, h) | Cand::File(_, h) => *h,
        }
    }
}

fn read_clipboard(cb: &mut Clipboard) -> Option<Cand> {
    if let Ok(text) = cb.get_text() {
        let text = text.trim_end_matches('\0').to_string();
        if !text.is_empty() {
            let h = hash_bytes(text.as_bytes());
            return Some(Cand::Text(text, h));
        }
    }
    if let Some((png, w, hgt)) = read_image(cb) {
        if png.len() <= 20 * 1024 * 1024 {
            let h = hash_bytes(&png);
            return Some(Cand::Image(png, w, hgt, h));
        }
        // 图片超过 20MB 上限，放弃本轮（不再尝试文件，避免把大图路径当文件记录）
        return None;
    }
    clipboard_files().map(|files| {
        let joined = files.join("\n");
        let h = hash_bytes(joined.as_bytes());
        Cand::File(joined, h)
    })
}

// 把哈希加入忽略集合（去重、上限 64 条）
fn push_ignored(state: &AppState, h: u64) {
    let mut ig = state.ignored_hashes.lock().unwrap();
    if !ig.contains(&h) {
        if ig.len() >= 64 {
            ig.remove(0);
        }
        ig.push(h);
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

            // 窗口尺寸落盘：Resize 事件只暂存，这里统一保存（天然去抖）
            {
                let mut pending = state.pending_size.lock().unwrap();
                if let Some((w, h)) = pending.take() {
                    let mut cfg = state.config.lock().unwrap();
                    if cfg.remember_size {
                        cfg.window_width = w;
                        cfg.window_height = h;
                        let _ = save_config_file(&state.config_file, &cfg);
                    }
                }
            }

            let (enabled, excluded, max_items) = {
                let cfg = state.config.lock().unwrap();
                // 每轮顺便执行保留时长清理（删除过期的非置顶记录）
                let db = state.db.lock().unwrap();
                apply_retention(&db, &cfg);
                (cfg.enabled, is_excluded(&cfg), cfg.max_items)
            };

            let Some(cand) = read_clipboard(&mut cb) else {
                continue;
            };
            let h = cand.hash();

            // 剪贴板内容没变化，跳过
            {
                let mut last = state.last_seen.lock().unwrap();
                if *last == h {
                    continue;
                }
                *last = h;
            }

            // 记录开关关闭：只标记已见，不存储（重新开启时之前的内容不会补录）
            if !enabled {
                continue;
            }

            // 排除的应用：在该应用中复制的内容加入忽略集合，离开/移除排除后也不入库
            if excluded {
                push_ignored(&state, h);
                continue;
            }

            // 被忽略的内容（刚删除的 / 排除应用里复制的）跳过
            {
                let mut ig = state.ignored_hashes.lock().unwrap();
                if ig.contains(&h) {
                    continue;
                }
                // 有新内容正常入库，旧的忽略记录不再需要
                ig.clear();
            }

            let db = state.db.lock().unwrap();
            let changed = match &cand {
                Cand::Text(text, h) => store_clip(&db, "text", Some(text), None, None, None, *h),
                Cand::Image(png, w, hgt, h) => {
                    store_clip(&db, "image", None, Some(png), Some(*w), Some(*hgt), *h)
                }
                Cand::File(joined, h) => store_clip(&db, "file", Some(joined), None, None, None, *h),
            };
            if changed {
                prune(&db, max_items);
                let _ = app.emit("clip-added", ());
            }
        }
    });
}

// 删除记录 / 重新开启记录时调用：读取当前系统剪贴板内容哈希并加入忽略集合
fn ignore_current_clipboard(state: &AppState) {
    let h = (|| {
        let mut cb = Clipboard::new().ok()?;
        read_clipboard(&mut cb).map(|c| c.hash())
    })();
    if let Some(h) = h {
        push_ignored(state, h);
        // 同时标记为已见，防止轮询把当前内容当新内容处理
        *state.last_seen.lock().unwrap() = h;
    }
}

// ---------- 命令 ----------

#[tauri::command]
fn list_clips(state: State<AppState>, keyword: Option<String>) -> Vec<Clip> {
    let db = state.db.lock().unwrap();
    // 列表查询不读取 image blob，图片由前端懒加载（get_clip_image），避免大数据量卡顿
    let (sql, kw): (&str, Option<String>) = match &keyword {
        Some(k) if !k.trim().is_empty() => (
            "SELECT id, kind, content, width, height, pinned, created_at FROM clips
             WHERE content LIKE ?1
             ORDER BY pinned DESC, created_at DESC LIMIT 500",
            Some(format!("%{}%", k.trim())),
        ),
        _ => (
            "SELECT id, kind, content, width, height, pinned, created_at FROM clips
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
        let w: Option<u32> = row.get(3)?;
        let h: Option<u32> = row.get(4)?;
        let url = content.as_deref().and_then(first_url);
        let category = category_of(&kind, content.as_deref()).to_string();
        let preview = if kind == "image" {
            format!("[图片 {}x{}]", w.unwrap_or(0), h.unwrap_or(0))
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
            if paths.len() > 1 {
                format!("📄 {} 等 {} 个文件", first, paths.len())
            } else {
                format!("📄 {}", first)
            }
        } else {
            let t = content.unwrap_or_default();
            t.chars().take(300).collect()
        };
        Ok(Clip {
            id: row.get(0)?,
            category,
            kind,
            preview,
            image_b64: None, // 列表不携带图片数据
            url,
            pinned: row.get::<_, i64>(5)? != 0,
            created_at: row.get(6)?,
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

// 前端懒加载图片：滚动到可视区域时才取回 base64
#[tauri::command]
fn get_clip_image(state: State<AppState>, id: i64) -> Option<String> {
    let db = state.db.lock().unwrap();
    let img: Option<Vec<u8>> = db
        .query_row("SELECT image FROM clips WHERE id=?1", params![id], |r| r.get(0))
        .ok()?;
    img.map(|b| B64.encode(b))
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

// 前台窗口句柄读取/归还（Windows）
#[cfg(target_family = "windows")]
fn foreground_hwnd() -> isize {
    extern "system" {
        fn GetForegroundWindow() -> *mut std::ffi::c_void;
    }
    unsafe { GetForegroundWindow() as isize }
}

#[cfg(target_family = "windows")]
fn focus_hwnd(hwnd: isize) {
    extern "system" {
        fn SetForegroundWindow(h: *mut std::ffi::c_void) -> i32;
    }
    unsafe {
        SetForegroundWindow(hwnd as *mut std::ffi::c_void);
    }
}

#[cfg(not(target_family = "windows"))]
fn foreground_hwnd() -> isize {
    0
}

#[cfg(not(target_family = "windows"))]
fn focus_hwnd(_hwnd: isize) {}

fn simulate_paste() {
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
}

#[tauri::command]
fn set_panel_pinned(state: State<AppState>, pinned: bool) {
    state.panel_pinned.store(pinned, Ordering::SeqCst);
}

#[tauri::command]
fn get_panel_pinned(state: State<AppState>) -> bool {
    state.panel_pinned.load(Ordering::SeqCst)
}

// 开始拖动面板：置 dragging 标记，拖动造成的瞬时失焦不触发自动隐藏
#[tauri::command]
fn start_drag(app: AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    app.state::<AppState>().dragging.store(true, Ordering::SeqCst);
    let _ = win.start_dragging();
    // 拖动是系统模态循环，前端收不到 mouseup，后端轮询左键松开后清除标记
    std::thread::spawn(move || {
        wait_left_button_released();
        app.state::<AppState>().dragging.store(false, Ordering::SeqCst);
    });
}

// 等待鼠标左键松开。若 150ms 内左键已不是按下状态，视为单击（未发生拖动），直接返回
#[cfg(target_family = "windows")]
fn wait_left_button_released() {
    extern "system" {
        fn GetAsyncKeyState(vk: i32) -> i16;
    }
    const VK_LBUTTON: i32 = 0x01;
    let down = || unsafe { (GetAsyncKeyState(VK_LBUTTON) as u16) & 0x8000 != 0 };
    let mut waited = 0;
    while !down() && waited < 150 {
        std::thread::sleep(Duration::from_millis(10));
        waited += 10;
    }
    while down() {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(target_family = "windows"))]
fn wait_left_button_released() {
    std::thread::sleep(Duration::from_secs(2));
}

#[tauri::command]
fn paste_clip(state: State<AppState>, app: AppHandle, id: i64) -> Result<(), String> {
    // 先立刻隐藏面板：点击的第一感知是面板消失，写剪贴板/模拟按键在后台完成
    // 钉住时不隐藏，方便连续粘贴多条
    if !state.panel_pinned.load(Ordering::SeqCst) {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.hide();
        }
    }
    // 剪贴板内容立即更新
    set_clipboard_by_id(&state, id)?;
    *state.paste_pending.lock().unwrap() = true;

    // 已有粘贴 worker 在跑：标记排队即可，它完成后会立刻补下一次
    if state.paste_running.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let app2 = app.clone();
    std::thread::spawn(move || paste_worker(app2));
    Ok(())
}

// 粘贴工作线程主体（独立函数便于重入）
fn paste_worker(app: AppHandle) {
    let state = app.state::<AppState>();
    loop {
        {
            let mut p = state.paste_pending.lock().unwrap();
            if !*p {
                break;
            }
            *p = false;
        }
        // 面板已在 paste_clip 里隐藏，这里只需把焦点还给之前的窗口
        let hwnd = *state.prev_hwnd.lock().unwrap();
        if hwnd != 0 {
            focus_hwnd(hwnd);
        }
        // 等焦点 settling 即可，50ms 在响应速度和可靠性之间比较平衡
        std::thread::sleep(Duration::from_millis(50));
        simulate_paste();
    }
    state.paste_running.store(false, Ordering::SeqCst);
    if *state.paste_pending.lock().unwrap() && !state.paste_running.swap(true, Ordering::SeqCst) {
        let app2 = app.clone();
        std::thread::spawn(move || paste_worker(app2));
    }
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
fn delete_range(
    state: State<AppState>,
    app: AppHandle,
    range: String,
    include_pinned: bool,
) -> Result<i64, String> {
    ignore_current_clipboard(&state);
    let db = state.db.lock().unwrap();
    let cond = range_where(&range, now_secs());
    let pinned_cond = if include_pinned { "1=1" } else { "pinned = 0" };
    let affected = db.execute(
        &format!("DELETE FROM clips WHERE {cond} AND {pinned_cond}"),
        [],
    );
    drop(db);
    let _ = app.emit("clip-added", ());
    affected.map(|n| n as i64).map_err(|e| e.to_string())
}

// 自定义时间区间删除
#[tauri::command]
fn count_pinned_between(state: State<AppState>, start: i64, end: i64) -> i64 {
    let db = state.db.lock().unwrap();
    db.query_row(
        "SELECT COUNT(*) FROM clips WHERE pinned = 1 AND created_at BETWEEN ?1 AND ?2",
        params![start, end],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

#[tauri::command]
fn delete_between(
    state: State<AppState>,
    app: AppHandle,
    start: i64,
    end: i64,
    include_pinned: bool,
) -> Result<i64, String> {
    ignore_current_clipboard(&state);
    let db = state.db.lock().unwrap();
    let pinned_cond = if include_pinned { "1=1" } else { "pinned = 0" };
    let affected = db.execute(
        &format!("DELETE FROM clips WHERE created_at BETWEEN ?1 AND ?2 AND {pinned_cond}"),
        params![start, end],
    );
    drop(db);
    let _ = app.emit("clip-added", ());
    affected.map(|n| n as i64).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_clip(state: State<AppState>, app: AppHandle, id: i64) -> Result<(), String> {
    ignore_current_clipboard(&state);
    let db = state.db.lock().unwrap();
    db.execute("DELETE FROM clips WHERE id=?1", params![id])
        .map_err(|e| e.to_string())?;
    drop(db);
    let _ = app.emit("clip-added", ());
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
    // 0. 开启「记住窗口大小」时，立即把当前面板实际尺寸写入配置
    let mut config = config;
    if config.remember_size {
        if let Some(win) = app.get_webview_window("main") {
            if let Ok(size) = win.inner_size() {
                config.window_width = size.width;
                config.window_height = size.height;
            }
        }
    }

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

    // 4. 持久化 + 同步托盘开关 + 广播
    save_config_file(&state.config_file, &config)?;
    if config.enabled {
        // 从关闭切到开启时，忽略当前剪贴板内容（关闭期间的不补录）
        ignore_current_clipboard(&state);
    }
    if let Some(item) = state.tray_toggle.lock().unwrap().as_ref() {
        let _ = item.set_checked(config.enabled);
    }
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

// 开关「剪贴板记录」的统一入口：托盘菜单 / 弹窗页 / 设置页共用
fn set_recording_enabled(app: &AppHandle, enabled: bool) {
    let state = app.state::<AppState>();
    let cfg = {
        let mut cfg = state.config.lock().unwrap();
        cfg.enabled = enabled;
        let _ = save_config_file(&state.config_file, &cfg);
        cfg.clone()
    };
    if enabled {
        // 关闭期间复制的内容不入库：开启瞬间忽略当前剪贴板内容
        ignore_current_clipboard(&state);
    }
    if let Some(item) = state.tray_toggle.lock().unwrap().as_ref() {
        let _ = item.set_checked(enabled);
    }
    let _ = app.emit("config-changed", cfg);
}

#[tauri::command]
fn set_enabled(app: AppHandle, enabled: bool) {
    set_recording_enabled(&app, enabled);
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
            // 记住弹出前的前台窗口，粘贴后把焦点还给它
            *app.state::<AppState>().prev_hwnd.lock().unwrap() = foreground_hwnd();
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
            // 配置文件放在软件同目录（便携模式，和数据库默认位置一致）
            // 旧版本配置在系统配置目录：首次启动自动迁移过来
            let config_file = exe_dir().join("lscopy-config.json");
            if !config_file.exists() {
                let legacy = app
                    .path()
                    .app_config_dir()
                    .map_err(|e| e.to_string())?
                    .join("lscopy-config.json");
                if legacy.exists() {
                    let _ = std::fs::copy(&legacy, &config_file);
                }
            }
            let config = load_config(&config_file);
            let db = init_db(&effective_db_path(&config))?;
            app.manage(AppState {
                db: Mutex::new(db),
                config: Mutex::new(config.clone()),
                config_file,
                ignored_hashes: Mutex::new(Vec::new()),
                last_seen: Mutex::new(0),
                tray_toggle: Mutex::new(None),
                paste_pending: Mutex::new(false),
                paste_running: AtomicBool::new(false),
                prev_hwnd: Mutex::new(0),
                panel_pinned: AtomicBool::new(false),
                dragging: AtomicBool::new(false),
                main_focused: AtomicBool::new(false),
                pending_size: Mutex::new(None),
            });

            // 托盘
            let show = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let toggle = CheckMenuItem::with_id(app, "toggle", "开启剪贴板记录", true, config.enabled, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &toggle, &settings, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .tooltip(&format!("剪贴板管家 ({})", config.hotkey))
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => toggle_window(app),
                    "toggle" => {
                        let current = app.state::<AppState>().config.lock().unwrap().enabled;
                        set_recording_enabled(app, !current);
                    }
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

            // 托盘开关存入状态，供其他界面同步勾选
            *app.state::<AppState>().tray_toggle.lock().unwrap() = Some(toggle);

            // 全局热键（默认 Ctrl+`）
            register_hotkey(app.handle(), &config.hotkey)?;

            // 开机自启状态同步
            apply_autostart(app.handle(), config.autostart);

            // 主窗口：关闭改隐藏；失焦自动关闭（点其他位置即关闭）
            if let Some(win) = app.get_webview_window("main") {
                // 记住窗口大小：启动时恢复上次尺寸（物理像素，避免 DPI 换算漂移）
                if config.remember_size && config.window_width > 0 && config.window_height > 0 {
                    let _ = win.set_size(tauri::PhysicalSize::new(
                        config.window_width,
                        config.window_height,
                    ));
                }
                let w = win.clone();
                win.on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                    WindowEvent::Resized(size) => {
                        // 记住窗口大小开启时：暂存新尺寸，由监听线程统一落盘
                        let state = w.state::<AppState>();
                        if state.config.lock().unwrap().remember_size {
                            *state.pending_size.lock().unwrap() = Some((size.width, size.height));
                        }
                    }
                    WindowEvent::Focused(focused) => {
                        let state = w.state::<AppState>();
                        state.main_focused.store(*focused, Ordering::SeqCst);
                        if *focused {
                            return;
                        }
                        // 失焦延迟复查：拖动/缩放是系统模态操作，会造成瞬时失焦；
                        // 等 150ms 确认仍无焦点、未钉住、未在拖动，才真正隐藏
                        let w2 = w.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(150));
                            let st = w2.state::<AppState>();
                            if st.panel_pinned.load(Ordering::SeqCst) {
                                return;
                            }
                            if st.dragging.load(Ordering::SeqCst) {
                                return;
                            }
                            if st.main_focused.load(Ordering::SeqCst) {
                                return;
                            }
                            let _ = w2.hide();
                        });
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
            open_clip_with_system,
            count_pinned_between,
            delete_between,
            set_enabled,
            get_clip_image,
            set_panel_pinned,
            get_panel_pinned,
            start_drag
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
