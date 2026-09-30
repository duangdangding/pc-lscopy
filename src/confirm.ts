// 应用内弹窗组件（不用 window.alert / window.confirm：Tauri 2 中它们被改写为异步调用且默认无权限，
// 且系统弹窗是白色原生样式，与应用主题风格割裂）

interface DialogParts {
  overlay: HTMLDivElement;
  box: HTMLDivElement;
  msg: HTMLElement;
  btns: HTMLDivElement;
}

// 组装通用弹窗骨架：遮罩 + 圆角盒子 + 消息 + 按钮区
// message 传 HTMLElement 时用作结构化内容（自带样式，不再套 confirm-msg 的 pre-wrap）
function buildDialog(message: string | HTMLElement): DialogParts {
  const overlay = document.createElement("div");
  overlay.className = "confirm-overlay";

  const box = document.createElement("div");
  box.className = "confirm-box";

  let msg: HTMLElement;
  if (typeof message === "string") {
    msg = document.createElement("div");
    msg.className = "confirm-msg";
    msg.textContent = message;
  } else {
    msg = message;
  }

  const btns = document.createElement("div");
  btns.className = "confirm-btns";

  box.append(msg, btns);
  overlay.appendChild(box);
  document.body.appendChild(overlay);
  return { overlay, box, msg, btns };
}

// 统一键盘处理：Enter = 确认，Escape / 点遮罩 = 取消
function bindDialogKeys(
  overlay: HTMLDivElement,
  onConfirm: () => void,
  onCancel: () => void
) {
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.stopPropagation();
      e.preventDefault();
      onConfirm();
    } else if (e.key === "Escape") {
      e.stopPropagation();
      e.preventDefault();
      onCancel();
    }
  };
  overlay.onclick = (e) => {
    if (e.target === overlay) onCancel();
  };
  document.addEventListener("keydown", onKey, true);
  return () => document.removeEventListener("keydown", onKey, true);
}

export interface ConfirmOptions {
  /** 确认按钮文案，默认「确定」 */
  okText?: string;
  /** 取消按钮文案，默认「取消」 */
  cancelText?: string;
  /** 确认按钮样式，默认 "primary" */
  okKind?: "primary" | "danger" | "";
  /** 取消按钮样式，默认普通 */
  cancelKind?: "danger" | "";
}

export function confirmDialog(
  message: string | HTMLElement,
  opts: ConfirmOptions = {}
): Promise<boolean> {
  return new Promise((resolve) => {
    const { overlay, btns } = buildDialog(message);

    const cancel = document.createElement("button");
    cancel.className = `btn ${opts.cancelKind ?? ""}`.trim();
    cancel.textContent = opts.cancelText ?? "取消";

    const ok = document.createElement("button");
    ok.className = `btn ${opts.okKind ?? "primary"}`.trim();
    ok.textContent = opts.okText ?? "确定";

    let unbind: () => void;
    const done = (v: boolean) => {
      overlay.remove();
      unbind();
      resolve(v);
    };
    unbind = bindDialogKeys(overlay, () => done(true), () => done(false));
    cancel.onclick = () => done(false);
    ok.onclick = () => done(true);

    btns.append(cancel, ok);
    ok.focus();
  });
}

/** 单按钮提示弹窗：替代原生 alert()，与主题风格一致 */
export function alertDialog(message: string, okText = "知道了"): Promise<void> {
  return new Promise((resolve) => {
    const { overlay, btns } = buildDialog(message);

    const ok = document.createElement("button");
    ok.className = "btn primary";
    ok.textContent = okText;

    let unbind: () => void;
    const done = () => {
      overlay.remove();
      unbind();
      resolve();
    };
    unbind = bindDialogKeys(overlay, done, done);
    ok.onclick = done;

    btns.appendChild(ok);
    ok.focus();
  });
}

export interface ChoiceOption {
  /** 选项值，作为 Promise 的返回结果 */
  value: string;
  text: string;
  /** 按钮样式："primary" | "danger" | ""（默认） */
  kind?: "primary" | "danger" | "";
}

/**
 * 多选一弹窗：返回所选选项的 value；点遮罩 / Esc / 显式「取消」类选项外关闭返回 null。
 * 用于"连同置顶删除 / 只删非置顶 / 取消"这类三分支场景。
 */
export function choiceDialog(
  message: string,
  options: ChoiceOption[]
): Promise<string | null> {
  return new Promise((resolve) => {
    const { overlay, btns } = buildDialog(message);
    // 三个及以上选项时纵向整行排列：长文案按钮横排放不下 320px 弹窗，会溢出
    const stacked = options.length >= 3;
    if (stacked) btns.classList.add("stacked");
    // 纵向排列时「取消」固定沉底（调用方统一把取消放第一位）
    const ordered =
      stacked && options[0]?.value === "cancel"
        ? [...options.slice(1), options[0]]
        : options;

    let unbind: () => void;
    const done = (v: string | null) => {
      overlay.remove();
      unbind();
      resolve(v);
    };
    // Enter 触发当前聚焦的选项按钮；焦点不在按钮上时触发第一个选项
    const onConfirm = () => {
      const focused = document.activeElement;
      const btn =
        focused instanceof HTMLButtonElement && btns.contains(focused)
          ? focused
          : btns.querySelector<HTMLButtonElement>("button");
      btn?.click();
    };
    unbind = bindDialogKeys(overlay, onConfirm, () => done(null));

    for (const opt of ordered) {
      const b = document.createElement("button");
      b.className = `btn ${opt.kind ?? ""}`.trim();
      b.textContent = opt.text;
      b.onclick = () => done(opt.value);
      btns.appendChild(b);
    }
    btns.querySelector<HTMLButtonElement>("button")?.focus();
  });
}
