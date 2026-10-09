import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import { applyAppearance, applyWindowEffect, applyWindowBackground, AppConfig, DEFAULT_HOTKEY, formatHotkey, isMac, loadConfig, watchTabsFade } from "./config";
import { alertDialog, confirmDialog } from "./confirm";
import { icons } from "./icons";
import { autoCheckEnabled, checkUpdate } from "./updater";

// macOS 无边框窗口没有系统圆角（Windows 由 DWM 自动圆角）：给 <html> 加 .mac，
// styles.css 据此把面板裁成圆角（与后端 Vibrancy 的 10px 一致）
if (isMac) document.documentElement.classList.add("mac");

interface Clip {
  id: number;
  kind: string; // "text" | "image" | "file"
  category: string; // 后端分类："text" | "image" | "video" | "office" | "file"
  preview: string;
  image_b64: string | null;
  url: string | null;
  pinned: boolean;
  created_at: number; // 秒
}

const listEl = document.querySelector<HTMLDivElement>("#list")!;
const emptyEl = document.querySelector<HTMLDivElement>("#empty")!;
const searchEl = document.querySelector<HTMLInputElement>("#search")!;
const hintEl = document.querySelector<HTMLDivElement>("#hint")!;
const tabsEl = document.querySelector<HTMLElement>("#tabs")!;
// 标签过多溢出时，右侧渐变遮罩提示可横向滚动
watchTabsFade(tabsEl);

let keyword = "";
let searchTimer: number | undefined;
let clips: Clip[] = [];
let selected = 0;
let config: AppConfig | null = null;

// 主面板始终在背景应用范围内（设置/互传文件窗口各有开关，见 config.ts）
const applyBackground = (bg: AppConfig["background"] | undefined) =>
  applyWindowBackground(bg, true);
// 上次渲染的内容签名（id + 置顶态）：唤起面板时若内容没变，直接渲染不播入场动画，避免闪一下
let lastRenderSig = "";
// 面板唤起时置位：下一次 refresh 且内容有变化才播交错入场
let animateOnNextRender = false;

// ---------- 类型标签页：全部 / 图片 / 视频 / 文字 / 办公 / 其他 ----------
// 分类由后端 category 字段给出："text" 纯文本 | "image" 图片 | "video" 视频 | "office" 办公/文本文件 | "file" 其他文件
type TabKey = "all" | "image" | "video" | "text" | "office" | "other";
let activeTab: TabKey = "all";

function matchTab(c: Clip): boolean {
  switch (activeTab) {
    case "image": return c.category === "image";
    case "text": return c.category === "text";
    case "office": return c.category === "office";
    case "video": return c.category === "video";
    case "other": return c.category === "file";
    default: return true;
  }
}

tabsEl.querySelectorAll<HTMLButtonElement>(".tab").forEach((btn) => {
  btn.onclick = () => {
    if (btn.dataset.tab === activeTab) return;
    activeTab = (btn.dataset.tab as TabKey) || "all";
    tabsEl.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
    btn.classList.add("active");
    refresh();
  };
});

// ---------- 标签页数量徽标：显示当前搜索条件下各类型的记录数 ----------
function updateTabCounts(all: Clip[]) {
  const counts: Record<TabKey, number> = {
    all: all.length,
    image: 0,
    video: 0,
    text: 0,
    office: 0,
    other: 0,
  };
  for (const c of all) {
    if (c.category === "image") counts.image++;
    else if (c.category === "video") counts.video++;
    else if (c.category === "text") counts.text++;
    else if (c.category === "office") counts.office++;
    else counts.other++;
  }
  tabsEl.querySelectorAll<HTMLButtonElement>(".tab").forEach((btn) => {
    const key = (btn.dataset.tab as TabKey) || "all";
    const n = counts[key] ?? 0;
    let badge = btn.querySelector<HTMLSpanElement>(".tab-count");
    if (!badge) {
      badge = document.createElement("span");
      badge.className = "tab-count";
      btn.appendChild(badge);
    }
    badge.textContent = n > 0 ? String(n) : "";
  });
}

// ---------- 图片缩略图懒加载：进入可视区域才取回数据，带缓存 ----------
const imageCache = new Map<number, string>();

async function loadThumb(el: HTMLElement, id: number) {
  let b64 = imageCache.get(id);
  if (!b64) {
    b64 = (await invoke<string | null>("get_clip_image", { id })) ?? undefined;
    if (b64) {
      if (imageCache.size > 50) imageCache.clear(); // 简单上限，防止无限增长
      imageCache.set(id, b64);
    }
  }
  if (!b64 || !el.isConnected) return;
  const img = document.createElement("img");
  img.src = `data:image/png;base64,${b64}`;
  img.className = "clip-thumb";
  el.replaceWith(img);
}

const imgObserver = new IntersectionObserver(
  (entries) => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      imgObserver.unobserve(e.target);
      const el = e.target as HTMLElement;
      loadThumb(el, Number(el.dataset.imgId));
    }
  },
  { root: listEl, rootMargin: "200px" } // 提前 200px 预加载
);

function fmtTime(ts: number): string {
  const d = new Date(ts * 1000);
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  if (sameDay) return `${hh}:${mm}`;
  return `${d.getMonth() + 1}/${d.getDate()} ${hh}:${mm}`;
}

function updateHint() {
  const hk = formatHotkey(config?.hotkey || DEFAULT_HOTKEY);
  hintEl.textContent = panelPinned
    ? `📌 已钉住 · Enter 粘贴不隐藏 · Esc 清空/关闭 · ${hk} 呼出/隐藏`
    : `↑↓ 选择 · Enter 粘贴 · Esc 清空/关闭 · 右键仅复制 · ${hk} 呼出/隐藏`;
}

// 空列表提示：无搜索词时引导复制（快捷键按平台显示，mac 为 ⌘C），有搜索词时提示无匹配
function updateEmpty() {
  const kw = keyword.trim();
  emptyEl.classList.toggle("search-empty", !!kw);
  emptyEl.textContent = kw
    ? `没有找到包含「${kw}」的记录`
    : `暂无记录，复制点什么试试 (${isMac ? "⌘C" : "Ctrl+C"})`;
}

// ---------- 面板钉住：钉住后失焦/粘贴都不自动隐藏 ----------
const pinBtn = document.querySelector<HTMLButtonElement>("#btn-pin")!;
let panelPinned = false;

function applyPinState() {
  pinBtn.classList.toggle("active", panelPinned);
  pinBtn.title = panelPinned
    ? "取消钉住（恢复失焦自动隐藏）"
    : "钉在桌面上（失焦不自动隐藏）";
  updateHint();
}

pinBtn.onclick = async () => {
  panelPinned = !panelPinned;
  await invoke("set_panel_pinned", { pinned: panelPinned });
  applyPinState();
};

// ---------- 无边框窗口拖动：工具栏/底栏空白处按住左键拖动 ----------
function enableDrag(el: HTMLElement) {
  el.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    const t = e.target as HTMLElement;
    // 输入框、按钮、开关等交互元素上不触发拖动
    if (t.closest("input, button, label, select, textarea, a")) return;
    e.preventDefault();
    // 走后端命令：拖动期间置 dragging 标记，防止瞬时失焦把面板隐藏
    invoke("start_drag");
  });
}
enableDrag(document.querySelector<HTMLElement>(".toolbar")!);
enableDrag(document.querySelector<HTMLElement>(".footer")!);

// ---------- 窗口缩放：拖右缘/下缘/右下角调整长宽 ----------
document.querySelectorAll<HTMLElement>(".resize-handle").forEach((el) => {
  el.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const dir = (el.dataset.dir || "SouthEast") as
      | "East"
      | "South"
      | "SouthEast";
    getCurrentWindow().startResizeDragging(dir);
  });
});

function applySelection() {
  const items = listEl.querySelectorAll<HTMLDivElement>(".clip-item");
  items.forEach((el, i) => {
    el.classList.toggle("selected", i === selected);
    if (i === selected) el.scrollIntoView({ block: "nearest" });
  });
}

async function refresh(keepSelection = false) {
  const all = await invoke<Clip[]>("list_clips", {
    keyword: keyword.trim() || null,
  });
  clips = all.filter(matchTab);
  updateTabCounts(all);
  emptyEl.style.display = clips.length ? "none" : "block";
  updateEmpty();
  listEl.innerHTML = "";
  // 内容没变不播动画；有变化才在本次渲染播交错入场
  const sig = clips.map((c) => `${c.id}:${c.pinned ? 1 : 0}`).join(",");
  const animate = animateOnNextRender && sig !== lastRenderSig && clips.length > 0;
  animateOnNextRender = false;
  lastRenderSig = sig;
  if (!keepSelection) selected = 0;
  if (selected >= clips.length) selected = Math.max(0, clips.length - 1);

  clips.forEach((c, idx) => {
    const item = document.createElement("div");
    item.className = "clip-item" + (c.pinned ? " pinned" : "");

    const body = document.createElement("div");
    body.className = "clip-body";
    if (c.kind === "image") {
      // 图片懒加载占位，滚动到可视区域时再取回数据
      const ph = document.createElement("div");
      ph.className = "clip-thumb-placeholder";
      ph.textContent = c.preview;
      ph.dataset.imgId = String(c.id);
      body.appendChild(ph);
      imgObserver.observe(ph);
    } else {
      const p = document.createElement("div");
      p.className = "clip-text";
      p.textContent = c.preview;
      body.appendChild(p);
    }

    const meta = document.createElement("div");
    meta.className = "clip-meta";

    const left = document.createElement("span");
    left.className = "clip-meta-left";
    const time = document.createElement("span");
    time.textContent = fmtTime(c.created_at);
    left.appendChild(time);
    if (c.pinned) {
      const tag = document.createElement("span");
      tag.className = "pin-tag";
      tag.textContent = "置顶";
      left.appendChild(tag);
    }

    const actions = document.createElement("span");
    actions.className = "clip-actions";

    // 内容含网址时显示浏览器按钮，点击打开第一个网址
    if (c.url) {
      const web = document.createElement("button");
      web.className = "clip-web";
      web.innerHTML = icons.globe;
      web.title = `用默认浏览器打开: ${c.url}`;
      web.onclick = async (e) => {
        e.stopPropagation();
        try {
          await openUrl(c.url!);
        } catch (err) {
          alertDialog(`打开网址失败: ${err}`);
        }
      };
      actions.appendChild(web);
    }

    // 文件条目：点击/回车 = 粘贴文件（文件本身进剪贴板，聊天发送框发文件、
    // 资源管理器/访达复制进文件夹；目标不支持文件时双格式自动落到路径文本）。
    // 额外的「粘贴地址」按钮：纯文本完整路径进输入框
    if (c.kind === "file") {
      const pa = document.createElement("button");
      pa.className = "clip-filepaste";
      pa.innerHTML = icons.link;
      pa.title = "粘贴地址（完整路径文本）";
      pa.onclick = async (e) => {
        e.stopPropagation();
        try {
          await invoke("paste_clip", { id: c.id });
        } catch (err) {
          alertDialog(`粘贴地址失败: ${err}`);
        }
      };
      actions.appendChild(pa);
      item.title = "点击粘贴文件（不支持时自动粘贴地址）";
    }

    const view = document.createElement("button");
    view.className = "clip-view";
    view.innerHTML = icons.eye;
    view.title =
      c.kind === "image"
        ? "用看图软件打开"
        : c.kind === "file"
          ? "用系统默认应用打开"
          : "用记事本打开全文";
    view.onclick = async (e) => {
      e.stopPropagation();
      try {
        await invoke("open_clip_with_system", { id: c.id });
      } catch (err) {
        alertDialog(`打开失败: ${err}`);
      }
    };

    const pin = document.createElement("button");
    pin.className = "clip-pin" + (c.pinned ? " active" : "");
    pin.innerHTML = icons.pin;
    pin.title = c.pinned ? "取消置顶" : "置顶（排到最前）";
    pin.onclick = async (e) => {
      e.stopPropagation();
      await invoke("toggle_pin", { id: c.id });
      refresh(true);
    };

    const del = document.createElement("button");
    del.className = "clip-del";
    del.innerHTML = icons.x;
    del.title = "删除此条";
    del.onclick = async (e) => {
      e.stopPropagation();
      // 置顶内容删除前确认
      if (c.pinned && !(await confirmDialog("该记录已置顶，确定一并删除吗？"))) return;
      await invoke("delete_clip", { id: c.id });
      refresh(true);
    };

    actions.appendChild(view);
    actions.appendChild(pin);
    actions.appendChild(del);
    meta.appendChild(left);
    meta.appendChild(actions);

    item.appendChild(body);
    item.appendChild(meta);

    // 点击粘贴：文件条目默认「粘贴文件」，其余类型粘贴文本/图片
    item.onclick = () => {
      if (c.kind === "file") {
        invoke("paste_clip_file", { id: c.id }).catch((err) =>
          alertDialog(`粘贴文件失败: ${err}`)
        );
      } else {
        invoke("paste_clip", { id: c.id });
      }
    };
    item.oncontextmenu = (e) => {
      e.preventDefault();
      invoke("copy_clip", { id: c.id });
    };
    item.onmousemove = () => {
      if (selected !== idx) {
        selected = idx;
        applySelection();
      }
    };

    listEl.appendChild(item);
  });

  // 交错入场动画只作用于本次渲染，播完即移除，不影响后续增量刷新
  if (animate) {
    listEl.classList.add("animate-in");
    window.setTimeout(() => listEl.classList.remove("animate-in"), 600);
  }

  applySelection();
}

// ---------- 键盘导航：↑↓ 选择，Enter 粘贴，Esc 关闭 ----------
document.addEventListener("keydown", async (e) => {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    if (!clips.length) return;
    selected =
      e.key === "ArrowDown"
        ? Math.min(selected + 1, clips.length - 1)
        : Math.max(selected - 1, 0);
    applySelection();
  } else if (e.key === "Enter") {
    e.preventDefault();
    const c = clips[selected];
    if (!c) return;
    // 与点击一致：文件条目回车 = 粘贴文件
    if (c.kind === "file") {
      try {
        await invoke("paste_clip_file", { id: c.id });
      } catch (err) {
        alertDialog(`粘贴文件失败: ${err}`);
      }
    } else {
      await invoke("paste_clip", { id: c.id });
    }
  } else if (e.key === "Escape") {
    e.preventDefault();
    // 有搜索词时第一次 Esc 只清空搜索，再按才关闭面板
    if (searchEl.value) {
      searchEl.value = "";
      keyword = "";
      refresh();
      return;
    }
    // 走后端隐藏：mac 上需要顺带把焦点还给之前的 App（NSApplication.hide）
    await invoke("hide_panel");
  }
});

searchEl.addEventListener("input", () => {
  keyword = searchEl.value;
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(() => refresh(), 200);
});

document.querySelector<HTMLButtonElement>("#btn-settings")!.onclick = () =>
  invoke("open_settings");

// 记录开关：三处（托盘/弹窗/设置页）通过 config-changed 事件保持同步
const toggleEnabledEl = document.querySelector<HTMLInputElement>("#toggle-enabled")!;
toggleEnabledEl.addEventListener("change", () => {
  invoke("set_enabled", { enabled: toggleEnabledEl.checked });
});

// 后端新增记录 / 面板显示时自动刷新
listen("clip-added", () => refresh(true));
listen("panel-shown", () => {
  searchEl.value = "";
  keyword = "";
  // 标记下一次刷新可播入场动画；refresh 里会比对内容签名，没变则不播
  animateOnNextRender = true;
  refresh();
  searchEl.focus();
});
listen<AppConfig>("config-changed", (e) => {
  config = e.payload;
  applyAppearance(config);
  applyWindowEffect(config.window_effect);
  applyBackground(config.background);
  updateHint();
  toggleEnabledEl.checked = config.enabled;
});

// ---------- 更新提示：启动时静默检查，有新版则在面板顶部显示横幅 ----------
const updateBannerEl = document.querySelector<HTMLDivElement>("#update-banner")!;
const updateBannerTextEl = document.querySelector<HTMLSpanElement>("#update-banner-text")!;
document.querySelector<HTMLButtonElement>("#update-banner-btn")!.onclick = async () => {
  // 打开设置并切到「关于」页（设置窗口随应用启动已加载，直接发事件即可）
  await invoke("open_settings");
  await emit("open-update-tab");
};

(async () => {
  config = await loadConfig();
  applyAppearance(config);
  applyWindowEffect(config.window_effect);
  applyBackground(config.background);
  panelPinned = await invoke<boolean>("get_panel_pinned");
  applyPinState();
  toggleEnabledEl.checked = config.enabled;
  refresh();

  if (autoCheckEnabled()) {
    checkUpdate().then((info) => {
      if (info) {
        updateBannerTextEl.textContent = `发现新版本 v${info.version}`;
        updateBannerEl.hidden = false;
      }
    });
  }
})();
