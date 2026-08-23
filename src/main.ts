import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { applyAppearance, AppConfig, loadConfig } from "./config";

interface Clip {
  id: number;
  kind: string; // "text" | "image"
  preview: string;
  image_b64: string | null;
  pinned: boolean;
  created_at: number; // 秒
}

const listEl = document.querySelector<HTMLDivElement>("#list")!;
const emptyEl = document.querySelector<HTMLDivElement>("#empty")!;
const searchEl = document.querySelector<HTMLInputElement>("#search")!;
const hintEl = document.querySelector<HTMLDivElement>("#hint")!;

let keyword = "";
let searchTimer: number | undefined;
let clips: Clip[] = [];
let selected = 0;
let config: AppConfig | null = null;

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
  clips = await invoke<Clip[]>("list_clips", {
    keyword: keyword.trim() || null,
  });
  emptyEl.style.display = clips.length ? "none" : "block";
  listEl.innerHTML = "";
  if (!keepSelection) selected = 0;
  if (selected >= clips.length) selected = Math.max(0, clips.length - 1);

  clips.forEach((c, idx) => {
    const item = document.createElement("div");
    item.className = "clip-item" + (c.pinned ? " pinned" : "");

    const body = document.createElement("div");
    body.className = "clip-body";
    if (c.kind === "image" && c.image_b64) {
      const img = document.createElement("img");
      img.src = `data:image/png;base64,${c.image_b64}`;
      img.className = "clip-thumb";
      body.appendChild(img);
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
      if (c.pinned && !confirm("该记录已置顶，确定一并删除吗？")) return;
      await invoke("delete_clip", { id: c.id });
      refresh(true);
    };

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

// 批量删除：先判断范围内是否有置顶记录，让用户决定是否连置顶一起删
document.querySelectorAll<HTMLButtonElement>(".del-group button").forEach((btn) => {
  btn.onclick = async () => {
    const range = btn.dataset.range!;
    const pinnedCount = await invoke<number>("count_pinned_in_range", { range });
    let includePinned = false;
    if (pinnedCount > 0) {
      includePinned = confirm(
        `该范围内有 ${pinnedCount} 条置顶记录。\n「确定」= 连同置顶内容一起删除\n「取消」= 只删除非置顶记录`
      );
    } else if (range === "all") {
      if (!confirm("确定清空全部剪贴板记录？")) return;
    }
    await invoke("delete_range", { range, includePinned });
    refresh();
  };
});

document.querySelector<HTMLButtonElement>("#btn-settings")!.onclick = () =>
  invoke("open_settings");

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
});

(async () => {
  config = await loadConfig();
  applyAppearance(config);
  updateHint();
  refresh();
})();
