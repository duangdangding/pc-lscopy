import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { applyAppearance, loadConfig } from "./config";

const $ = <T extends HTMLElement>(sel: string) =>
  document.querySelector<T>(sel)!;

interface BlockedDto {
  device_id: string;
  name: string;
  model: string | null;
}

interface LanStateDto {
  blocked: BlockedDto[];
}

function renderBlocked(blocked: BlockedDto[]) {
  const listEl = $<HTMLDivElement>("#blocked-list");
  listEl.innerHTML = "";
  if (!blocked.length) {
    const p = document.createElement("p");
    p.className = "desc";
    p.textContent = "黑名单为空";
    listEl.appendChild(p);
    return;
  }
  for (const b of blocked) {
    const row = document.createElement("div");
    row.className = "lan-device";

    const meta = document.createElement("div");
    meta.className = "lan-meta";
    const title = document.createElement("div");
    title.className = "lan-title";
    title.textContent = b.name || "未知设备";
    const sub = document.createElement("div");
    sub.className = "lan-sub";
    sub.textContent = b.model || b.device_id;
    meta.append(title, sub);

    const actions = document.createElement("div");
    actions.className = "lan-actions";
    const btn = document.createElement("button");
    btn.className = "btn small";
    btn.textContent = "移出黑名单";
    btn.onclick = async () => {
      await invoke("lan_unblock", { deviceId: b.device_id });
      refresh();
    };
    actions.appendChild(btn);

    row.append(meta, actions);
    listEl.appendChild(row);
  }
}

async function refresh() {
  try {
    const s = await invoke<LanStateDto>("lan_get_state");
    renderBlocked(s.blocked);
  } catch {
    /* 后端未就绪时静默 */
  }
}

$("#blocked-refresh").addEventListener("click", refresh);

// 其他窗口操作黑名单 / 设备状态时同步刷新
listen("lan-state-changed", refresh);

(async () => {
  applyAppearance(await loadConfig());
  refresh();
})();
