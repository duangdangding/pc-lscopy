import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { open } from "@tauri-apps/plugin-dialog";
import { applyAppearance, applyWindowEffect, applyWindowBackground, AppConfig, initEscClose, loadConfig } from "./config";
import { alertDialog, choiceDialog } from "./confirm";
import { initLockGuard } from "./lock";

const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;

interface LanDeviceDto {
  device_id: string;
  name: string;
  model: string | null;
  host: string | null;
  port: number;
  sharing: boolean;
  paired: boolean;
  online: boolean;
  via: string; // "" 离线 | "lan" | "relay" | "lan+relay"
}

interface LanStateDto {
  device_name: string;
  local_ip: string;
  download_dir: string;
  transfer_auto_accept: boolean;
  devices: LanDeviceDto[];
}

interface TransferDto {
  id: number;
  direction: string; // "send" | "recv"
  peer: string;
  file_name: string;
  path: string | null;
  size: number;
  ok: boolean;
  msg: string | null;
  created_at: number; // 秒级时间戳
}

interface TransferProgress {
  direction: string;
  peer: string;
  name: string;
  index: number;
  count: number;
  sent: number;
  total: number;
}

interface TransferIncoming {
  request_id: string;
  peer: string;
  name: string;
  size: number;
  host: string;
  device_key: string;
}

interface TransferBatchIncoming {
  request_id: string;
  peer: string;
  host: string;
  device_key: string;
  files: { name: string; size: number }[];
}

// ---------- 状态 ----------

let selectedDeviceId: string | null = null;
let sending = false;
let sweeping = false;
let devices: LanDeviceDto[] = [];

// 发送文件列表：发送时逐项显示状态（等待/传输中/完成）
interface SendItem {
  name: string;
  state: "pending" | "sending" | "done";
}
let sendItems: SendItem[] = [];

function baseName(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() || p;
}

function renderSendList() {
  const el = $<HTMLDivElement>("#tf-send-list");
  el.hidden = sendItems.length === 0;
  el.innerHTML = "";
  for (const f of sendItems) {
    const row = document.createElement("div");
    row.className = `tf-send-item ${f.state}`;
    const st = document.createElement("span");
    st.className = "st";
    st.textContent = f.state === "done" ? "✓" : f.state === "sending" ? "⟳" : "○";
    const nm = document.createElement("span");
    nm.className = "nm";
    nm.textContent = f.name;
    row.append(st, nm);
    el.appendChild(row);
  }
}

// ---------- 工具 ----------

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function fmtTime(secs: number): string {
  const d = new Date(secs * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}-${d.getDate()} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// ---------- 设备列表 ----------

function renderDevices() {
  const listEl = $<HTMLDivElement>("#tf-devices");
  listEl.innerHTML = "";
  // 只显示实时扫描到的在线设备：离线/历史配对记录不出现在列表里；
  // 互传走局域网 HTTP（POST /recv），仅云端可达（via === "relay"）的设备也要排除
  const shown = devices.filter((d) => d.online && d.via !== "relay");
  if (!shown.length) {
    const p = document.createElement("p");
    p.className = "desc";
    p.textContent = "未发现在线设备。请确认对方应用已启动，或直接在下方输入对方 IP 发送。";
    listEl.appendChild(p);
    return;
  }
  // 已选中的设备掉线时清除选择
  if (selectedDeviceId && !shown.some((d) => d.device_id === selectedDeviceId)) {
    selectedDeviceId = null;
  }
  for (const d of shown) {
    const row = document.createElement("div");
    row.className = "lan-device tf-device" + (d.device_id === selectedDeviceId ? " selected" : "");

    const dot = document.createElement("span");
    dot.className = "lan-dot online";

    const meta = document.createElement("div");
    meta.className = "lan-meta";
    const title = document.createElement("div");
    title.className = "lan-title";
    title.textContent = d.name || "未知设备";
    const sub = document.createElement("div");
    sub.className = "lan-sub";
    sub.textContent = [d.model, d.host ? `${d.host}:${d.port}` : null, "在线"]
      .filter(Boolean)
      .join(" · ");
    meta.append(title, sub);

    row.append(dot, meta);
    row.onclick = () => {
      selectedDeviceId = d.device_id === selectedDeviceId ? null : d.device_id;
      renderDevices();
    };
    listEl.appendChild(row);
  }
}

async function refreshDevices() {
  try {
    const s = await invoke<LanStateDto>("lan_get_state");
    devices = s.devices;
    $("#tf-dir").textContent = s.download_dir.trim() || "系统下载目录";
    // 本机信息：名称 + IP，便于发送方确认接收对象
    $("#tf-self").textContent = s.local_ip
      ? `${s.device_name}（${s.local_ip}）`
      : `${s.device_name}（未联网）`;
    // 自动接收开关：仅当用户没在操作该复选框时回填（避免轮询打断点击）
    const autoEl = $<HTMLInputElement>("#tf-auto-accept");
    if (document.activeElement !== autoEl) autoEl.checked = s.transfer_auto_accept;
    renderDevices();
  } catch {
    /* 后端未就绪时静默 */
  }
}

// 主动深度扫描（遍历本机 /24 网段探测），发现结果通过 lan-state-changed 事件回来
async function sweepDevices() {
  if (sweeping) return;
  sweeping = true;
  try {
    await invoke("lan_sweep");
  } catch {
    /* 扫描失败静默 */
  } finally {
    sweeping = false;
    refreshDevices();
  }
}

// ---------- 发送 ----------

// 发送入口：deviceId 发给扫描到的设备；ip 发给手动输入的地址
async function sendPaths(paths: string[], target: { deviceId?: string; ip?: string }) {
  if (sending) return;
  if (!paths.length) return;
  sending = true;
  const pickBtn = $<HTMLButtonElement>("#tf-pick");
  const cancelBtn = $<HTMLButtonElement>("#tf-cancel");
  pickBtn.disabled = true;
  cancelBtn.disabled = false;
  cancelBtn.textContent = "取消发送";
  showProgress(true);
  sendItems = paths.map((p) => ({ name: baseName(p), state: "pending" as const }));
  renderSendList();
  try {
    const msg = target.ip
      ? await invoke<string>("transfer_send_ip", { ip: target.ip, paths })
      : await invoke<string>("transfer_send", { deviceId: target.deviceId, paths });
    if (msg.startsWith("已发送") || msg.startsWith("已取消")) {
      showResultToast(msg); // 全部成功/用户取消：30s 后自动关闭
    } else {
      alertDialog(msg); // 有失败：保留手动关闭，避免错过错误信息
    }
  } catch (e) {
    alertDialog(`发送失败: ${e}`);
  } finally {
    sending = false;
    pickBtn.disabled = false;
    showProgress(false);
    refreshHistory();
  }
}

// 取消发送：中断当前连接，剩余文件跳过（已传完的文件不受影响）
$<HTMLButtonElement>("#tf-cancel").onclick = () => {
  const cancelBtn = $<HTMLButtonElement>("#tf-cancel");
  cancelBtn.disabled = true;
  cancelBtn.textContent = "正在取消…";
  invoke("transfer_cancel").catch(() => {});
};

// 发送成功提示：底部 toast，倒计时 30s 自动关闭（可点 × 立即关闭）
let toastTimer: number | undefined;
function showResultToast(message: string, seconds = 30) {
  document.querySelector(".tf-toast")?.remove();
  if (toastTimer !== undefined) window.clearInterval(toastTimer);
  const toast = document.createElement("div");
  toast.className = "tf-toast";
  const text = document.createElement("span");
  text.className = "tf-toast-text";
  text.textContent = message;
  const cd = document.createElement("span");
  cd.className = "tf-toast-countdown";
  const close = document.createElement("button");
  close.className = "tf-toast-close";
  close.textContent = "×";
  close.title = "关闭";
  let left = seconds;
  cd.textContent = `${left}s`;
  const dismiss = () => {
    if (toastTimer !== undefined) window.clearInterval(toastTimer);
    toastTimer = undefined;
    toast.remove();
  };
  close.onclick = dismiss;
  toastTimer = window.setInterval(() => {
    left -= 1;
    if (left <= 0) {
      dismiss();
    } else {
      cd.textContent = `${left}s`;
    }
  }, 1000);
  toast.append(text, cd, close);
  document.body.appendChild(toast);
}

async function pickAndSend(target: { deviceId?: string; ip?: string }) {
  if (sending) return;
  const selected = await open({ multiple: true, directory: false });
  if (!selected) return;
  const paths = Array.isArray(selected) ? selected : [selected];
  sendPaths(paths, target);
}

function showProgress(show: boolean) {
  $("#tf-progress").hidden = !show;
  if (!show) {
    $<HTMLDivElement>("#tf-progress-fill").style.width = "0%";
    $("#tf-progress-text").textContent = "";
    sendItems = [];
    renderSendList();
  }
}

// ---------- 传输记录 ----------

let historyItems: TransferDto[] = [];

function renderHistory(items: TransferDto[]) {
  const listEl = $<HTMLDivElement>("#tf-history");
  listEl.innerHTML = "";
  if (!items.length) {
    const p = document.createElement("p");
    p.className = "desc";
    p.textContent = historyItems.length
      ? "没有匹配的记录"
      : "暂无记录（只保留接收成功的文件）";
    listEl.appendChild(p);
    return;
  }
  for (const t of items) {
    const row = document.createElement("div");
    row.className = "lan-device";

    // 记录只保留接收成功的条目，箭头固定为「接收」
    const arrow = document.createElement("span");
    arrow.className = "tf-arrow recv";
    arrow.textContent = "⬇";
    arrow.title = "已接收";

    const meta = document.createElement("div");
    meta.className = "lan-meta";
    const title = document.createElement("div");
    title.className = "lan-title";
    title.textContent = t.file_name;
    const sub = document.createElement("div");
    sub.className = "lan-sub";
    sub.textContent = `来自 ${t.peer} · ${fmtSize(t.size)} · ${fmtTime(t.created_at)}`;
    meta.append(title, sub);

    const actions = document.createElement("div");
    actions.className = "lan-actions";
    if (t.path) {
      const openBtn = document.createElement("button");
      openBtn.className = "btn small";
      openBtn.textContent = "打开位置";
      openBtn.onclick = async () => {
        try {
          await invoke("transfer_reveal", { path: t.path });
        } catch (e) {
          alertDialog(String(e));
        }
      };
      actions.appendChild(openBtn);
    }
    const delBtn = document.createElement("button");
    delBtn.className = "btn small danger";
    delBtn.textContent = "删除";
    delBtn.onclick = async () => {
      const c = await choiceDialog(`删除记录「${t.file_name}」？`, [
        { value: "cancel", text: "取消" },
        { value: "record", text: "仅删除记录" },
        { value: "file", text: "连同文件一起删除", kind: "danger" },
      ]);
      if (!c || c === "cancel") return;
      await invoke("transfer_delete", { id: t.id, deleteFile: c === "file" });
      refreshHistory();
    };
    actions.appendChild(delBtn);

    row.append(arrow, meta, actions);
    listEl.appendChild(row);
  }
}

// 按文件名 / 来源设备过滤传输记录
function applyHistoryFilter() {
  const kw = $<HTMLInputElement>("#tf-search").value.trim().toLowerCase();
  const filtered = kw
    ? historyItems.filter(
        (t) =>
          t.file_name.toLowerCase().includes(kw) ||
          t.peer.toLowerCase().includes(kw)
      )
    : historyItems;
  renderHistory(filtered);
}

async function refreshHistory() {
  try {
    historyItems = await invoke<TransferDto[]>("transfer_history");
    applyHistoryFilter();
  } catch {
    /* 后端未就绪时静默 */
  }
}

// ---------- 接收确认（手动模式） ----------

// 待接收列表：多个设备/多个文件同时发来时全部列在「待接收」卡片里，
// 可逐个或批量答复；后端每个请求 60 秒未答复自动拒绝，前端同步倒计时移除
const RECV_CONFIRM_TIMEOUT_MS = 60_000;
// 批量清单确认超时与后端一致（120 秒）
const BATCH_CONFIRM_TIMEOUT_MS = 120_000;

interface PendingRecv extends TransferIncoming {
  deadline: number;
}

interface PendingBatch extends TransferBatchIncoming {
  deadline: number;
}

const pendingRecvs = new Map<string, PendingRecv>();
const pendingBatches = new Map<string, PendingBatch>();
let pendingTimer: number | undefined;

function renderPendingRecvs() {
  const card = $<HTMLElement>("#tf-pending-card");
  const listEl = $<HTMLDivElement>("#tf-pending");
  const totalCount = pendingBatches.size + pendingRecvs.size;
  $("#tf-pending-count").textContent = String(totalCount);
  card.hidden = totalCount === 0;
  listEl.innerHTML = "";
  const now = Date.now();
  // 批量清单在前，单文件请求在后
  for (const b of pendingBatches.values()) {
    listEl.appendChild(buildBatchRow(b, now));
  }
  for (const p of pendingRecvs.values()) {
    const row = document.createElement("div");
    row.className = "lan-device";

    const arrow = document.createElement("span");
    arrow.className = "tf-arrow recv";
    arrow.textContent = "⬇";

    const meta = document.createElement("div");
    meta.className = "lan-meta";
    const title = document.createElement("div");
    title.className = "lan-title";
    title.textContent = p.name; // 对端传来的文件名，必须走 textContent 防注入
    const sub = document.createElement("div");
    sub.className = "lan-sub";
    sub.textContent = `${fmtSize(p.size)} · 来自「${p.peer}」（${p.host}）`;
    meta.append(title, sub);

    const actions = document.createElement("div");
    actions.className = "lan-actions";
    const cd = document.createElement("span");
    cd.className = "tf-pending-cd";
    cd.textContent = `${Math.max(0, Math.ceil((p.deadline - now) / 1000))}s`;
    const batchBtn = document.createElement("button");
    batchBtn.className = "btn small";
    batchBtn.textContent = "本批都接收";
    batchBtn.title = "接收此文件，并自动接收该设备 10 分钟内的后续文件（每收一个重新计时）";
    batchBtn.onclick = () => respondRecv(p.request_id, true, true);
    const acceptBtn = document.createElement("button");
    acceptBtn.className = "btn small primary";
    acceptBtn.textContent = "接收";
    acceptBtn.onclick = () => respondRecv(p.request_id, true);
    const rejectBtn = document.createElement("button");
    rejectBtn.className = "btn small danger";
    rejectBtn.textContent = "拒绝";
    rejectBtn.onclick = () => respondRecv(p.request_id, false);
    actions.append(cd, batchBtn, acceptBtn, rejectBtn);

    row.append(arrow, meta, actions);
    listEl.appendChild(row);
  }
}

// 批量清单行：头部（标题/来源/倒计时/按钮）+ 逐个勾选的文件列表
function buildBatchRow(b: PendingBatch, now: number): HTMLElement {
  const row = document.createElement("div");
  row.className = "lan-device tf-batch";

  const head = document.createElement("div");
  head.className = "tf-batch-head";

  const arrow = document.createElement("span");
  arrow.className = "tf-arrow recv";
  arrow.textContent = "⬇";

  const meta = document.createElement("div");
  meta.className = "lan-meta";
  const totalSize = b.files.reduce((acc, f) => acc + f.size, 0);
  const title = document.createElement("div");
  title.className = "lan-title";
  title.textContent = `批量接收 ${b.files.length} 个文件（共 ${fmtSize(totalSize)}）`;
  const sub = document.createElement("div");
  sub.className = "lan-sub";
  sub.textContent = `来自「${b.peer}」（${b.host}）· 勾选要接收的文件`;
  meta.append(title, sub);

  const actions = document.createElement("div");
  actions.className = "lan-actions";
  const cd = document.createElement("span");
  cd.className = "tf-pending-cd";
  cd.textContent = `${Math.max(0, Math.ceil((b.deadline - now) / 1000))}s`;
  const acceptBtn = document.createElement("button");
  acceptBtn.className = "btn small primary";
  acceptBtn.textContent = "接收所选";
  const rejectBtn = document.createElement("button");
  rejectBtn.className = "btn small danger";
  rejectBtn.textContent = "全部拒绝";
  actions.append(cd, acceptBtn, rejectBtn);
  head.append(arrow, meta, actions);

  // 文件勾选列表（默认全选；全选开关放在列表顶部）
  const filesEl = document.createElement("div");
  filesEl.className = "tf-batch-files";
  const boxes: HTMLInputElement[] = [];
  const allLabel = document.createElement("label");
  allLabel.className = "tf-batch-all";
  const allBox = document.createElement("input");
  allBox.type = "checkbox";
  allBox.checked = true;
  const allText = document.createElement("span");
  allText.textContent = "全选";
  allLabel.append(allBox, allText);
  filesEl.appendChild(allLabel);
  b.files.forEach((f) => {
    const label = document.createElement("label");
    label.className = "tf-batch-file";
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = true;
    const nameSpan = document.createElement("span");
    nameSpan.className = "tf-batch-name";
    nameSpan.textContent = f.name; // 对端传来的文件名，必须走 textContent 防注入
    const sizeSpan = document.createElement("span");
    sizeSpan.className = "tf-batch-size";
    sizeSpan.textContent = fmtSize(f.size);
    label.append(box, nameSpan, sizeSpan);
    boxes.push(box);
    filesEl.appendChild(label);
  });
  allBox.onchange = () => {
    for (const bx of boxes) bx.checked = allBox.checked;
  };

  acceptBtn.onclick = () => {
    const accepted = boxes
      .map((bx, i) => (bx.checked ? i : -1))
      .filter((i) => i >= 0);
    respondBatch(b.request_id, accepted);
  };
  rejectBtn.onclick = () => respondBatch(b.request_id, []);

  row.append(head, filesEl);
  return row;
}

function respondRecv(requestId: string, accept: boolean, remember = false) {
  pendingRecvs.delete(requestId);
  renderPendingRecvs();
  invoke("transfer_respond_recv", { requestId, accept, remember }).catch(() => {});
}

function respondBatch(requestId: string, accepted: number[]) {
  pendingBatches.delete(requestId);
  renderPendingRecvs();
  invoke("transfer_respond_batch", { requestId, accepted }).catch(() => {});
}

// 每秒刷新倒计时并移除已超时的行（后端同时已自动拒绝，无需再答复）
function ensurePendingTimer() {
  if (pendingTimer !== undefined) return;
  pendingTimer = window.setInterval(() => {
    const now = Date.now();
    for (const [id, p] of pendingRecvs) {
      if (now >= p.deadline) pendingRecvs.delete(id);
    }
    for (const [id, b] of pendingBatches) {
      if (now >= b.deadline) pendingBatches.delete(id);
    }
    renderPendingRecvs();
    if (!pendingRecvs.size && !pendingBatches.size && pendingTimer !== undefined) {
      window.clearInterval(pendingTimer);
      pendingTimer = undefined;
    }
  }, 1000);
}

listen<TransferIncoming>("transfer-incoming", (e) => {
  pendingRecvs.set(e.payload.request_id, {
    ...e.payload,
    deadline: Date.now() + RECV_CONFIRM_TIMEOUT_MS,
  });
  ensurePendingTimer();
  renderPendingRecvs();
});

listen<TransferBatchIncoming>("transfer-batch-incoming", (e) => {
  pendingBatches.set(e.payload.request_id, {
    ...e.payload,
    deadline: Date.now() + BATCH_CONFIRM_TIMEOUT_MS,
  });
  ensurePendingTimer();
  renderPendingRecvs();
});

$("#tf-accept-all").addEventListener("click", () => {
  for (const [id, b] of [...pendingBatches]) {
    respondBatch(id, b.files.map((_, i) => i));
  }
  for (const id of [...pendingRecvs.keys()]) respondRecv(id, true);
});
$("#tf-reject-all").addEventListener("click", () => {
  for (const id of [...pendingBatches.keys()]) respondBatch(id, []);
  for (const id of [...pendingRecvs.keys()]) respondRecv(id, false);
});

// ---------- 事件绑定 ----------

$("#tf-refresh").addEventListener("click", async () => {
  const btn = $<HTMLButtonElement>("#tf-refresh");
  if (btn.disabled) return;
  btn.disabled = true;
  btn.textContent = "扫描中…";
  await sweepDevices();
  btn.textContent = "✓ 已扫描";
  window.setTimeout(() => {
    btn.disabled = false;
    btn.textContent = "刷新";
  }, 1000);
});

$("#tf-open-settings").addEventListener("click", () => {
  invoke("open_settings").catch(() => {});
});

$("#tf-pick").addEventListener("click", () => {
  if (!selectedDeviceId) {
    alertDialog("请先在上方选择一台要发送到的设备（或输入对方 IP）");
    return;
  }
  pickAndSend({ deviceId: selectedDeviceId });
});

$("#tf-send-ip").addEventListener("click", () => {
  const ip = $<HTMLInputElement>("#tf-manual-ip").value.trim();
  if (!ip) {
    alertDialog("请先输入对方 IP 地址");
    $<HTMLInputElement>("#tf-manual-ip").focus();
    return;
  }
  pickAndSend({ ip });
});

// 回车提交 IP 输入框
$<HTMLInputElement>("#tf-manual-ip").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $<HTMLButtonElement>("#tf-send-ip").click();
});

$("#tf-auto-accept").addEventListener("change", async (e) => {
  const checked = (e.target as HTMLInputElement).checked;
  await invoke("lan_update_settings", { patch: { transfer_auto_accept: checked } });
});

// 搜索框输入即时过滤
$<HTMLInputElement>("#tf-search").addEventListener("input", applyHistoryFilter);

// 打开接收文件的保存目录（目录路径由后端解析，含系统下载目录回落）
$("#tf-open-dir").addEventListener("click", async () => {
  try {
    await invoke("transfer_open_dir");
  } catch (e) {
    alertDialog(String(e));
  }
});

$("#tf-clear").addEventListener("click", async () => {
  const items = await invoke<TransferDto[]>("transfer_history");
  if (!items.length) return;
  const c = await choiceDialog(`清空全部 ${items.length} 条传输记录？`, [
    { value: "cancel", text: "取消" },
    { value: "record", text: "仅删除记录" },
    { value: "file", text: "连同文件一起删除", kind: "danger" },
  ]);
  if (!c || c === "cancel") return;
  await invoke("transfer_clear_history", { deleteFile: c === "file" });
  refreshHistory();
});

// 拖拽文件到窗口发送
getCurrentWebviewWindow()
  .onDragDropEvent((e) => {
    const drop = $("#tf-drop");
    if (e.payload.type === "over") {
      drop.classList.add("active");
    } else if (e.payload.type === "leave") {
      drop.classList.remove("active");
    } else if (e.payload.type === "drop") {
      drop.classList.remove("active");
      const ip = $<HTMLInputElement>("#tf-manual-ip").value.trim();
      if (selectedDeviceId) {
        sendPaths(e.payload.paths, { deviceId: selectedDeviceId });
      } else if (ip) {
        sendPaths(e.payload.paths, { ip });
      } else {
        alertDialog("请先选择一台要发送到的设备（或输入对方 IP）");
      }
    }
  })
  .catch(() => {});

// 传输进度（发送与接收共用）
listen<TransferProgress>("transfer-progress", (e) => {
  const p = e.payload;
  showProgress(true);
  // 发送文件列表：按文件名定位当前项，之前的标记完成，完成帧把当前项也标记完成
  if (p.direction === "send" && sendItems.length) {
    const idx = sendItems.findIndex((f) => f.name === p.name && f.state !== "done");
    if (idx >= 0) {
      for (let i = 0; i < idx; i++) {
        if (sendItems[i].state === "pending") sendItems[i].state = "done";
      }
      sendItems[idx].state = p.total > 0 && p.sent >= p.total ? "done" : "sending";
      renderSendList();
    }
  }
  const pct = p.total > 0 ? Math.min(100, Math.round((p.sent / p.total) * 100)) : 0;
  $<HTMLDivElement>("#tf-progress-fill").style.width = `${pct}%`;
  const verb = p.direction === "send" ? "发送到" : "接收自";
  const idx = p.count > 1 ? `（第 ${p.index}/${p.count} 个）` : "";
  $("#tf-progress-text").textContent =
    `${verb}「${p.peer}」：${p.name}${idx} — ${fmtSize(p.sent)} / ${fmtSize(p.total)}（${pct}%）`;
  // 接收方完成的进度条停一下再隐藏（发送方由 sendPaths 的 finally 统一收尾）
  if (p.direction === "recv" && p.total > 0 && p.sent >= p.total) {
    window.setTimeout(() => showProgress(false), 1500);
  }
});

// 传输完成/失败 → 刷新记录
listen("transfer-changed", refreshHistory);

// 设备上下线 → 刷新设备列表
listen("lan-state-changed", refreshDevices);

// 窗口可见期间：每 3s 刷新在线状态（beacon 实时发现），每 10s 主动深度扫描一次
// （在线判定窗口为 12s，10s 重扫可让不开广播的设备也保持在线状态不闪烁）
window.setInterval(refreshDevices, 3000);
window.setInterval(sweepDevices, 10000);

// 主题 / 窗口效果变化时即时同步
listen<AppConfig>("config-changed", (e) => {
  applyAppearance(e.payload);
  applyWindowEffect(e.payload.window_effect);
  // 面板背景图：勾选了「应用到互传文件窗口」才在本窗口生效
  applyWindowBackground(e.payload.background, e.payload.background?.apply_transfer ?? false);
});

(async () => {
  const cfg = await loadConfig();
  applyAppearance(cfg);
  applyWindowEffect(cfg.window_effect);
  applyWindowBackground(cfg.background, cfg.background?.apply_transfer ?? false);
  initLockGuard();
  initEscClose(); // ESC 关闭窗口
  refreshDevices();
  refreshHistory();
  sweepDevices(); // 打开窗口立即扫一轮
})();
