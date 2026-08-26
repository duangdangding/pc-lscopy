import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import { applyAppearance, AppConfig, loadConfig } from "./config";
import { confirmDialog } from "./confirm";

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

let keyword = "";
let searchTimer: number | undefined;
let clips: Clip[] = [];
let selected = 0;
let config: AppConfig | null = null;

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
  const hk = config?.hotkey || "Ctrl+`";
  hintEl.textContent = `↑↓ 选择 · Enter 粘贴 · Esc 关闭 · 右键仅复制 · ${hk} 呼出/隐藏`;
}

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
  emptyEl.style.display = clips.length ? "none" : "block";
  listEl.innerHTML = "";
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
      web.textContent = "🌐";
      web.title = `用默认浏览器打开: ${c.url}`;
      web.onclick = async (e) => {
        e.stopPropagation();
        try {
          await openUrl(c.url!);
        } catch (err) {
          alert(`打开网址失败: ${err}`);
        }
      };
      actions.appendChild(web);
    }

    const view = document.createElement("button");
    view.className = "clip-view";
    view.textContent = "👁";
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
        alert(`打开失败: ${err}`);
      }
    };

    const pin = document.createElement("button");
    pin.className = "clip-pin" + (c.pinned ? " active" : "");
    pin.textContent = "📌";
    pin.title = c.pinned ? "取消置顶" : "置顶（排到最前）";
    pin.onclick = async (e) => {
      e.stopPropagation();
      await invoke("toggle_pin", { id: c.id });
      refresh(true);
    };

    const del = document.createElement("button");
    del.className = "clip-del";
    del.textContent = "✕";
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

    item.onclick = () => invoke("paste_clip", { id: c.id });
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
    if (clips[selected]) {
      await invoke("paste_clip", { id: clips[selected].id });
    }
  } else if (e.key === "Escape") {
    e.preventDefault();
    await getCurrentWindow().hide();
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
  refresh();
  searchEl.focus();
});
listen<AppConfig>("config-changed", (e) => {
  config = e.payload;
  applyAppearance(config);
  updateHint();
  toggleEnabledEl.checked = config.enabled;
});

(async () => {
  config = await loadConfig();
  applyAppearance(config);
  updateHint();
  toggleEnabledEl.checked = config.enabled;
  refresh();
})();
