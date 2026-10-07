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

mod lan;
mod relay;

// macOS 的 NSPasteboard 不是线程安全的：后台线程读写会与应用主线程（WebKit 周期性
// 轮询 changeCount）竞争，导致内存损坏闪退（tauri-plugins-workspace#3205）。
// 进程内所有 arboard 读写统一走这把锁串行化；mac 上写操作还会调度到主线程执行。
static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());

// ---------- 配置 ----------

/// 面板背景图设置：选区 / 旋转在设置页用 canvas 烘焙进缓存图（lscopy-bg.png），
/// 运行时主面板只按 mode + opacity 用 CSS 应用缓存图
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct BackgroundConfig {
    pub enabled: bool,   // 是否启用面板背景图
    pub mode: String,    // "stretch" 拉伸铺满 | "tile" 原尺寸平铺
    pub opacity: f64,    // 背景图不透明度 0.05 - 1.0
    pub scale_w: f64,    // 图片宽度占面板百分比 5 - 300（100 = 与面板同宽）
    pub scale_h: f64,    // 图片高度占面板百分比 5 - 300（100 = 与面板同高）
    pub apply_settings: bool, // 背景同时应用到设置窗口
    pub apply_transfer: bool, // 背景同时应用到互传文件窗口
    pub rotation: i32,   // 旋转角度（0/90/180/270，烘焙进缓存图）
    pub region: Option<BgRegion>, // 归一化选区（基于旋转后的源图），None = 整图
    pub has_image: bool, // 是否已选择图片（源图已入库）
}

/// 背景图选区：归一化坐标（0-1），相对于旋转后的源图
#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct BgRegion {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "stretch".into(),
            opacity: 0.6,
            scale_w: 100.0,
            scale_h: 100.0,
            apply_settings: false,
            apply_transfer: false,
            rotation: 0,
            region: None,
            has_image: false,
        }
    }
}

impl Default for BgRegion {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    pub hotkey: String,          // 例如 "Ctrl+`" / "Ctrl+Shift+V"
    pub autostart: bool,         // 开机自启
    pub silent_start: bool,      // 静默启动（不弹主窗口）
    pub db_dir: Option<String>,  // 数据库目录；None = 启动文件所在目录
    pub config_dir: Option<String>, // 配置文件目录；None = 启动文件所在目录
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
    pub follow_cursor_monitor: bool, // 多显示器：唤起时面板跟随光标所在屏幕
    pub window_effect: String,       // 主面板窗口材质："default" | "acrylic" | "vibrancy" | "mica"
    pub background: BackgroundConfig, // 面板背景图设置
}

/// 默认全局快捷键：全平台统一 Ctrl+`（mac 上即 Control+`）
fn default_hotkey() -> String {
    "Ctrl+`".into()
}

/// 快捷键显示形式：mac 上把修饰键转成符号（⌃⇧⌥⌘），其他平台原样返回
fn format_hotkey_display(hk: &str) -> String {
    if !cfg!(target_os = "macos") {
        return hk.to_string();
    }
    hk.split('+')
        .map(|p| match p.trim().to_lowercase().as_str() {
            "ctrl" | "control" => "⌃".to_string(),
            "shift" => "⇧".to_string(),
            "alt" | "option" => "⌥".to_string(),
            "win" | "cmd" | "command" | "super" | "meta" => "⌘".to_string(),
            _ => p.trim().to_string(),
        })
        .collect::<Vec<_>>()
        .join("")
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: default_hotkey(),
            autostart: false,
            silent_start: true,
            db_dir: None,
            config_dir: None,
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
            follow_cursor_monitor: true,
            window_effect: "default".into(),
            background: BackgroundConfig::default(),
        }
    }
}

/// 合并后的配置文件结构：通用设置 + 局域网同步设置 + 云端中继设置保存在同一个 JSON
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct ConfigFile {
    app: AppConfig,
    lan: lan::LanSettings,
    relay: relay::RelaySettings,
}

/// 配置文件目录指针文件：内容是配置文件所在目录（空 = 默认数据目录）。
/// 放在 default_data_dir()（mac 上不能放 .app 包内——更新会整体替换 bundle，
/// 包内文件全部丢失，且改写包内容会破坏代码签名）
fn config_pointer_file() -> PathBuf {
    default_data_dir().join("lscopy-config-dir.txt")
}

/// 读取目录指针：优先新位置，兼容 exe 同目录的旧指针（仅迁移过渡期有用；
/// mac 更新后旧 bundle 已整体被替换，旧指针随之消失，需重新设置一次）
fn read_config_pointer() -> Option<String> {
    let read = |p: PathBuf| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    read(config_pointer_file()).or_else(|| read(exe_dir().join("lscopy-config-dir.txt")))
}

/// 当前生效的配置文件路径（读指针文件决定目录）
fn current_config_file() -> PathBuf {
    let dir = read_config_pointer()
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);
    dir.join("lscopy-config.json")
}

/// 按配置里的 config_dir 计算配置文件路径
fn effective_config_file(cfg: &AppConfig) -> PathBuf {
    cfg.config_dir
        .as_ref()
        .map(|d| d.trim())
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir)
        .join("lscopy-config.json")
}

/// 解析配置文件文本为（通用设置, 局域网设置, 中继设置）。
/// 兼容旧格式：扁平的 AppConfig JSON（局域网设置再从旧 lscopy-lan.json 读）。
fn load_config_full(raw: Option<&str>) -> (AppConfig, lan::LanSettings, relay::RelaySettings) {
    if let Some(text) = raw {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            if v.get("app").is_some() || v.get("lan").is_some() || v.get("relay").is_some() {
                // 新格式：{ "app": {...}, "lan": {...}, "relay": {...} }（各字段缺失时按默认值填充）
                if let Ok(mut cf) = serde_json::from_value::<ConfigFile>(v) {
                    cf.lan = lan::parse_settings(
                        serde_json::to_string(&cf.lan).ok().as_deref(),
                    );
                    return (cf.app, cf.lan, cf.relay);
                }
            } else {
                // 旧格式：整个文件就是 AppConfig
                let app: AppConfig = serde_json::from_value(v).unwrap_or_default();
                let old_lan = exe_dir().join("lscopy-lan.json");
                let lan_raw = std::fs::read_to_string(&old_lan).ok();
                return (
                    app,
                    lan::parse_settings(lan_raw.as_deref()),
                    relay::RelaySettings::default(),
                );
            }
        }
    }
    (
        AppConfig::default(),
        lan::parse_settings(None),
        relay::RelaySettings::default(),
    )
}

/// 把「通用设置 + 局域网设置 + 中继设置」合并写入配置文件。
/// 锁顺序固定：config → lan.settings → relay.settings → config_file，各调用点不得反向加锁。
pub(crate) fn persist_config(state: &AppState) -> Result<(), String> {
    let app = state.config.lock().unwrap().clone();
    let lan = state.lan.settings.lock().unwrap().clone();
    let relay = state.relay.settings.lock().unwrap().clone();
    let path = state.config_file.lock().unwrap().clone();
    let cf = ConfigFile { app, lan, relay };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(&cf).map_err(|e| e.to_string())?;
    std::fs::write(&path, &json).map_err(|e| e.to_string())?;
    // 镜像一份到默认数据目录：macOS 更新会整体替换 .app，若配置目录被更新抹掉
    // （如自定义目录选在包内），下次启动可从镜像恢复上次配置（含数据库/配置文件
    // 目录设置），不用重新配置
    let mirror = default_data_dir().join("lscopy-config.json");
    if mirror != path {
        if let Some(dir) = mirror.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&mirror, &json);
    }
    Ok(())
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
    pub(crate) db: Mutex<Connection>,
    pub(crate) config: Mutex<AppConfig>,
    config_file: Mutex<PathBuf>,
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
    // 局域网同步模块共享状态
    pub(crate) lan: lan::LanShared,
    // 云端中继模块共享状态
    pub(crate) relay: relay::RelayShared,
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(crate) fn hash_bytes(data: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

// 软件所在目录（Windows 便携模式的默认数据位置）
pub(crate) fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 默认数据目录（配置文件/数据库/目录指针的缺省位置）。
/// Windows 保持便携模式 = exe 同目录；macOS 的 .app 会被更新整体替换，
/// 包内数据（含目录指针）每次更新都会丢，故缺省放 Application Support。
pub(crate) fn default_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            return home
                .join("Library")
                .join("Application Support")
                .join("com.lsh.lscopy");
        }
    }
    exe_dir()
}

fn effective_db_path(cfg: &AppConfig) -> PathBuf {
    let dir = match &cfg.db_dir {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => default_data_dir(),
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
        CREATE INDEX IF NOT EXISTS idx_clips_time ON clips(created_at DESC);
        CREATE TABLE IF NOT EXISTS transfers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            direction TEXT NOT NULL,
            peer TEXT NOT NULL,
            file_name TEXT NOT NULL,
            path TEXT,
            size INTEGER NOT NULL DEFAULT 0,
            ok INTEGER NOT NULL DEFAULT 0,
            msg TEXT,
            created_at INTEGER NOT NULL
        );",
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
    // 旧版本库迁移：局域网同步来源追踪（环回防护用）
    if conn.prepare("SELECT remote_device_id FROM clips LIMIT 1").is_err() {
        conn.execute_batch(
            "ALTER TABLE clips ADD COLUMN remote_device_id TEXT;
             ALTER TABLE clips ADD COLUMN remote_id INTEGER;",
        )
        .map_err(|e| e.to_string())?;
    }
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

pub(crate) fn encode_png(rgba: image::RgbaImage) -> Option<(Vec<u8>, u32, u32)> {
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
                        drop(cfg);
                        let _ = persist_config(&state);
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

            let Some(cand) = ({
                let _g = CLIPBOARD_LOCK.lock().unwrap();
                read_clipboard(&mut cb)
            }) else {
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
                // 云端中继：文本默认走中继；图片需开「图片经中继」开关（设计文档 §7.3）
                match &cand {
                    Cand::Text(text, h) => {
                        drop(db);
                        relay::push_local_text(&state, text, *h);
                    }
                    Cand::Image(png, w, hgt, h) => {
                        drop(db);
                        relay::push_local_image(&state, png, *w, *hgt, *h);
                    }
                    Cand::File(..) => {}
                }
            }
        }
    });
}

// 删除记录 / 重新开启记录时调用：读取当前系统剪贴板内容哈希并加入忽略集合
fn ignore_current_clipboard(state: &AppState) {
    let h = (|| {
        let _g = CLIPBOARD_LOCK.lock().unwrap();
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

// 写剪贴板（粘贴/仅复制共用入口）：
// 全程持 CLIPBOARD_LOCK 与监听线程的读互斥；macOS 上 NSPasteboard 只允许主线程访问，
// 否则与 WebKit 主线程的剪贴板轮询竞争导致闪退，故 mac 调度到主线程执行并同步等待结果。
fn set_clipboard_by_id_safe(app: &AppHandle, id: i64) -> Result<(), String> {
    let _guard = CLIPBOARD_LOCK.lock().unwrap();
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let app2 = app.clone();
        app.run_on_main_thread(move || {
            let st = app2.state::<AppState>();
            let _ = tx.send(set_clipboard_by_id(&st, id));
        })
        .map_err(|e| format!("调度主线程失败: {e}"))?;
        rx.recv().map_err(|_| "主线程写剪贴板无响应".to_string())?
    }
    #[cfg(not(target_os = "macos"))]
    {
        let st = app.state::<AppState>();
        set_clipboard_by_id(&st, id)
    }
}

#[tauri::command]
fn copy_clip(app: AppHandle, id: i64) -> Result<(), String> {
    set_clipboard_by_id_safe(&app, id)
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
        // macOS 上必须用物理键码 Other(0x09)（kVK_ANSI_V）：Key::Unicode 会走
        // enigo 0.2.1 的 get_layoutdependent_keycode → TIS 键盘布局查询，
        // 在后台线程 TISGetInputSourceProperty 可能返回 NULL，
        // release 版直接 CFDataGetBytePtr(NULL) 段错误（进程闪退）
        #[cfg(target_os = "macos")]
        let v_key = Key::Other(9);
        #[cfg(not(target_os = "macos"))]
        let v_key = Key::Unicode('v');
        let _ = enigo.key(v_key, Direction::Click);
        let _ = enigo.key(modifier, Direction::Release);
    }
}

// 模拟粘贴入口：macOS 上 enigo 初始化会调用 AppKit（NSEvent::doubleClickInterval），
// AppKit 应在主线程访问，故 mac 调度到主线程执行并同步等待；其他平台原线程执行。
fn simulate_paste_safe(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        if app
            .run_on_main_thread(move || {
                simulate_paste();
                let _ = tx.send(());
            })
            .is_ok()
        {
            let _ = rx.recv();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        simulate_paste();
    }
}

// macOS：当前前台 App 是否为访达（Finder）。
// 桌面/访达窗口没有文本粘贴目标，⌘V 会让访达把剪贴板文本落成「文本剪贴」文件，
// 粘贴前检测到前台是访达就跳过模拟按键（剪贴板仍已更新，可到目标处手动粘贴）。
// 用 lsappinfo 查询前台应用的 bundle id，无需额外权限（AppleScript 会弹自动化授权）。
#[cfg(target_os = "macos")]
fn frontmost_is_finder() -> bool {
    let Ok(front) = std::process::Command::new("lsappinfo").arg("front").output() else {
        return false;
    };
    let asn = String::from_utf8_lossy(&front.stdout).trim().to_string();
    if asn.is_empty() {
        return false;
    }
    let Ok(info) = std::process::Command::new("lsappinfo")
        .args(["info", "-only", "bundleID", &asn])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&info.stdout).contains("com.apple.finder")
}

// ---------- macOS 辅助功能权限 ----------

/// 本进程是否已有「辅助功能」授权（AXIsProcessTrusted）
#[cfg(target_os = "macos")]
fn accessibility_trusted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}

/// 检查辅助功能权限；未授权时弹窗引导并打开系统设置页，返回 false。
/// 注意：mac 上覆盖安装/更新后旧授权可能失效，需在设置里移除 lscopy 再重新添加。
#[cfg(target_os = "macos")]
fn ensure_accessibility(app: &AppHandle) -> bool {
    if accessibility_trusted() {
        return true;
    }
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        app2.dialog()
            .message("macOS 需要「辅助功能」权限才能模拟 ⌘V 粘贴。\n\n请在刚打开的系统设置中允许 lscopy。\n如果是更新/重装后失效：先在列表里移除 lscopy，再重新添加并打开开关。\n\n（剪贴板已更新，授权前可手动 ⌘V 粘贴）")
            .title("需要辅助功能权限")
            .blocking_show();
    });
    false
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
        hide_panel_and_yield_focus(&app);
    }
    // 剪贴板内容立即更新
    set_clipboard_by_id_safe(&app, id)?;
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
        // macOS：没有辅助功能权限时模拟按键会被系统静默丢弃（不崩溃、不报错），
        // 提前检查并引导授权，避免「光标回来了但内容没粘贴」
        #[cfg(target_os = "macos")]
        if !ensure_accessibility(&app) {
            continue; // 剪贴板已更新，用户可手动 ⌘V
        }
        // 等焦点 settling：Windows 50ms 在响应速度和可靠性之间比较平衡；
        // macOS 要等 NSApplication.hide 完成前台 App 切换，需要略久
        #[cfg(target_os = "macos")]
        std::thread::sleep(Duration::from_millis(120));
        #[cfg(not(target_os = "macos"))]
        std::thread::sleep(Duration::from_millis(50));
        // macOS：焦点已还给目标 App，此时前台是访达（桌面/访达窗口）说明没有
        // 文本粘贴目标，⌘V 会在桌面生成「文本剪贴」文件——跳过模拟按键
        #[cfg(target_os = "macos")]
        if frontmost_is_finder() {
            continue;
        }
        simulate_paste_safe(&app);
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
fn save_config(
    app: AppHandle,
    state: State<AppState>,
    config: AppConfig,
    config_dir_action: String,
) -> Result<(), String> {
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
    let old = state.config.lock().unwrap().clone();
    if old.db_dir != config.db_dir {
        let new_path = effective_db_path(&config);
        let conn = init_db(&new_path)?;
        let mut db = state.db.lock().unwrap();
        *db = conn;
    }

    // 2. 配置文件目录变更：验证新目录可写 → 更新指针 → 切换，按选择处理旧文件
    if old.config_dir != config.config_dir {
        let new_file = effective_config_file(&config);
        if let Some(dir) = new_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{}", e))?;
        }
        // 试写探测（只写不存在的临时文件，避免误覆盖）
        let probe = new_file.with_extension("tmp");
        std::fs::write(&probe, "{}").map_err(|e| format!("配置目录不可写：{}", e))?;
        let _ = std::fs::remove_file(&probe);

        let old_file = state.config_file.lock().unwrap().clone();
        // 指针文件：空字符串表示默认（默认数据目录）
        let pointer = config.config_dir.clone().unwrap_or_default();
        let pointer_file = config_pointer_file();
        if let Some(dir) = pointer_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{}", e))?;
        }
        std::fs::write(&pointer_file, pointer)
            .map_err(|e| format!("保存配置目录设置失败：{}", e))?;
        // 清理 exe 同目录的旧指针（mac 上位于 .app 包内，避免改写 bundle）
        let legacy_pointer = exe_dir().join("lscopy-config-dir.txt");
        if legacy_pointer != pointer_file {
            let _ = std::fs::remove_file(legacy_pointer);
        }
        *state.config_file.lock().unwrap() = new_file.clone();
        // 背景图（源图 + 烘焙缓存）跟随配置目录迁移：「保留旧副本」= 复制；
        // 「迁移并删除」= 复制后删旧；「不迁移」则沿用新目录里已有的背景
        if config_dir_action != "reset" && old_file != new_file {
            if let (Some(old_dir), Some(new_dir)) = (old_file.parent(), new_file.parent()) {
                let mut bg_files = bg_src_files(old_dir);
                let cache = old_dir.join(BG_CACHE_NAME);
                if cache.exists() {
                    bg_files.push(cache);
                }
                for f in bg_files {
                    if let Some(name) = f.file_name() {
                        let _ = std::fs::copy(&f, new_dir.join(name));
                        if config_dir_action == "move" {
                            let _ = std::fs::remove_file(&f);
                        }
                    }
                }
            }
        }
        // 「迁移并删除旧文件」与「删除旧文件（不迁移）」都会删掉旧位置的配置文件
        if config_dir_action != "keep" && old_file != new_file {
            let _ = std::fs::remove_file(&old_file);
        }
        // 不迁移：新目录已有配置则直接采用，没有则生成一份全新默认配置；
        // 表单里本次的其他改动一并丢弃（前端保存后会重新加载设置页）
        if config_dir_action == "reset" {
            let raw = std::fs::read_to_string(&new_file).ok();
            let (mut loaded, lan_settings, relay_settings) = load_config_full(raw.as_deref());
            // config_dir 以本次指针为准，避免沿用新目录旧文件里记录的目录
            loaded.config_dir = config.config_dir.clone();
            // 新配置的数据库目录可能不同：切换数据库连接
            let db_path = effective_db_path(&loaded);
            let conn = init_db(&db_path)?;
            *state.db.lock().unwrap() = conn;
            *state.lan.settings.lock().unwrap() = lan_settings;
            *state.relay.settings.lock().unwrap() = relay_settings;
            config = loaded;
        }
    }

    // 3. 重注册全局热键
    register_hotkey(&app, &config.hotkey)?;

    // 4. 应用开机自启
    apply_autostart(&app, config.autostart);

    // 5. 持久化 + 同步托盘开关 + 广播
    if config.enabled {
        // 从关闭切到开启时，忽略当前剪贴板内容（关闭期间的不补录）
        ignore_current_clipboard(&state);
    }
    *state.config.lock().unwrap() = config.clone();
    persist_config(&state)?;
    if let Some(item) = state.tray_toggle.lock().unwrap().as_ref() {
        let _ = item.set_checked(config.enabled);
    }
    // 窗口效果可能随配置整体保存而变化（主题明暗也影响 mica/亚克力配色），立即重应用
    apply_window_effect_all(&app, &config.window_effect, config.theme != "light");
    let _ = app.emit("config-changed", config);
    Ok(())
}

// ---------- 窗口材质效果（亚克力 / 云母 / 苹果毛玻璃） ----------

/// 给全部窗口应用窗口材质效果，可在窗口已显示时调用（立即生效）。
/// effect: "default" | "acrylic" | "vibrancy" | "mica"；dark 决定材质明暗配色。
/// 需要窗口在 tauri.conf.json 里开启 transparent，前端配合把背景改为半透明。
fn apply_window_effect_all(app: &AppHandle, effect: &str, dark: bool) {
    for label in ["main", "settings", "blocked", "transfer"] {
        if let Some(win) = app.get_webview_window(label) {
            apply_window_effect(&win, effect, dark);
        }
    }
}

/// 给单个窗口应用窗口材质效果。
fn apply_window_effect(win: &tauri::WebviewWindow, effect: &str, dark: bool) {
    #[cfg(target_os = "windows")]
    {
        // 先清掉旧效果再套新的（未应用过时 clear 无害）
        let _ = window_vibrancy::clear_acrylic(win);
        let _ = window_vibrancy::clear_mica(win);
        let _ = window_vibrancy::clear_blur(win);
        match effect {
            "acrylic" => {
                let color = if dark { (28, 28, 44, 140) } else { (240, 241, 245, 140) };
                let _ = window_vibrancy::apply_acrylic(win, Some(color));
            }
            "mica" => {
                let _ = window_vibrancy::apply_mica(win, Some(dark));
            }
            // 「苹果毛玻璃」：Windows 没有 Vibrancy，用 Blur 近似
            "vibrancy" => {
                let color = if dark { (22, 22, 34, 110) } else { (239, 241, 245, 110) };
                let _ = window_vibrancy::apply_blur(win, Some(color));
            }
            _ => {} // default：仅清除
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = dark;
        let _ = window_vibrancy::clear_vibrancy(win);
        // 亚克力/云母在 mac 上没有对应材质，统一用 Vibrancy 呈现
        let material = match effect {
            "acrylic" => Some(window_vibrancy::NSVisualEffectMaterial::Sidebar),
            "mica" => Some(window_vibrancy::NSVisualEffectMaterial::UnderWindowBackground),
            "vibrancy" => Some(window_vibrancy::NSVisualEffectMaterial::HudWindow),
            _ => None,
        };
        if let Some(m) = material {
            let _ = window_vibrancy::apply_vibrancy(win, m, None, Some(10.0));
        }
    }
}

#[tauri::command]
fn set_window_effect(app: AppHandle, state: State<AppState>, effect: String) -> Result<(), String> {
    const VALID: [&str; 4] = ["default", "acrylic", "vibrancy", "mica"];
    if !VALID.contains(&effect.as_str()) {
        return Err(format!("未知的窗口效果: {effect}"));
    }
    let cfg = {
        let mut c = state.config.lock().unwrap();
        c.window_effect = effect;
        c.clone()
    };
    persist_config(&state)?;
    apply_window_effect_all(&app, &cfg.window_effect, cfg.theme != "light");
    let _ = app.emit("config-changed", cfg);
    Ok(())
}

// ---------- 面板背景图 ----------
// 源图复制为配置目录下的 lscopy-bg-src.<ext>；设置页烘焙（旋转+选区）后的成品
// 存为 lscopy-bg.png，主面板直接按 base64 读取应用。换目录时随配置文件一起迁移。

const BG_SRC_STEM: &str = "lscopy-bg-src";
const BG_CACHE_NAME: &str = "lscopy-bg.png";
const BG_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif"];

/// 背景图存放目录：跟随当前生效的配置文件所在目录
fn bg_dir(state: &AppState) -> PathBuf {
    state
        .config_file
        .lock()
        .unwrap()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_data_dir)
}

/// 目录下已存在的背景源图（按支持的扩展名枚举）
fn bg_src_files(dir: &std::path::Path) -> Vec<PathBuf> {
    BG_EXTS
        .iter()
        .map(|e| dir.join(format!("{BG_SRC_STEM}.{e}")))
        .filter(|p| p.exists())
        .collect()
}

#[derive(Serialize)]
pub struct BgImageData {
    b64: String,
    mime: String,
}

/// 选择背景图：把源图复制进配置目录（换图后旧的烘焙缓存作废）
#[tauri::command]
fn import_background_source(state: State<AppState>, path: String) -> Result<(), String> {
    let src = PathBuf::from(&path);
    if !src.exists() {
        return Err(format!("文件不存在: {path}"));
    }
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    if !BG_EXTS.contains(&ext.as_str()) {
        return Err("仅支持 png / jpg / webp / bmp / gif 图片".into());
    }
    let size = std::fs::metadata(&src).map_err(|e| e.to_string())?.len();
    if size > 30 * 1024 * 1024 {
        return Err("图片过大（超过 30MB），请换一张".into());
    }
    let dir = bg_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for f in bg_src_files(&dir) {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_file(dir.join(BG_CACHE_NAME));
    std::fs::copy(&src, dir.join(format!("{BG_SRC_STEM}.{ext}")))
        .map_err(|e| format!("复制图片失败: {e}"))?;
    Ok(())
}

/// 读取背景源图（设置页重新编辑选区/旋转时回显用）
#[tauri::command]
fn read_background_source(state: State<AppState>) -> Option<BgImageData> {
    let dir = bg_dir(&state);
    let f = bg_src_files(&dir).into_iter().next()?;
    let bytes = std::fs::read(&f).ok()?;
    let mime = match f
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        _ => return None, // bg_src_files 只枚举已知扩展名，兜底防御
    };
    Some(BgImageData {
        b64: B64.encode(bytes),
        mime: mime.to_string(),
    })
}

/// 保存设置页烘焙后的成品背景图（base64 PNG）
#[tauri::command]
fn save_background_cache(state: State<AppState>, b64: String) -> Result<(), String> {
    let bytes = B64
        .decode(b64)
        .map_err(|e| format!("图片数据解码失败: {e}"))?;
    if bytes.len() > 30 * 1024 * 1024 {
        return Err("处理后的图片过大".into());
    }
    let dir = bg_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(BG_CACHE_NAME), bytes).map_err(|e| format!("写入背景图失败: {e}"))
}

/// 主面板读取烘焙后的背景图
#[tauri::command]
fn get_background_cache(state: State<AppState>) -> Option<String> {
    std::fs::read(bg_dir(&state).join(BG_CACHE_NAME))
        .ok()
        .map(|b| B64.encode(b))
}

/// 保存背景图设置（即时落盘 + 广播，与 set_window_effect 同一模式）
#[tauri::command]
fn set_background_config(
    app: AppHandle,
    state: State<AppState>,
    bg: BackgroundConfig,
) -> Result<(), String> {
    let cfg = {
        let mut c = state.config.lock().unwrap();
        c.background = bg;
        c.clone()
    };
    persist_config(&state)?;
    let _ = app.emit("config-changed", cfg);
    Ok(())
}

/// 清除背景图：删除源图与缓存，并重置背景设置为默认
#[tauri::command]
fn clear_background_image(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let dir = bg_dir(&state);
    for f in bg_src_files(&dir) {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_file(dir.join(BG_CACHE_NAME));
    let cfg = {
        let mut c = state.config.lock().unwrap();
        c.background = BackgroundConfig::default();
        c.clone()
    };
    persist_config(&state)?;
    let _ = app.emit("config-changed", cfg);
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
        cfg.clone()
    };
    let _ = persist_config(&state);
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

// 打开黑名单管理窗口
#[tauri::command]
fn open_blocked(app: AppHandle) {
    if let Some(w) = app.get_webview_window("blocked") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

// 打开互传文件窗口
#[tauri::command]
fn open_transfer(app: AppHandle) {
    if let Some(w) = app.get_webview_window("transfer") {
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

// 隐藏主面板；macOS 上同时把焦点还给之前的 App：
// mac 上单纯 win.hide() 不改变活跃 App（本进程仍是 frontmost，焦点回不到之前的文本框，
// ⌘V 甚至会打进自己），NSApplication.hide() 才会让系统重新激活之前的前台应用。
// 仅在设置/黑名单窗口都不可见时才隐藏整个 App，避免误伤其他窗口。
fn hide_panel_and_yield_focus(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    #[cfg(target_os = "macos")]
    {
        let others_visible = ["settings", "blocked", "transfer"].iter().any(|l| {
            app.get_webview_window(l)
                .map(|w| w.is_visible().unwrap_or(false))
                .unwrap_or(false)
        });
        if !others_visible {
            let _ = app.hide();
        }
    }
}

#[tauri::command]
fn hide_panel(app: AppHandle) {
    hide_panel_and_yield_focus(&app);
}

/// 设置页切换窗口效果时调用：仅把主面板显示出来做实时预览，不抢焦点。
/// 主面板是 alwaysOnTop，会直接浮在设置窗口上方，用户切换下拉即可实时看到效果。
#[tauri::command]
fn show_panel(app: AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if !win.is_visible().unwrap_or(false) {
            // 记住当前前台窗口，之后若在面板上粘贴能把焦点还回去
            *app.state::<AppState>().prev_hwnd.lock().unwrap() = foreground_hwnd();
            let _ = win.show();
        }
    }
}

fn toggle_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if win.is_visible().unwrap_or(false) {
            hide_panel_and_yield_focus(app);
        } else {
            // 记住弹出前的前台窗口，粘贴后把焦点还给它
            *app.state::<AppState>().prev_hwnd.lock().unwrap() = foreground_hwnd();
            if app
                .state::<AppState>()
                .config
                .lock()
                .unwrap()
                .follow_cursor_monitor
            {
                move_panel_to_cursor_monitor(&win);
            }
            let _ = win.show();
            let _ = win.set_focus();
            let _ = app.emit("panel-shown", ());
        }
    }
}

/// 多显示器：唤起前把面板挪到光标所在屏幕。
/// 面板中心已在该屏幕上则不挪动（尊重用户摆过的位置），否则居中到该屏幕。
fn move_panel_to_cursor_monitor(win: &tauri::WebviewWindow) {
    let (Ok(cursor), Ok(monitors)) = (win.cursor_position(), win.available_monitors()) else {
        return;
    };
    // 显示器与窗口坐标都是物理像素，同一坐标空间，可直接做包含判断
    let contains = |m: &tauri::window::Monitor, x: f64, y: f64| {
        let p = m.position();
        let s = m.size();
        x >= p.x as f64
            && x < (p.x + s.width as i32) as f64
            && y >= p.y as f64
            && y < (p.y + s.height as i32) as f64
    };
    let Some(mon) = monitors.iter().find(|m| contains(m, cursor.x, cursor.y)) else {
        return;
    };
    let (Ok(wp), Ok(ws)) = (win.outer_position(), win.outer_size()) else {
        return;
    };
    let cx = wp.x as f64 + ws.width as f64 / 2.0;
    let cy = wp.y as f64 + ws.height as f64 / 2.0;
    if contains(mon, cx, cy) {
        return;
    }
    let mp = mon.position();
    let ms = mon.size();
    let nx = mp.x + (ms.width as i32 - ws.width as i32) / 2;
    let ny = mp.y + (ms.height as i32 - ws.height as i32) / 2;
    let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
        nx, ny,
    )));
}

// ---------- 便携版应用内更新 ----------
// NSIS 安装版走 tauri-plugin-updater 官方流程（NSIS 会装回注册表记录的原目录）；
// 绿色便携版是单文件 exe，不能装到别处（配置/数据库都在 exe 同目录），这里实现
// 「下载便携版 exe 到本目录 → 校验 SHA-256 → 退出后由 PowerShell 脚本替换并重启」。

/// 判断当前是安装版还是便携版：注册表有 NSIS 卸载项即安装版
#[tauri::command]
fn update_install_kind() -> String {
    #[cfg(target_family = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            for key in ["lscopy", "com.lsh.lscopy"] {
                let path =
                    format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{key}");
                if RegKey::predef(hive).open_subkey(&path).is_ok() {
                    return "installed".to_string();
                }
            }
        }
        "portable".to_string()
    }
    #[cfg(not(target_family = "windows"))]
    "installed".to_string()
}

/// 计算便携版 exe 的下载目标路径：默认 exe 同目录的 lscopy_new.exe（等替换）；
/// 指定目录时为该目录下的 lscopy.exe（用户选择「下载到其他目录」）
#[tauri::command]
fn portable_update_begin(target_dir: Option<String>) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_dir = exe
        .parent()
        .ok_or("无法确定程序目录")?
        .to_path_buf();
    let (dir, name) = match target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(d) => (std::path::PathBuf::from(d), "lscopy.exe"),
        None => (exe_dir, "lscopy_new.exe"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法写入目录 {}: {e}", dir.display()))?;
    let path = dir.join(name);
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    Ok(path.to_string_lossy().to_string())
}

/// 流式下载更新包到指定路径，进度通过 portable-update-progress 事件回传
#[tauri::command]
async fn portable_update_download(app: AppHandle, url: String, path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        use std::io::Read;
        let resp = ureq::get(&url)
            .call()
            .map_err(|e| format!("下载失败: {e}"))?;
        let total: u64 = resp
            .header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(&path)
            .map_err(|e| format!("无法创建文件 {path}: {e}"))?;
        let mut buf = [0u8; 65536];
        let mut sent: u64 = 0;
        loop {
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            use std::io::Write;
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            sent += n as u64;
            let _ = app.emit(
                "portable-update-progress",
                serde_json::json!({ "sent": sent, "total": total }),
            );
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 拉取文本资源（用于读取 Release 的 sha256sums 校验文件）
#[tauri::command]
async fn http_get_text(url: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        ureq::get(&url)
            .call()
            .map_err(|e| format!("请求失败: {e}"))?
            .into_string()
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 校验下载文件的 SHA-256（与 Release 的 sha256sums 比对，防止下载损坏/被篡改）
#[tauri::command]
fn portable_update_verify(path: String, expected_sha256: String) -> Result<bool, String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let digest = Sha256::digest(&bytes);
    let got: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(got.eq_ignore_ascii_case(expected_sha256.trim()))
}

/// 退出本进程，由辅助进程等待退出后替换 exe 并重启新版。
/// 辅助进程 = 自身 exe 拷到 %TEMP% 的副本：GUI 子系统天然无控制台窗口。
/// （早期版本用 PowerShell 脚本，powershell.exe 是控制台程序，CREATE_NO_WINDOW
/// 在 Windows Terminal 作为默认终端时仍会弹窗，故弃用。）
#[tauri::command]
fn portable_update_apply(app: AppHandle, new_path: String) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let helper = std::env::temp_dir().join("lscopy-updater.exe");
    std::fs::copy(&exe, &helper).map_err(|e| format!("创建更新辅助程序失败: {e}"))?;
    let mut cmd = std::process::Command::new(&helper);
    cmd.arg("--apply-update").arg(&new_path).arg(&exe);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    cmd.spawn().map_err(|e| format!("启动更新辅助程序失败: {e}"))?;
    app.exit(0);
    Ok(())
}

/// 便携版更新辅助进程入口：等旧进程退出（exe 文件锁释放）后用新 exe 覆盖旧 exe，
/// 再启动新版。运行在 %TEMP% 的副本上，与替换的源/目标路径都不冲突。
/// 仅 Windows 便携版使用（macOS 走官方 updater）。
#[cfg(windows)]
fn apply_update_helper(new_path: &str, old_path: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match std::fs::rename(new_path, old_path) {
            Ok(_) => break,
            // 旧进程未退出时目标 exe 被占用，rename 失败，稍候重试
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(400))
            }
            // 超时放弃：新 exe 保留在原位，用户可手动替换
            Err(_) => return,
        }
    }
    let mut cmd = std::process::Command::new(old_path);
    if let Some(dir) = std::path::Path::new(old_path).parent() {
        cmd.current_dir(dir);
    }
    let _ = cmd.spawn();
}

// ---------- 安装版应用内更新（Windows NSIS） ----------
// 不用官方 updater 静默安装：先把安装包流式下载到系统「下载」目录（好找、可留档），
// 校验 SHA-256 后由用户确认运行安装程序（NSIS 会装回注册表记录的原目录）。
// 下载/校验复用便携版的 portable_update_download / portable_update_verify。

/// 安装包的下载目标路径：系统「下载」目录下的安装包文件
#[tauri::command]
fn installer_update_path(filename: String) -> Result<String, String> {
    let dir = dirs::download_dir().ok_or("找不到系统下载目录")?;
    let path = dir.join(filename);
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    Ok(path.to_string_lossy().to_string())
}

/// 运行下载好的 NSIS 安装包并退出本程序（安装程序装回原目录后会重启新版）
#[tauri::command]
fn installer_update_run(app: AppHandle, path: String) -> Result<(), String> {
    std::process::Command::new(&path)
        .spawn()
        .map_err(|e| format!("启动安装程序失败: {e}"))?;
    app.exit(0);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().collect();
        // 便携版更新辅助进程：--apply-update <new_exe> <old_exe>，跑完即退，
        // 不进入 Tauri 初始化（避免单实例插件把它拦下）
        if args.len() >= 4 && args[1] == "--apply-update" {
            apply_update_helper(&args[2], &args[3]);
            return;
        }
        // 正常启动时顺手清理上次更新遗留的临时辅助程序副本（可能仍被占用，失败无碍）
        let _ = std::fs::remove_file(std::env::temp_dir().join("lscopy-updater.exe"));
    }
    tauri::Builder::default()
        // 单实例：重复启动时提示并聚焦已有窗口
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            app.dialog()
                .message("共享剪贴板已经在运行中，可通过托盘图标或快捷键呼出。")
                .title("提示")
                .blocking_show();
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        // 应用内更新：检查 GitHub Release 的 latest.json，下载并安装新版本
        .plugin(tauri_plugin_updater::Builder::new().build())
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
            // 配置文件默认在 default_data_dir()（Windows 便携模式 = exe 同目录；
            // macOS = ~/Library/Application Support/com.lsh.lscopy，因为更新会整体替换
            // .app，包内数据每次更新都会丢），实际目录由指针文件 lscopy-config-dir.txt
            // 决定（可在设置里自定义）。旧版本配置在系统配置目录：首次启动自动迁移过来
            let config_file = current_config_file();
            // 配置目录被抹掉时（如 macOS 更新整体替换 .app、自定义目录选在包内），
            // 从默认数据目录的镜像恢复上次配置，更新后无需重配数据库/配置文件目录
            if !config_file.exists() {
                let mirror = default_data_dir().join("lscopy-config.json");
                if mirror != config_file && mirror.exists() {
                    if let Some(dir) = config_file.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::copy(&mirror, &config_file);
                }
            }
            if !config_file.exists() {
                let legacy = app
                    .path()
                    .app_config_dir()
                    .map_err(|e| e.to_string())?
                    .join("lscopy-config.json");
                if legacy.exists() && legacy != config_file {
                    let _ = std::fs::copy(&legacy, &config_file);
                }
            }
            // mac 过渡兜底：旧版本配置/数据库可能在 .app 包内（exe 同目录），
            // 新默认位置没有时拷过来（更新场景旧 bundle 已被替换，查不到也无害）
            if !config_file.exists() {
                let in_bundle = exe_dir().join("lscopy-config.json");
                if in_bundle != config_file && in_bundle.exists() {
                    let _ = std::fs::copy(&in_bundle, &config_file);
                }
            }
            let raw = std::fs::read_to_string(&config_file).ok();
            let (config, lan_settings, relay_settings) = load_config_full(raw.as_deref());
            let db_path = effective_db_path(&config);
            if !db_path.exists() {
                let in_bundle_db = exe_dir().join("lscopy.db");
                if in_bundle_db != db_path && in_bundle_db.exists() {
                    if let Some(dir) = db_path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::copy(&in_bundle_db, &db_path);
                }
            }
            let db = init_db(&db_path)?;
            app.manage(AppState {
                db: Mutex::new(db),
                config: Mutex::new(config.clone()),
                config_file: Mutex::new(config_file),
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
                lan: lan::LanShared::new(lan_settings),
                relay: relay::RelayShared::new(relay_settings),
            });
            // 统一为合并格式落盘一次（旧格式/旧局域网文件 → 新 ConfigFile）
            {
                let state = app.state::<AppState>();
                let _ = persist_config(&state);
            }
            // 旧版独立局域网配置文件：迁移后备份为 .bak
            let old_lan = exe_dir().join("lscopy-lan.json");
            if old_lan.exists() {
                let _ = std::fs::rename(&old_lan, old_lan.with_extension("json.bak"));
            }

            // 托盘
            let show = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let toggle = CheckMenuItem::with_id(app, "toggle", "开启剪贴板记录", true, config.enabled, None::<&str>)?;
            let transfer = MenuItem::with_id(app, "transfer", "互传文件", true, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &toggle, &transfer, &settings, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .tooltip(format!("共享剪贴板 ({})", format_hotkey_display(&config.hotkey)))
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
                    "transfer" => {
                        if let Some(w) = app.get_webview_window("transfer") {
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
                // 启动时应用配置的窗口材质效果（默认不透明则无操作）
                apply_window_effect_all(app.handle(), &config.window_effect, config.theme != "light");
                let w = win.clone();
                win.on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        hide_panel_and_yield_focus(w.app_handle());
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
                            hide_panel_and_yield_focus(w2.app_handle());
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

            // 黑名单窗口：关闭改隐藏
            if let Some(win) = app.get_webview_window("blocked") {
                let w = win.clone();
                win.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            // 互传文件窗口：关闭改隐藏
            if let Some(win) = app.get_webview_window("transfer") {
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
            // 启动局域网同步模块（beacon 收发 + 自动同步，按开关启停 HTTP 服务）
            lan::start(app.handle());
            // 启动云端中继客户端（按配置连接，未启用时空转等待）
            relay::start(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_clips,
            toggle_pin,
            copy_clip,
            paste_clip,
            hide_panel,
            count_pinned_in_range,
            delete_range,
            delete_clip,
            get_config,
            save_config,
            set_window_effect,
            import_background_source,
            read_background_source,
            save_background_cache,
            get_background_cache,
            set_background_config,
            clear_background_image,
            show_panel,
            get_db_info,
            export_clips,
            import_clips,
            open_settings,
            open_blocked,
            open_transfer,
            list_system_fonts,
            open_clip_with_system,
            count_pinned_between,
            delete_between,
            set_enabled,
            get_clip_image,
            set_panel_pinned,
            get_panel_pinned,
            start_drag,
            lan::lan_get_state,
            lan::lan_update_settings,
            lan::lan_regenerate_token,
            lan::lan_add_device,
            lan::lan_sweep,
            lan::lan_pair,
            lan::lan_unpair,
            lan::lan_forget_device,
            lan::lan_sync_now,
            lan::lan_block,
            lan::lan_unblock,
            lan::lan_respond_pair,
            relay::relay_get_state,
            relay::relay_update_settings,
            lan::transfer::transfer_send,
            lan::transfer::transfer_send_ip,
            lan::transfer::transfer_cancel,
            lan::transfer::transfer_respond_recv,
            lan::transfer::transfer_history,
            lan::transfer::transfer_delete,
            lan::transfer::transfer_clear_history,
            lan::transfer::transfer_reveal,
            lan::transfer::transfer_open_dir,
            update_install_kind,
            portable_update_begin,
            portable_update_download,
            portable_update_verify,
            portable_update_apply,
            installer_update_path,
            installer_update_run,
            http_get_text
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
