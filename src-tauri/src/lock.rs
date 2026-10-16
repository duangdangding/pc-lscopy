// 全局安全锁定：密码 / 组合键两种解锁方式、手动锁定、超时自动锁定、忘记密码重置。
// 锁定只是 UI 层防护（各窗口被锁定遮罩覆盖），剪贴板监听/同步后台照常运行。
// 锁定设置随统一配置文件落盘（顶层 "lock" 键）；密码只存加盐迭代哈希，不存明文。

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{now_secs, persist_config, AppState};

// ---------- 配置 ----------

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct LockSettings {
    pub enabled: bool,   // 是否启用锁定（已设置密码或解锁快捷键）
    pub mode: String,    // "password" 密码解锁 | "shortcut" 组合键解锁
    pub password_hash: String, // hex，迭代 SHA-256(salt:password)，不落明文
    pub salt: String,    // hex 随机盐
    pub shortcut: String, // 解锁组合键，如 "Ctrl+Shift+K"（要求 ≥2 个修饰键）
    pub auto_lock_minutes: u32, // 解锁后多少分钟自动锁定，0 = 不自动锁
}

impl Default for LockSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "password".into(),
            password_hash: String::new(),
            salt: String::new(),
            shortcut: String::new(),
            auto_lock_minutes: 0,
        }
    }
}

/// 密码哈希：随机盐 + 10 万轮迭代 SHA-256，拖慢穷举；盐与哈希都以 hex 存储
fn hash_password(salt: &str, password: &str) -> String {
    let mut data = format!("{salt}:{password}").into_bytes();
    for _ in 0..100_000 {
        let mut h = Sha256::new();
        h.update(&data);
        data = h.finalize().to_vec();
    }
    hex::encode(data)
}

fn new_salt() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// 校验组合键格式：至少两个修饰键（Ctrl/Shift/Alt/Win/Cmd）+ 一个普通键
fn valid_shortcut(s: &str) -> bool {
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    if parts.len() < 3 {
        return false; // 至少 2 修饰键 + 1 键
    }
    let mods = parts
        .iter()
        .filter(|p| {
            matches!(
                p.to_lowercase().as_str(),
                "ctrl" | "control" | "shift" | "alt" | "option" | "win" | "cmd" | "command" | "meta" | "super"
            )
        })
        .count();
    mods >= 2 && mods < parts.len()
}

fn verify_credential(settings: &LockSettings, credential: &str) -> bool {
    if settings.mode == "shortcut" {
        !settings.shortcut.is_empty() && credential.trim() == settings.shortcut
    } else {
        !settings.password_hash.is_empty()
            && hash_password(&settings.salt, credential) == settings.password_hash
    }
}

// ---------- 状态 / 事件 ----------

#[derive(Serialize, Clone)]
struct LockStatePayload {
    locked: bool,
    enabled: bool,
    mode: String,
}

/// 前端查询用：不回传密码哈希 / 盐，只回传解锁所需的状态
#[derive(Serialize)]
pub struct LockStatusDto {
    enabled: bool,
    mode: String,
    locked: bool,
    auto_lock_minutes: u32,
    shortcut: String, // 组合键不是强秘密（本地解锁用），回传用于设置页回显
    has_password: bool,
}

fn emit_lock_state(app: &AppHandle) {
    let st = app.state::<AppState>();
    let s = st.lock.lock().unwrap();
    let _ = app.emit(
        "lock-state",
        LockStatePayload {
            locked: st.locked.load(Ordering::SeqCst),
            enabled: s.enabled,
            mode: s.mode.clone(),
        },
    );
}

/// 托盘「立即锁定」菜单项跟随启用状态（未启用锁定时置灰）
fn update_tray_item(app: &AppHandle) {
    let st = app.state::<AppState>();
    let enabled = st.lock.lock().unwrap().enabled;
    if let Some(item) = st.tray_lock.lock().unwrap().as_ref() {
        let _ = item.set_enabled(enabled);
    };
}

/// 锁定/解锁的统一入口：写状态、解锁时重置自动锁定计时、广播事件
pub(crate) fn set_locked(app: &AppHandle, locked: bool) {
    {
        let st = app.state::<AppState>();
        st.locked.store(locked, Ordering::SeqCst);
        if !locked {
            *st.unlock_epoch.lock().unwrap() = now_secs();
        }
    }
    emit_lock_state(app);
}

// ---------- 自动锁定线程 ----------

/// 每 2 秒检查一次：已启用锁定、当前未锁定、配置了自动锁定且超时 → 锁定。
/// 计时起点是上次解锁（或启动）时刻，期间不随操作顺延。
pub fn start(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        let st = app.state::<AppState>();
        if st.locked.load(Ordering::SeqCst) {
            continue;
        }
        let (enabled, mins) = {
            let s = st.lock.lock().unwrap();
            (s.enabled, s.auto_lock_minutes)
        };
        if !enabled || mins == 0 {
            continue;
        }
        let epoch = *st.unlock_epoch.lock().unwrap();
        if now_secs().saturating_sub(epoch) >= mins as i64 * 60 {
            set_locked(&app, true);
        }
    });
}

// ---------- 命令 ----------

#[tauri::command]
pub fn lock_get_status(state: State<AppState>) -> LockStatusDto {
    let s = state.lock.lock().unwrap();
    LockStatusDto {
        enabled: s.enabled,
        mode: s.mode.clone(),
        locked: state.locked.load(Ordering::SeqCst),
        auto_lock_minutes: s.auto_lock_minutes,
        shortcut: s.shortcut.clone(),
        has_password: !s.password_hash.is_empty(),
    }
}

/// 设置 / 修改密码。已有密码时必须先验证旧密码；仅在未锁定状态下允许修改。
#[tauri::command]
pub fn lock_set_password(
    app: AppHandle,
    state: State<AppState>,
    old: Option<String>,
    new: String,
) -> Result<(), String> {
    if state.locked.load(Ordering::SeqCst) {
        return Err("已锁定，请先解锁再修改".into());
    }
    if new.chars().count() < 4 {
        return Err("密码至少 4 位".into());
    }
    {
        let mut s = state.lock.lock().unwrap();
        if !s.password_hash.is_empty() {
            let old = old.ok_or("修改密码需要提供当前密码")?;
            if hash_password(&s.salt, &old) != s.password_hash {
                return Err("当前密码不正确".into());
            }
        }
        s.salt = new_salt();
        s.password_hash = hash_password(&s.salt, &new);
        s.enabled = true;
        s.mode = "password".into();
    }
    persist_config(&state)?;
    set_locked(&app, false); // 重置自动锁定计时
    update_tray_item(&app);
    Ok(())
}

/// 设置 / 修改解锁组合键；已有密码时切换到快捷键模式需先验证当前密码。
#[tauri::command]
pub fn lock_set_shortcut(
    app: AppHandle,
    state: State<AppState>,
    old: Option<String>,
    shortcut: String,
) -> Result<(), String> {
    if state.locked.load(Ordering::SeqCst) {
        return Err("已锁定，请先解锁再修改".into());
    }
    let shortcut = shortcut.trim().to_string();
    if !valid_shortcut(&shortcut) {
        return Err("解锁组合键需包含至少两个修饰键（Ctrl/Shift/Alt/Win）加一个普通键".into());
    }
    {
        let mut s = state.lock.lock().unwrap();
        // 从密码模式切走：要求验证当前密码（防止他人在未锁定时顺手改掉解锁方式）
        if s.enabled && s.mode == "password" && !s.password_hash.is_empty() {
            let old = old.ok_or("切换到快捷键解锁需要验证当前密码")?;
            if hash_password(&s.salt, &old) != s.password_hash {
                return Err("当前密码不正确".into());
            }
            s.password_hash.clear();
            s.salt.clear();
        }
        s.shortcut = shortcut;
        s.enabled = true;
        s.mode = "shortcut".into();
    }
    persist_config(&state)?;
    set_locked(&app, false);
    update_tray_item(&app);
    Ok(())
}

/// 解锁：按当前模式校验密码或组合键，成功返回 true
#[tauri::command]
pub fn lock_unlock(app: AppHandle, state: State<AppState>, credential: String) -> bool {
    let s = state.lock.lock().unwrap();
    if !s.enabled {
        return true;
    }
    if !state.locked.load(Ordering::SeqCst) {
        return true;
    }
    let ok = verify_credential(&s, &credential);
    drop(s);
    if ok {
        set_locked(&app, false);
    }
    ok
}

/// 手动立即锁定（未启用锁定时为空操作）
#[tauri::command]
pub fn lock_now(app: AppHandle, state: State<AppState>) {
    if state.lock.lock().unwrap().enabled {
        set_locked(&app, true);
    }
}

/// 设置自动锁定分钟数（0 = 关闭自动锁定），即时生效
#[tauri::command]
pub fn lock_set_auto_lock(
    app: AppHandle,
    state: State<AppState>,
    minutes: u32,
) -> Result<(), String> {
    let minutes = minutes.min(24 * 60);
    state.lock.lock().unwrap().auto_lock_minutes = minutes;
    persist_config(&state)?;
    emit_lock_state(&app);
    Ok(())
}

/// 关闭锁定：需验证当前密码 / 组合键（设置页「关闭」入口）
#[tauri::command]
pub fn lock_disable(
    app: AppHandle,
    state: State<AppState>,
    credential: String,
) -> Result<(), String> {
    {
        let s = state.lock.lock().unwrap();
        if !s.enabled {
            return Ok(());
        }
        if !verify_credential(&s, &credential) {
            return Err(if s.mode == "shortcut" {
                "组合键不正确".into()
            } else {
                "密码不正确".into()
            });
        }
    }
    *state.lock.lock().unwrap() = LockSettings::default();
    persist_config(&state)?;
    set_locked(&app, false);
    update_tray_item(&app);
    Ok(())
}

/// 忘记密码 / 组合键时的重置：直接清空全部锁定设置（无需验证，剪贴板记录不受影响）。
/// 本应用为本地单机工具，数据本身未加密，重置只解除界面锁定。
#[tauri::command]
pub fn lock_reset(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    *state.lock.lock().unwrap() = LockSettings::default();
    persist_config(&state)?;
    set_locked(&app, false);
    update_tray_item(&app);
    Ok(())
}
