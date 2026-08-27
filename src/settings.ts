import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import { applyAppearance, AppConfig, loadConfig } from "./config";
import { confirmDialog, choiceDialog } from "./confirm";

const $ = <T extends HTMLElement>(sel: string) =>
  document.querySelector<T>(sel)!;

const hotkeyEl = $<HTMLInputElement>("#hotkey");
const enabledEl = $<HTMLInputElement>("#enabled");
const autostartEl = $<HTMLInputElement>("#autostart");
const silentStartEl = $<HTMLInputElement>("#silent-start");
const rememberSizeEl = $<HTMLInputElement>("#remember-size");
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
    setLanPolling(tab.dataset.tab === "sync");
    // 局域网同步页的所有设置即时生效，隐藏底部「保存设置」栏
    document.querySelector<HTMLElement>(".settings-footer")!.hidden =
      tab.dataset.tab === "sync";
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
// 范围内有置顶记录时的三选一询问：连同置顶删 / 只删非置顶 / 取消
async function askPinnedChoice(pinnedCount: number, scope: string): Promise<boolean | null> {
  const choice = await choiceDialog(
    `该${scope}内有 ${pinnedCount} 条置顶记录。`,
    [
      { value: "cancel", text: "取消" },
      { value: "unpinned", text: "只删除非置顶" },
      { value: "all", text: "连同置顶一起删除", kind: "danger" },
    ]
  );
  if (choice === null || choice === "cancel") return null;
  return choice === "all";
}

// 预设范围批量删除：范围内有置顶记录时先询问
document.querySelectorAll<HTMLButtonElement>("[data-range]").forEach((btn) => {
  btn.onclick = async () => {
    const range = btn.dataset.range!;
    const pinnedCount = await invoke<number>("count_pinned_in_range", { range });
    let includePinned = false;
    if (pinnedCount > 0) {
      const choice = await askPinnedChoice(pinnedCount, "范围");
      if (choice === null) return; // 取消：什么都不删
      includePinned = choice;
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
    const choice = await askPinnedChoice(pinnedCount, "区间");
    if (choice === null) return; // 取消：什么都不删
    includePinned = choice;
  } else if (!(await confirmDialog("确定删除该时间区间内的所有记录？"))) {
    return;
  }
  const n = await invoke<number>("delete_between", { start, end, includePinned });
  $("#del-result").textContent = `已删除 ${n} 条记录`;
  refreshDbInfo();
});

// ---------- 局域网同步 ----------
interface LanDeviceDto {
  device_id: string;
  name: string;
  model: string | null;
  host: string | null;
  port: number;
  sharing: boolean;
  paired: boolean;
  online: boolean;
  last_sync: number; // 毫秒
  syncing: boolean;
}

interface LanStateDto {
  device_id: string;
  device_name: string;
  pairing_token: string;
  server_port: number;
  actual_port: number;
  server_running: boolean;
  local_ip: string;
  discoverable: boolean;
  sharing: boolean;
  auto_sync: boolean;
  auto_accept_pair: boolean;
  devices: LanDeviceDto[];
  blocked: { device_id: string; name: string; model: string | null }[];
}

const lanDiscoverableEl = $<HTMLInputElement>("#lan-discoverable");
const lanSharingEl = $<HTMLInputElement>("#lan-sharing");
const lanAutoSyncEl = $<HTMLInputElement>("#lan-auto-sync");
const lanAutoAcceptEl = $<HTMLInputElement>("#lan-auto-accept");
const lanNameEl = $<HTMLInputElement>("#lan-name");
const lanTokenEl = $<HTMLElement>("#lan-token");
const lanPortEl = $<HTMLInputElement>("#lan-port");
const lanDevicesEl = $<HTMLDivElement>("#lan-devices");

let lanTimer: number | undefined;
let lanLoaded = false;

// 开关即时生效（不走底部"保存设置"按钮）
function lanPatch(patch: Record<string, unknown>) {
  invoke("lan_update_settings", { patch }).catch((e) => alert(`设置失败: ${e}`));
}

lanDiscoverableEl.onchange = () => lanPatch({ discoverable: lanDiscoverableEl.checked });
lanSharingEl.onchange = () => lanPatch({ sharing: lanSharingEl.checked });
lanAutoSyncEl.onchange = () => lanPatch({ auto_sync: lanAutoSyncEl.checked });
lanAutoAcceptEl.onchange = () => lanPatch({ auto_accept_pair: lanAutoAcceptEl.checked });

$("#lan-name-save").addEventListener("click", () => {
  const name = lanNameEl.value.trim();
  if (name) lanPatch({ device_name: name });
});

$("#lan-port-save").addEventListener("click", () => {
  const port = Number(lanPortEl.value) || 8765;
  if (port < 1024 || port > 65535) {
    alert("端口范围 1024-65535");
    return;
  }
  lanPatch({ server_port: port });
  setTimeout(refreshLanState, 500);
});

$("#lan-token-regen").addEventListener("click", async () => {
  if (!(await confirmDialog("重新生成配对码后，已配对设备需要用新码重新配对。确定继续？"))) return;
  const t = await invoke<string>("lan_regenerate_token");
  lanTokenEl.textContent = t;
});

$("#lan-open-blocked").addEventListener("click", () => {
  invoke("open_blocked").catch(() => {});
});

$("#lan-refresh").addEventListener("click", async () => {
  const btn = $<HTMLButtonElement>("#lan-refresh");
  if (btn.disabled) return;
  btn.disabled = true;
  btn.textContent = "刷新中…";
  await refreshLanState();
  btn.textContent = "✓ 已刷新";
  window.setTimeout(() => {
    btn.disabled = false;
    btn.textContent = "刷新";
  }, 1000);
});

$("#lan-sweep").addEventListener("click", async () => {
  const btn = $<HTMLButtonElement>("#lan-sweep");
  btn.disabled = true;
  btn.textContent = "扫描中…";
  try {
    await invoke("lan_sweep");
  } finally {
    btn.disabled = false;
    btn.textContent = "深度扫描";
    refreshLanState();
  }
});

$("#lan-add-ip").addEventListener("click", async () => {
  const ipEl = $<HTMLInputElement>("#lan-manual-ip");
  const ip = ipEl.value.trim();
  if (!ip) return;
  const ok = await invoke<boolean>("lan_add_device", { ip });
  if (ok) {
    ipEl.value = "";
    refreshLanState();
  } else {
    alert("未找到设备：请确认对方应用已启动且开启了「可被发现」");
  }
});

function fmtTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}-${d.getDate()} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function lanBtn(text: string, cls = ""): HTMLButtonElement {
  const b = document.createElement("button");
  b.className = `btn small ${cls}`.trim();
  b.textContent = text;
  return b;
}

// 正在配对的设备 id 与已输入的配对码草稿：
// 列表每 2s 轮询会整体重建，靠这两个状态在重建后恢复配对表单
let pairingDeviceId: string | null = null;
let pairingDraft = "";
let lastDevices: LanDeviceDto[] = [];

// 配对按钮点击后变成"输入配对码 + 确定/取消"的内联表单
function pairForm(device: LanDeviceDto): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "lan-actions";
  const input = document.createElement("input");
  input.type = "text";
  input.placeholder = "对方配对码";
  input.maxLength = 6;
  input.style.width = "90px";
  input.value = pairingDraft;
  input.addEventListener("input", () => (pairingDraft = input.value));
  const ok = lanBtn("确定", "primary");
  const cancel = lanBtn("取消");
  ok.onclick = async () => {
    const token = input.value.trim();
    if (!token) return;
    ok.disabled = true;
    ok.textContent = "配对中…";
    try {
      await invoke("lan_pair", { deviceId: device.device_id, token });
      pairingDeviceId = null;
      pairingDraft = "";
      refreshLanState();
    } catch (e) {
      alert(`配对失败: ${e}`);
      ok.disabled = false;
      ok.textContent = "确定";
    }
  };
  cancel.onclick = () => {
    pairingDeviceId = null;
    pairingDraft = "";
    renderLanDevices(lastDevices);
  };
  wrap.append(input, ok, cancel);
  // 表单展开后自动聚焦输入框
  window.setTimeout(() => input.focus(), 0);
  return wrap;
}

function renderLanDevices(devices: LanDeviceDto[]) {
  lanDevicesEl.innerHTML = "";
  if (!devices.length) {
    const p = document.createElement("p");
    p.className = "desc";
    p.textContent = "未发现设备。请确认对方已开启「可被发现」，或手动添加 IP。";
    lanDevicesEl.appendChild(p);
    return;
  }
  for (const d of devices) {
    const row = document.createElement("div");
    row.className = "lan-device";

    const dot = document.createElement("span");
    dot.className = "lan-dot" + (d.online ? " online" : "");

    const meta = document.createElement("div");
    meta.className = "lan-meta";
    const title = document.createElement("div");
    title.className = "lan-title";
    title.textContent = d.name || "未知设备";
    const badge = document.createElement("span");
    badge.className = "lan-badge" + (d.sharing ? "" : " off");
    badge.textContent = d.sharing ? "共享中" : "未共享";
    title.appendChild(badge);
    if (d.paired) {
      const pb = document.createElement("span");
      pb.className = "lan-badge";
      pb.textContent = "已配对";
      title.appendChild(pb);
    }
    const sub = document.createElement("div");
    sub.className = "lan-sub";
    sub.textContent =
      [d.model, d.host ? `${d.host}:${d.port}` : null, d.online ? "在线" : "离线"]
        .filter(Boolean)
        .join(" · ") + (d.last_sync ? ` · 上次同步 ${fmtTime(d.last_sync)}` : "");
    meta.append(title, sub);

    const actions = document.createElement("div");
    actions.className = "lan-actions";
    if (d.syncing) {
      const s = lanBtn("同步中…");
      s.disabled = true;
      actions.appendChild(s);
    } else if (!d.paired) {
      if (pairingDeviceId === d.device_id) {
        // 该设备正在配对：直接渲染表单（轮询重建后恢复）
        row.append(dot, meta, pairForm(d));
        lanDevicesEl.appendChild(row);
        continue;
      }
      const pair = lanBtn("配对", "primary");
      pair.onclick = async () => {
        // 对方开启「自动同意配对」时免码直接配对；需要配对码时才展开输入框
        pair.disabled = true;
        pair.textContent = "配对中…";
        try {
          await invoke("lan_pair", { deviceId: d.device_id, token: "" });
          refreshLanState();
        } catch (e) {
          pair.disabled = false;
          pair.textContent = "配对";
          if (String(e).includes("__need_token__")) {
            pairingDeviceId = d.device_id;
            pairingDraft = "";
            renderLanDevices(lastDevices);
          } else {
            alert(`配对失败: ${e}`);
          }
        }
      };
      actions.appendChild(pair);
    } else {
      const sync = lanBtn("同步");
      sync.onclick = async () => {
        sync.disabled = true;
        try {
          const msg = await invoke<string>("lan_sync_now", { deviceId: d.device_id });
          alert(msg);
        } catch (e) {
          alert(`同步失败: ${e}`);
        }
        refreshLanState();
      };
      const unpair = lanBtn("解除配对");
      unpair.onclick = async () => {
        if (!(await confirmDialog(`确定解除与「${d.name}」的配对？`))) return;
        await invoke("lan_unpair", { deviceId: d.device_id });
        refreshLanState();
      };
      actions.append(sync, unpair);
    }
    const block = lanBtn("拉黑", "danger");
    block.onclick = async () => {
      if (
        !(await confirmDialog(
          `拉黑「${d.name}」后将互相不可见、无法同步。可在本页黑名单中恢复。确定？`
        ))
      )
        return;
      await invoke("lan_block", { deviceId: d.device_id });
      refreshLanState();
    };
    actions.appendChild(block);

    row.append(dot, meta, actions);
    lanDevicesEl.appendChild(row);
  }
}

async function refreshLanState() {
  try {
    const s = await invoke<LanStateDto>("lan_get_state");
    lanDiscoverableEl.checked = s.discoverable;
    lanSharingEl.checked = s.sharing;
    lanAutoSyncEl.checked = s.auto_sync;
    lanAutoAcceptEl.checked = s.auto_accept_pair;
    if (!lanLoaded || document.activeElement !== lanNameEl) lanNameEl.value = s.device_name;
    if (!lanLoaded || document.activeElement !== lanPortEl) lanPortEl.value = String(s.server_port);
    lanTokenEl.textContent = s.pairing_token;
    $("#lan-ip").textContent = s.local_ip || "未联网";
    $("#lan-server-status").textContent = s.server_running
      ? `🟢 运行中 · 端口 ${s.actual_port}${s.actual_port !== s.server_port ? "（配置端口被占用）" : ""}`
      : "⚪ 未运行（开启「可被发现」或「共享剪贴板」后自动启动）";
    lastDevices = s.devices;
    // 配对表单展开期间不重建设备列表（避免输入框被轮询刷新掉、丢失焦点）
    if (!pairingDeviceId) renderLanDevices(s.devices);
    lanLoaded = true;
  } catch {
    /* 后端未就绪时静默 */
  }
}

function setLanPolling(on: boolean) {
  if (on && lanTimer === undefined) {
    refreshLanState();
    lanTimer = window.setInterval(refreshLanState, 2000);
  } else if (!on && lanTimer !== undefined) {
    window.clearInterval(lanTimer);
    lanTimer = undefined;
  }
}

// 对方发起配对请求：弹窗确认（30 秒无人答复则自动按"需确认"失败）
listen<{ device_id: string; name: string; model: string | null; host: string | null }>(
  "lan-pair-request",
  async (e) => {
    const d = e.payload;
    const accept = await confirmDialog(
      `设备「${d.name}」${d.model ? `（${d.model}）` : ""}${d.host ? `\n来自 ${d.host}` : ""}\n请求与本机配对并同步剪贴板。是否同意？`
    );
    await invoke("lan_respond_pair", { deviceId: d.device_id, accept });
    refreshLanState();
  }
);

listen("lan-state-changed", () => {
  if (lanTimer !== undefined) refreshLanState();
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
    <div>保留上限：${info.max_items > 0 ? `${info.max_items} 条（不含置顶）` : "无限制"}</div>
  `;
}
$("#db-info-refresh").addEventListener("click", refreshDbInfo);

// ---------- 保存 ----------
$("#btn-save").addEventListener("click", async () => {
  const next: AppConfig = {
    hotkey: hotkeyEl.value.trim() || "Ctrl+`",
    enabled: enabledEl.checked,
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
    max_items: Math.max(0, Number(maxItemsEl.value) || 0),
    retention_value: Math.max(0, Number(retentionValueEl.value) || 0),
    retention_unit: retentionUnitEl.value,
    remember_size: rememberSizeEl.checked,
    // 尺寸由后端在开启时抓取当前实际值，这里带上已有值兜底
    window_width: config.window_width,
    window_height: config.window_height,
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

// ---------- 字体选择器：只读输入框 + 面板内搜索 ----------
const fontDropdownEl = $<HTMLDivElement>("#font-dropdown");
const fontSearchEl = $<HTMLInputElement>("#font-search");
const fontListEl = $<HTMLDivElement>("#font-list");
const fontPickerEl = document.querySelector<HTMLDivElement>(".font-picker")!;
let allFonts: string[] = [];

function renderFontList(filter: string) {
  const q = filter.trim().toLowerCase();
  const matched = allFonts.filter((f) => f.toLowerCase().includes(q)).slice(0, 200);
  fontListEl.innerHTML = "";
  for (const f of matched) {
    const div = document.createElement("div");
    div.className = "font-option" + (f === fontFamilyEl.value ? " active" : "");
    div.textContent = f;
    div.style.fontFamily = f;
    div.title = f;
    div.onclick = () => {
      fontFamilyEl.value = f;
      closeFontDropdown();
    };
    fontListEl.appendChild(div);
  }
}

function openFontDropdown() {
  renderFontList("");
  fontSearchEl.value = "";
  fontDropdownEl.style.display = "block";
  window.setTimeout(() => fontSearchEl.focus(), 0);
}

function closeFontDropdown() {
  fontDropdownEl.style.display = "none";
}

function initFontPicker(fonts: string[]) {
  allFonts = fonts;
  fontFamilyEl.addEventListener("click", () => {
    if (fontDropdownEl.style.display === "block") {
      closeFontDropdown();
    } else {
      openFontDropdown();
    }
  });
  fontSearchEl.addEventListener("input", () => renderFontList(fontSearchEl.value));
  fontSearchEl.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeFontDropdown();
    } else if (e.key === "Enter") {
      e.preventDefault();
      const first = fontListEl.querySelector<HTMLDivElement>(".font-option");
      if (first) {
        fontFamilyEl.value = first.title;
        closeFontDropdown();
      }
    }
  });
  // 点击选择器外部时关闭
  document.addEventListener("mousedown", (e) => {
    if (!fontPickerEl.contains(e.target as Node)) closeFontDropdown();
  });
}

listen<AppConfig>("config-changed", (e) => {
  applyAppearance(e.payload);
  enabledEl.checked = e.payload.enabled;
});

(async () => {
  config = await loadConfig();
  applyAppearance(config);

  hotkeyEl.value = config.hotkey;
  enabledEl.checked = config.enabled;
  autostartEl.checked = config.autostart;
  silentStartEl.checked = config.silent_start;
  rememberSizeEl.checked = config.remember_size ?? false;
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
