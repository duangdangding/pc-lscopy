import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { open } from "@tauri-apps/plugin-dialog";
import { applyAppearance, loadConfig } from "./config";

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
}

interface LanStateDto {
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
}

// ---------- 状态 ----------

let selectedDeviceId: string | null = null;
let sending = false;
let sweeping = false;
let devices: LanDeviceDto[] = [];

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
  // 只显示实时扫描到的在线设备：离线/历史配对记录不出现在列表里
  const shown = devices.filter((d) => d.online);
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
  pickBtn.disabled = true;
  showProgress(true);
  try {
    const msg = target.ip
      ? await invoke<string>("transfer_send_ip", { ip: target.ip, paths })
      : await invoke<string>("transfer_send", { deviceId: target.deviceId, paths });
    alert(msg);
  } catch (e) {
    alert(`发送失败: ${e}`);
  } finally {
    sending = false;
    pickBtn.disabled = false;
    showProgress(false);
    refreshHistory();
  }
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
  }
}

// ---------- 传输记录 ----------

function renderHistory(items: TransferDto[]) {
  const listEl = $<HTMLDivElement>("#tf-history");
  listEl.innerHTML = "";
  if (!items.length) {
    const p = document.createElement("p");
    p.className = "desc";
    p.textContent = "暂无记录";
    listEl.appendChild(p);
    return;
  }
  for (const t of items) {
    const row = document.createElement("div");
    row.className = "lan-device";

    const arrow = document.createElement("span");
    arrow.className = "tf-arrow " + (t.direction === "send" ? "send" : "recv");
    arrow.textContent = t.direction === "send" ? "⬆" : "⬇";
    arrow.title = t.direction === "send" ? "已发送" : "已接收";

    const meta = document.createElement("div");
    meta.className = "lan-meta";
    const title = document.createElement("div");
    title.className = "lan-title";
    title.textContent = t.file_name;
    if (!t.ok) {
      const badge = document.createElement("span");
      badge.className = "lan-badge off";
      badge.textContent = "失败";
      title.appendChild(badge);
    }
    const sub = document.createElement("div");
    sub.className = "lan-sub";
    sub.textContent =
      `${t.direction === "send" ? "发给" : "来自"} ${t.peer} · ${fmtSize(t.size)} · ${fmtTime(t.created_at)}` +
      (t.ok ? "" : ` · ${t.msg || "未知错误"}`);
    meta.append(title, sub);

    row.append(arrow, meta);
    // 成功记录可定位到文件（发送的为源文件，接收的为保存位置）
    if (t.ok && t.path) {
      const actions = document.createElement("div");
      actions.className = "lan-actions";
      const btn = document.createElement("button");
      btn.className = "btn small";
      btn.textContent = "打开位置";
      btn.onclick = async () => {
        try {
          await invoke("transfer_reveal", { path: t.path });
        } catch (e) {
          alert(String(e));
        }
      };
      actions.appendChild(btn);
      row.appendChild(actions);
    }
    listEl.appendChild(row);
  }
}

async function refreshHistory() {
  try {
    renderHistory(await invoke<TransferDto[]>("transfer_history"));
  } catch {
    /* 后端未就绪时静默 */
  }
}

// ---------- 接收确认（手动模式） ----------

// 接收/拒绝 弹窗（confirmDialog 按钮文案固定为 确定/取消，这里需要 接收/拒绝）
function recvConfirmDialog(message: string): Promise<boolean> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "confirm-overlay";
    const box = document.createElement("div");
    box.className = "confirm-box";
    const msg = document.createElement("div");
    msg.className = "confirm-msg";
    msg.textContent = message;
    const btns = document.createElement("div");
    btns.className = "confirm-btns";
    const reject = document.createElement("button");
    reject.className = "btn danger";
    reject.textContent = "拒绝";
    const accept = document.createElement("button");
    accept.className = "btn primary";
    accept.textContent = "接收";
    const done = (v: boolean) => {
      overlay.remove();
      document.removeEventListener("keydown", onKey, true);
      resolve(v);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Enter") {
        e.stopPropagation();
        e.preventDefault();
        done(true);
      } else if (e.key === "Escape") {
        e.stopPropagation();
        e.preventDefault();
        done(false);
      }
    };
    reject.onclick = () => done(false);
    accept.onclick = () => done(true);
    btns.append(reject, accept);
    box.append(msg, btns);
    overlay.appendChild(box);
    document.body.appendChild(overlay);
    document.addEventListener("keydown", onKey, true);
    accept.focus();
  });
}

// 多个设备同时发来时排队逐个确认
const recvQueue: TransferIncoming[] = [];
let recvDialogOpen = false;

async function processRecvQueue() {
  if (recvDialogOpen) return;
  const req = recvQueue.shift();
  if (!req) return;
  recvDialogOpen = true;
  try {
    const accept = await recvConfirmDialog(
      `设备「${req.peer}」（${req.host}）想向你发送文件：\n\n${req.name}（${fmtSize(req.size)}）\n\n是否接收？`
    );
    await invoke("transfer_respond_recv", { requestId: req.request_id, accept });
  } finally {
    recvDialogOpen = false;
    processRecvQueue();
  }
}

listen<TransferIncoming>("transfer-incoming", (e) => {
  recvQueue.push(e.payload);
  processRecvQueue();
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
    alert("请先在上方选择一台要发送到的设备（或输入对方 IP）");
    return;
  }
  pickAndSend({ deviceId: selectedDeviceId });
});

$("#tf-send-ip").addEventListener("click", () => {
  const ip = $<HTMLInputElement>("#tf-manual-ip").value.trim();
  if (!ip) {
    alert("请先输入对方 IP 地址");
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

$("#tf-clear").addEventListener("click", async () => {
  await invoke("transfer_clear_history");
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
        alert("请先选择一台要发送到的设备（或输入对方 IP）");
      }
    }
  })
  .catch(() => {});

// 传输进度（发送与接收共用）
listen<TransferProgress>("transfer-progress", (e) => {
  const p = e.payload;
  showProgress(true);
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

(async () => {
  applyAppearance(await loadConfig());
  refreshDevices();
  refreshHistory();
  sweepDevices(); // 打开窗口立即扫一轮
})();
