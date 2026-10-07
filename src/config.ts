import { invoke } from "@tauri-apps/api/core";

/** 背景图选区：归一化坐标（0-1），相对于旋转后的源图 */
export interface BgRegion {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** 面板背景图设置（与后端 BackgroundConfig 对应） */
export interface BackgroundConfig {
  enabled: boolean; // 是否启用面板背景图
  mode: string; // "stretch" 拉伸铺满 | "tile" 原尺寸平铺
  opacity: number; // 背景图不透明度 0.05 - 1.0
  scale_w: number; // 图片宽度占面板百分比 5 - 300（100 = 与面板同宽）
  scale_h: number; // 图片高度占面板百分比 5 - 300（100 = 与面板同高）
  apply_settings: boolean; // 背景同时应用到设置窗口
  apply_transfer: boolean; // 背景同时应用到互传文件窗口
  rotation: number; // 旋转角度（0/90/180/270，已烘焙进缓存图）
  region: BgRegion | null; // 归一化选区（已烘焙进缓存图），null = 整图
  has_image: boolean; // 是否已选择图片
}

export interface AppConfig {
  hotkey: string;
  autostart: boolean;
  silent_start: boolean;
  db_dir: string | null;
  config_dir: string | null; // 配置文件目录；null = 程序所在目录
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
  follow_cursor_monitor: boolean; // 多显示器：唤起时面板跟随光标所在屏幕
  window_effect: string; // 主面板窗口材质："default" | "acrylic" | "vibrancy" | "mica"
  background: BackgroundConfig; // 面板背景图设置
}

export async function loadConfig(): Promise<AppConfig> {
  // 启动早期页面加载可能早于 Rust 端 .manage() 完成（Windows 上复现过
  // get_config 报 "state not managed"），一旦失败整个初始化中断，
  // 表现为字号/热键等不回填、字体下拉列表为空。这里短间隔重试等后端就绪。
  let lastErr: unknown;
  for (let i = 0; i < 20; i++) {
    try {
      return await invoke<AppConfig>("get_config");
    } catch (e) {
      lastErr = e;
      await new Promise((r) => setTimeout(r, 250));
    }
  }
  throw lastErr;
}

/**
 * 把背景图按设置应用到一个背景层元素上（主面板与设置页预览共用）。
 * b64 为烘焙后的 PNG base64；mode 决定拉伸（不重复）还是平铺（重复），
 * scale_w / scale_h 为图片占面板的宽高百分比，opacity 控制不透明度。
 */
export function styleBgLayer(el: HTMLElement, bg: BackgroundConfig, b64: string) {
  el.style.backgroundImage = `url("data:image/png;base64,${b64}")`;
  el.style.opacity = String(Math.max(0.05, Math.min(1, bg.opacity)));
  const w = Math.max(5, Math.min(300, bg.scale_w || 100));
  const h = Math.max(5, Math.min(300, bg.scale_h || 100));
  el.style.backgroundSize = `${w}% ${h}%`;
  el.style.backgroundPosition = "center";
  el.style.backgroundRepeat = bg.mode === "tile" ? "repeat" : "no-repeat";
}

/** 背景设置的默认值（与后端 BackgroundConfig::default 一致） */
export function defaultBackground(): BackgroundConfig {
  return {
    enabled: false,
    mode: "stretch",
    opacity: 0.6,
    scale_w: 100,
    scale_h: 100,
    apply_settings: false,
    apply_transfer: false,
    rotation: 0,
    region: null,
    has_image: false,
  };
}

// 当前窗口的背景层（主面板/设置/互传文件共用，懒创建）
let bgLayerEl: HTMLElement | null = null;

/**
 * 把背景图应用到当前窗口。
 * include：该窗口是否在背景应用范围内——主面板恒 true；
 * 设置/互传文件窗口分别传 bg.apply_settings / bg.apply_transfer。
 * b64Override：设置页编辑草稿时用未落盘的烘焙图实时预览，不传则读取已保存的缓存图。
 */
export async function applyWindowBackground(
  bg: BackgroundConfig | undefined,
  include: boolean,
  b64Override?: string
) {
  const on = !!bg && bg.enabled && bg.has_image && include;
  document.documentElement.dataset.bg = on ? "on" : "off";
  if (!bgLayerEl) {
    bgLayerEl = document.createElement("div");
    bgLayerEl.className = "bg-layer";
    document.body.prepend(bgLayerEl);
  }
  if (!on) {
    bgLayerEl.style.backgroundImage = "none";
    return;
  }
  const b64 = b64Override ?? (await invoke<string | null>("get_background_cache"));
  if (!b64) {
    // 缓存图缺失（如配置目录换过但图片没跟过来）：静默退回无背景
    document.documentElement.dataset.bg = "off";
    bgLayerEl.style.backgroundImage = "none";
    return;
  }
  styleBgLayer(bgLayerEl, bg!, b64);
}

/** 是否 macOS（用于快捷键的显示与默认值） */
export const isMac = /mac/i.test(navigator.platform || navigator.userAgent);

/** 默认全局快捷键：全平台统一 Ctrl+`（mac 上即 Control+`） */
export const DEFAULT_HOTKEY = "Ctrl+`";

/**
 * 把存储格式的快捷键（如 "Ctrl+Shift+V" / "Cmd+`"）转成当前平台的显示形式。
 * mac 上显示为符号形式：⌃ Control、⇧ Shift、⌥ Option、⌘ Command，键名间不加 "+"；
 * Windows/Linux 原样返回。
 */
export function formatHotkey(hk: string): string {
  if (!isMac) return hk;
  return hk
    .split("+")
    .map((p) => {
      switch (p.trim().toLowerCase()) {
        case "ctrl":
        case "control":
          return "⌃";
        case "shift":
          return "⇧";
        case "alt":
        case "option":
          return "⌥";
        case "win":
        case "cmd":
        case "command":
        case "super":
        case "meta":
          return "⌘";
        default:
          return p.trim();
      }
    })
    .join("");
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

/**
 * 窗口材质效果（亚克力/云母/苹果毛玻璃）：把效果名写到 <html data-effect>，
 * 面板背景由 styles.css 里对应的半透明规则接管，透出系统材质。
 * 只在主面板窗口调用——其余窗口不是透明窗口，不需要半透明背景。
 */
export function applyWindowEffect(effect: string) {
  document.documentElement.dataset.effect = effect || "default";
}

/**
 * 标签栏右侧渐变遮罩：标签过多溢出可横向滚动时，右缘淡出提示还有更多。
 * 要求 nav 外层包一个 .tabs-wrap 容器；滚动 / 尺寸变化（含字号调整）时自动更新。
 */
export function watchTabsFade(nav: HTMLElement) {
  const wrap = nav.parentElement;
  if (!wrap || !wrap.classList.contains("tabs-wrap")) return;
  const update = () => {
    const more = nav.scrollWidth - nav.clientWidth - nav.scrollLeft > 4;
    wrap.classList.toggle("scrollable", more);
  };
  new ResizeObserver(update).observe(nav);
  nav.addEventListener("scroll", update);
  update();
}
