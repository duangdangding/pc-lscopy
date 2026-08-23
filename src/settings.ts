import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import { applyAppearance, AppConfig, loadConfig } from "./config";
import { confirmDialog } from "./confirm";

const $ = <T extends HTMLElement>(sel: string) =>
  document.querySelector<T>(sel)!;

const hotkeyEl = $<HTMLInputElement>("#hotkey");
const autostartEl = $<HTMLInputElement>("#autostart");
const silentStartEl = $<HTMLInputElement>("#silent-start");
const dbDirEl = $<HTMLInputElement>("#db-dir");
const themeEl = $<HTMLSelectElement>("#theme");
const fontFamilyEl = $<HTMLInputElement>("#font-family");
const fontSizeEl = $<HTMLInputElement>("#font-size");
const maxItemsEl = $<HTMLInputElement>("#max-items");
const retentionValueEl = $<HTMLInputElement>("#retention-value");
const retentionUnitEl = $<HTMLSelectElement>("#retention-unit");
const excludeAppsEl = $<HTMLTextAreaElement>("#exclude-apps");
const saveMsgEl = $<HTMLSpanElement>("#save-msg");

let config: AppConfig;

// ---------- 标签页切换 ----------
document.querySelectorAll<HTMLButtonElement>(".tabs .tab").forEach((tab) => {
  tab.onclick = () => {
    document.querySelectorAll(".tabs .tab").forEach((t) => t.classList.remove("active"));
    document.querySelectorAll(".tab-panel").forEach((p) => ((p as HTMLElement).hidden = true));
    tab.classList.add("active");
    const panel = $(`#tab-${tab.dataset.tab}`);
    panel.hidden = false;
    if (tab.dataset.tab === "database") refreshDbInfo();
  };
});

// ---------- 快捷键录制 ----------
function keyName(e: KeyboardEvent): string | null {
  const k = e.key;
  if (["Control", "Shift", "Alt", "Meta"].includes(k)) return null;
  if (k === " ") return "Space";
  if (k === "`" || k === "~") return "`";
  if (/^arrow/i.test(k)) return k.replace(/^Arrow/, "");
  if (k === "Escape") return "Esc";
  if (k.length === 1) return k.toUpperCase();
  return k; // Enter / Tab / F1-F12 / Home / End ...
}

hotkeyEl.addEventListener("keydown", (e) => {
  e.preventDefault();
  const key = keyName(e);
  if (!key) return; // 只按了修饰键，继续等
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.shiftKey) parts.push("Shift");
  if (e.altKey) parts.push("Alt");
  if (e.metaKey) parts.push("Win");
  if (!parts.length) {
    hotkeyEl.value = "请同时按住 Ctrl / Shift / Alt / Win 中至少一个";
    return;
  }
  parts.push(key);
  hotkeyEl.value = parts.join("+");
});

$("#hotkey-reset").addEventListener("click", () => {
  hotkeyEl.value = "Ctrl+`";
});

// ---------- 数据库目录 ----------
$("#db-browse").addEventListener("click", async () => {
  const dir = await open({ directory: true, title: "选择数据库所在目录" });
  if (typeof dir === "string") dbDirEl.value = dir;
});
$("#db-default").addEventListener("click", () => {
  dbDirEl.value = "";
});

// ---------- 导入 / 导出 ----------
$("#btn-export").addEventListener("click", async () => {
  const path = await save({
    title: "导出剪贴板记录",
    defaultPath: "lscopy-backup.json",
    filters: [{ name: "JSON", extensions: ["json"] }],
  });
  if (!path) return;
  try {
    const n = await invoke<number>("export_clips", { path });
    alert(`已导出 ${n} 条记录到:\n${path}`);
  } catch (e) {
    alert(`导出失败: ${e}`);
  }
});

$("#btn-import").addEventListener("click", async () => {
  const path = await open({
    title: "导入剪贴板记录",
    filters: [{ name: "JSON", extensions: ["json"] }],
  });
  if (typeof path !== "string") return;
  try {
    const n = await invoke<number>("import_clips", { path });
    alert(`已导入 ${n} 条记录`);
    refreshDbInfo();
  } catch (e) {
    alert(`导入失败: ${e}`);
  }
});

// ---------- 删除管理 ----------
// 预设范围批量删除：范围内有置顶记录时先询问
document.querySelectorAll<HTMLButtonElement>("[data-range]").forEach((btn) => {
  btn.onclick = async () => {
    const range = btn.dataset.range!;
    const pinnedCount = await invoke<number>("count_pinned_in_range", { range });
    let includePinned = false;
    if (pinnedCount > 0) {
      includePinned = await confirmDialog(
        `该范围内有 ${pinnedCount} 条置顶记录。\n「确定」= 连同置顶内容一起删除\n「取消」= 只删除非置顶记录`
      );
    } else if (range === "all") {
      if (!(await confirmDialog("确定清空全部剪贴板记录？"))) return;
    }
    const n = await invoke<number>("delete_range", { range, includePinned });
    $("#del-result").textContent = `已删除 ${n} 条记录`;
    refreshDbInfo();
  };
});

// 自定义时间区间删除
$("#btn-del-between").addEventListener("click", async () => {
  const startVal = $<HTMLInputElement>("#del-start").value;
  const endVal = $<HTMLInputElement>("#del-end").value;
  if (!startVal || !endVal) {
    alert("请选择开始和结束时间");
    return;
  }
  const start = Math.floor(new Date(startVal).getTime() / 1000);
  const end = Math.floor(new Date(endVal).getTime() / 1000);
  if (start > end) {
    alert("开始时间不能晚于结束时间");
    return;
  }
  const pinnedCount = await invoke<number>("count_pinned_between", { start, end });
  let includePinned = false;
  if (pinnedCount > 0) {
    includePinned = await confirmDialog(
      `该区间内有 ${pinnedCount} 条置顶记录。\n「确定」= 连同置顶内容一起删除\n「取消」= 只删除非置顶记录`
    );
  } else if (!(await confirmDialog("确定删除该时间区间内的所有记录？"))) {
    return;
  }
  const n = await invoke<number>("delete_between", { start, end, includePinned });
  $("#del-result").textContent = `已删除 ${n} 条记录`;
  refreshDbInfo();
});

// ---------- 数据库信息 ----------
function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

async function refreshDbInfo() {
  const info = await invoke<{
    path: string;
    file_size: number;
    total: number;
    text_count: number;
    image_count: number;
    pinned_count: number;
    max_items: number;
  }>("get_db_info");
  $("#db-info").innerHTML = `
    <div>文件位置：<code>${info.path}</code></div>
    <div>文件大小：${fmtSize(info.file_size)}</div>
    <div>总记录数：${info.total} 条（文字 ${info.text_count} · 图片 ${info.image_count} · 置顶 ${info.pinned_count}）</div>
    <div>保留上限：${info.max_items} 条（不含置顶）</div>
  `;
}
$("#db-info-refresh").addEventListener("click", refreshDbInfo);

// ---------- 保存 ----------
$("#btn-save").addEventListener("click", async () => {
  const next: AppConfig = {
    hotkey: hotkeyEl.value.trim() || "Ctrl+`",
    autostart: autostartEl.checked,
    silent_start: silentStartEl.checked,
    db_dir: dbDirEl.value.trim() || null,
    theme: themeEl.value,
    font_family: fontFamilyEl.value.trim(),
    font_size: Math.max(10, Math.min(24, Number(fontSizeEl.value) || 14)),
    exclude_apps: excludeAppsEl.value
      .split(/\r?\n/)
      .map((s) => s.trim())
      .filter(Boolean),
    max_items: Math.max(10, Number(maxItemsEl.value) || 500),
    retention_value: Math.max(0, Number(retentionValueEl.value) || 0),
    retention_unit: retentionUnitEl.value,
  };
  try {
    await invoke("save_config", { config: next });
    config = next;
    saveMsgEl.textContent = "✓ 已保存";
    window.setTimeout(() => (saveMsgEl.textContent = ""), 2000);
    refreshDbInfo();
  } catch (e) {
    saveMsgEl.textContent = "";
    alert(`保存失败: ${e}`);
  }
});

// ---------- 字体可搜索下拉框 ----------
const fontDropdownEl = $<HTMLDivElement>("#font-dropdown");
let allFonts: string[] = [];

function renderFontDropdown(filter: string) {
  const q = filter.trim().toLowerCase();
  const matched = allFonts.filter((f) => f.toLowerCase().includes(q)).slice(0, 200);
  fontDropdownEl.innerHTML = "";
  for (const f of matched) {
    const div = document.createElement("div");
    div.className = "font-option" + (f === fontFamilyEl.value ? " active" : "");
    div.textContent = f;
    div.style.fontFamily = f;
    div.title = f;
    div.onmousedown = (e) => {
      e.preventDefault(); // 阻止 input 失焦，保证点击生效
      fontFamilyEl.value = f;
      closeFontDropdown();
    };
    fontDropdownEl.appendChild(div);
  }
  fontDropdownEl.style.display = matched.length ? "block" : "none";
}

function closeFontDropdown() {
  fontDropdownEl.style.display = "none";
}

function initFontPicker(fonts: string[]) {
  allFonts = fonts;
  fontFamilyEl.addEventListener("focus", () => renderFontDropdown(""));
  fontFamilyEl.addEventListener("input", () => renderFontDropdown(fontFamilyEl.value));
  fontFamilyEl.addEventListener("blur", closeFontDropdown);
  fontFamilyEl.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeFontDropdown();
      fontFamilyEl.blur();
    } else if (e.key === "Enter") {
      e.preventDefault();
      const first = fontDropdownEl.querySelector<HTMLDivElement>(".font-option");
      if (first && fontDropdownEl.style.display !== "none") {
        fontFamilyEl.value = first.textContent || first.title;
        closeFontDropdown();
      }
    }
  });
}

listen<AppConfig>("config-changed", (e) => applyAppearance(e.payload));

(async () => {
  config = await loadConfig();
  applyAppearance(config);

  hotkeyEl.value = config.hotkey;
  autostartEl.checked = config.autostart;
  silentStartEl.checked = config.silent_start;
  dbDirEl.value = config.db_dir || "";
  themeEl.value = config.theme;
  fontFamilyEl.value = config.font_family;
  fontSizeEl.value = String(config.font_size);
  maxItemsEl.value = String(config.max_items);
  retentionValueEl.value = String(config.retention_value ?? 0);
  retentionUnitEl.value = config.retention_unit || "days";
  excludeAppsEl.value = (config.exclude_apps || []).join("\n");

  // 加载系统字体列表：可搜索下拉框，每项用自身字体预览
  try {
    const fonts = await invoke<string[]>("list_system_fonts");
    if (config.font_family && !fonts.includes(config.font_family)) {
      fonts.unshift(config.font_family);
    }
    initFontPicker(fonts);
  } catch {
    /* 获取失败时保持手动输入 */
  }

  refreshDbInfo();
})();
