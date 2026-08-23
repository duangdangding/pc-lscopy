// 应用内确认弹窗（不用 window.confirm：Tauri 2 中它被改写为异步调用且默认无权限）
export function confirmDialog(message: string): Promise<boolean> {
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

    const cancel = document.createElement("button");
    cancel.className = "btn";
    cancel.textContent = "取消";

    const ok = document.createElement("button");
    ok.className = "btn primary";
    ok.textContent = "确定";

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
    cancel.onclick = () => done(false);
    ok.onclick = () => done(true);
    overlay.onclick = (e) => {
      if (e.target === overlay) done(false);
    };
    document.addEventListener("keydown", onKey, true);

    btns.appendChild(cancel);
    btns.appendChild(ok);
    box.appendChild(msg);
    box.appendChild(btns);
    overlay.appendChild(box);
    document.body.appendChild(overlay);
    ok.focus();
  });
}
