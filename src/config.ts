import { invoke } from "@tauri-apps/api/core";

export interface AppConfig {
  hotkey: string;
  autostart: boolean;
  silent_start: boolean;
  db_dir: string | null;
  theme: string; // "dark" | "light"
  font_family: string;
  font_size: number;
  exclude_apps: string[];
  max_items: number;
  retention_value: number; // 数据保留时长数值，0 = 永久保留
  retention_unit: string; // "hours" | "days" | "months" | "years"
  enabled: boolean; // 是否开启剪贴板记录
  remember_size: boolean; // 记住窗口大小
  window_width: number; // 记住的窗口宽度（物理像素）
  window_height: number; // 记住的窗口高度（物理像素）
}

export async function loadConfig(): Promise<AppConfig> {
  return await invoke<AppConfig>("get_config");
}

export function applyAppearance(cfg: AppConfig) {
  const root = document.documentElement;
  root.dataset.theme = cfg.theme === "light" ? "light" : "dark";
  root.style.setProperty(
    "--app-font",
    cfg.font_family?.trim() || "Segoe UI, Microsoft YaHei, system-ui, sans-serif"
  );
  root.style.setProperty("--app-font-size", `${cfg.font_size || 14}px`);
}
