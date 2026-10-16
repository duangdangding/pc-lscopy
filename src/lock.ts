// 全局安全锁定：共享锁定遮罩组件（主面板 / 设置 / 黑名单 / 互传文件四个窗口共用）。
// 锁定时遮罩盖住整个窗口，按当前模式用密码或组合键解锁；
// 「忘记密码 / 快捷键？」可重置清空锁定设置（剪贴板记录不受影响）。

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { confirmDialog } from "./confirm";
import { formatHotkey, isMac } from "./config";

export interface LockStatus {
  enabled: boolean;
  mode: string; // "password" | "shortcut"
  locked: boolean;
  auto_lock_minutes: number;
  shortcut: string;
  has_password: boolean;
}

// 与 settings.ts 的快捷键录制同一套键名规范（组合键比对依赖一致的字符串格式）
function keyName(e: KeyboardEvent): string | null {
  const k = e.key;
  if (["Control", "Shift", "Alt", "Meta"].includes(k)) return null;
  if (k === " ") return "Space";
  if (k === "`" || k === "~") return "`";
  if (/^arrow/i.test(k)) return k.replace(/^Arrow/, "");
  if (k === "Escape") return "Esc";
  if (k.length === 1) return k.toUpperCase();
  return k;
}

/** 从键盘事件组装组合键字符串（如 "Ctrl+Shift+K"）；只按了修饰键时返回 null */
export function comboFromEvent(e: KeyboardEvent): string | null {
  const key = keyName(e);
  if (!key) return null;
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.shiftKey) parts.push("Shift");
  if (e.altKey) parts.push("Alt");
  if (e.metaKey) parts.push(isMac ? "Cmd" : "Win");
  parts.push(key);
  return parts.join("+");
}

let overlay: HTMLDivElement | null = null;
let pwInput: HTMLInputElement | null = null;
let scInput: HTMLInputElement | null = null;
let errEl: HTMLDivElement | null = null;
let onUnlockCb: (() => void) | null = null;
let currentStatus: LockStatus | null = null;

function fail(msg: string) {
  if (!errEl) return;
  errEl.textContent = msg;
  if (pwInput) {
    pwInput.value = "";
    pwInput.focus();
  }
}

async function tryUnlock(credential: string) {
  let ok = false;
  try {
    ok = await invoke<boolean>("lock_unlock", { credential });
  } catch (e) {
    fail(`解锁失败：${e}`);
    return;
  }
  if (ok) {
    applyLocked(false);
    onUnlockCb?.();
  } else {
    fail(currentStatus?.mode === "shortcut" ? "组合键不正确，请重试" : "密码不正确，请重试");
  }
}

// 忘记密码 / 快捷键：确认后重置清空锁定设置（本应用为本地工具，重置只解除界面锁定）
async function forgotReset() {
  const what = currentStatus?.mode === "shortcut" ? "快捷键" : "密码";
  const ok = await confirmDialog(
    `忘记${what}时可以重置：将清空全部锁定设置（${what} / 自动锁定），剪贴板记录不受影响。确定重置？`,
    { okText: "重置并解锁", okKind: "danger" }
  );
  if (!ok) return;
  try {
    await invoke("lock_reset");
  } catch (e) {
    fail(`重置失败：${e}`);
  }
  // 后端会广播 lock-state，applyLocked 随事件更新
}

function buildOverlay() {
  overlay = document.createElement("div");
  overlay.className = "lock-overlay";
  overlay.hidden = true;

  const box = document.createElement("div");
  box.className = "lock-box";

  const icon = document.createElement("div");
  icon.className = "lock-icon";
  icon.textContent = "🔒";

  const title = document.createElement("div");
  title.className = "lock-title";
  title.textContent = "已锁定";

  const desc = document.createElement("div");
  desc.className = "lock-desc";

  // 密码模式：密码输入框 + 解锁按钮
  pwInput = document.createElement("input");
  pwInput.type = "password";
  pwInput.className = "lock-input";
  pwInput.placeholder = "输入密码解锁";
  pwInput.autocomplete = "off";
  pwInput.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter") {
      e.preventDefault();
      if (pwInput!.value) tryUnlock(pwInput!.value);
    }
  });

  const unlockBtn = document.createElement("button");
  unlockBtn.className = "btn primary lock-unlock-btn";
  unlockBtn.textContent = "解锁";
  unlockBtn.onclick = () => {
    if (pwInput!.value) tryUnlock(pwInput!.value);
  };

  // 快捷键模式：只读输入框捕获组合键，按完整即自动尝试解锁
  scInput = document.createElement("input");
  scInput.type = "text";
  scInput.className = "lock-input";
  scInput.readOnly = true;
  scInput.placeholder = "按下解锁组合键";
  scInput.addEventListener("keydown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const combo = comboFromEvent(e);
    if (!combo) return; // 只按了修饰键，继续等
    scInput!.value = formatHotkey(combo);
    tryUnlock(combo);
  });

  errEl = document.createElement("div");
  errEl.className = "lock-error";

  const forgot = document.createElement("button");
  forgot.className = "lock-forgot";
  forgot.textContent = "忘记密码 / 快捷键？";
  forgot.onclick = forgotReset;

  box.append(icon, title, desc, pwInput, scInput, unlockBtn, errEl, forgot);
  overlay.appendChild(box);
  document.body.appendChild(overlay);

  // 遮罩上拦截所有键盘事件：锁定时不触发页面自身的快捷键（如主面板 Esc 关闭）
  overlay.addEventListener("keydown", (e) => e.stopPropagation());

  // 按当前模式切换显示密码区 / 组合键区
  const syncMode = () => {
    const isShortcut = currentStatus?.mode === "shortcut";
    pwInput!.hidden = isShortcut;
    unlockBtn.hidden = isShortcut;
    scInput!.hidden = !isShortcut;
    desc.textContent = isShortcut
      ? "按下设置的解锁组合键继续使用"
      : "输入密码后按回车或点击「解锁」";
  };
  syncMode();
  (overlay as any)._syncMode = syncMode;
}

function applyLocked(locked: boolean) {
  if (!overlay) return;
  if (!locked || !currentStatus?.enabled) {
    overlay.hidden = true;
    return;
  }
  (overlay as any)._syncMode?.();
  if (errEl) errEl.textContent = "";
  if (pwInput) pwInput.value = "";
  if (scInput) scInput.value = "";
  overlay.hidden = false;
  window.setTimeout(() => {
    (currentStatus?.mode === "shortcut" ? scInput : pwInput)?.focus();
  }, 30);
}

async function refreshStatus() {
  try {
    currentStatus = await invoke<LockStatus>("lock_get_status");
  } catch {
    return; // 后端未就绪（启动早期），下次事件再试
  }
  applyLocked(currentStatus.locked);
}

/**
 * 初始化当前窗口的锁定守卫：
 * - 启动时若已锁定立即显示遮罩；
 * - 监听后端 lock-state 广播（锁定 / 解锁 / 重置 / 设置变更）；
 * - 主面板每次唤起（panel-shown）重新核对一次状态，防事件丢失。
 * onUnlock：解锁成功后的回调（如主面板重新聚焦搜索框）。
 */
export async function initLockGuard(onUnlock?: () => void) {
  onUnlockCb = onUnlock ?? null;
  if (!overlay) buildOverlay();
  await refreshStatus();
  listen("lock-state", () => refreshStatus());
  listen("panel-shown", () => refreshStatus());
}

/** 设置页用：查询最新锁定状态 */
export async function getLockStatus(): Promise<LockStatus> {
  currentStatus = await invoke<LockStatus>("lock_get_status");
  return currentStatus;
}
