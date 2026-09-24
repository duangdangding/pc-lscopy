import { check, Update } from "@tauri-apps/plugin-updater";

// ---------- 应用内更新：主面板与设置页共用的检查逻辑 ----------
// 结果缓存一次：主面板自动检查过后，设置页直接复用，避免重复请求 GitHub
export interface UpdateInfo {
  version: string;
  notes: string;
  update: Update;
}

let cached: UpdateInfo | null | undefined; // undefined = 还没检查过

const AUTO_CHECK_KEY = "lscopy:auto-check-update";

export function autoCheckEnabled(): boolean {
  return localStorage.getItem(AUTO_CHECK_KEY) !== "0";
}

export function setAutoCheckEnabled(on: boolean) {
  localStorage.setItem(AUTO_CHECK_KEY, on ? "1" : "0");
}

export async function checkUpdate(): Promise<UpdateInfo | null> {
  if (cached !== undefined) return cached;
  try {
    const update = await check();
    cached = update
      ? { version: update.version, notes: update.body ?? "", update }
      : null;
  } catch {
    // 断网、无 Release、未配置签名等场景一律静默按「无更新」处理
    cached = null;
  }
  return cached;
}

// 手动点击「检查更新」时清掉缓存强制重新查
export function resetUpdateCache() {
  cached = undefined;
}
