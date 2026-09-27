// relay-server M1 冒烟测试：鉴权拒绝 + 双客户端分组转发
// 用法：bun run scripts/smoke.ts [端口] [接入密钥]
import { createHmac } from "node:crypto";

const port = Number(process.argv[2] ?? 18799);
const key = process.argv[3] ?? "testkey";
const url = `ws://127.0.0.1:${port}/ws`;

let failures = 0;
function check(name: string, cond: boolean) {
  console.log(`${cond ? "PASS" : "FAIL"}  ${name}`);
  if (!cond) failures++;
}

function openAuthed(deviceId: string, name: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    ws.onmessage = (e) => {
      const msg = JSON.parse(String(e.data));
      if (msg.op === "challenge") {
        const ts = Date.now();
        const auth = createHmac("sha256", key)
          .update(`${msg.nonce}${deviceId}${ts}`)
          .digest("hex");
        ws.send(JSON.stringify({ op: "hello", deviceId, name, group: "g1", ts, auth }));
      } else if (msg.op === "welcome") {
        resolve(ws);
      } else if (msg.op === "error") {
        reject(new Error(`auth error: ${msg.code}`));
      }
    };
    ws.onerror = () => reject(new Error("connect error"));
  });
}

function nextMsg(ws: WebSocket, timeoutMs = 3000): Promise<any> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("timeout waiting message")), timeoutMs);
    const prev = ws.onmessage;
    ws.onmessage = (e) => {
      clearTimeout(timer);
      ws.onmessage = prev;
      resolve(JSON.parse(String(e.data)));
    };
  });
}

// 1) 错误密钥应被拒绝
{
  const ws = new WebSocket(url);
  const result = await new Promise<string>((resolve) => {
    ws.onmessage = (e) => {
      const msg = JSON.parse(String(e.data));
      if (msg.op === "challenge") {
        const ts = Date.now();
        const auth = createHmac("sha256", "wrong-key")
          .update(`${msg.nonce}evil${ts}`)
          .digest("hex");
        ws.send(JSON.stringify({ op: "hello", deviceId: "evil", group: "g1", ts, auth }));
      }
      if (msg.op === "error") resolve(msg.code);
    };
    ws.onerror = () => resolve("connect_error");
    setTimeout(() => resolve("timeout"), 3000);
  });
  check("错误密钥被拒绝（auth_failed）", result === "auth_failed");
}

// 2) 两台设备正确鉴权入网，A push → B 收到 clip
const a = await openAuthed("dev-a", "电脑A");
const b = await openAuthed("dev-b", "手机B");

// B 会先收到 peers（A/B 在组内），再收 clip；循环等到 clip 为止
const clipPromise = (async () => {
  for (let i = 0; i < 5; i++) {
    const msg = await nextMsg(b);
    if (msg.op === "clip") return msg;
  }
  throw new Error("未收到 clip");
})();

const clip = {
  type: 1,
  text: "中继冒烟测试",
  timestamp: Date.now(),
  remoteDeviceId: "dev-a",
  remoteId: 42,
};
a.send(JSON.stringify({ op: "push", clip }));

const got = await clipPromise;
check("B 收到 A 的 clip", got.clip?.text === "中继冒烟测试");
check("来源设备保留（remoteDeviceId=dev-a）", got.clip?.remoteDeviceId === "dev-a");

const ack = await nextMsg(a);
check("A 收到 acked（remoteId=42）", ack.op === "acked" && ack.remoteId === 42);

// 3) ping/pong
const pongPromise = nextMsg(b);
b.send(JSON.stringify({ op: "ping" }));
const pong = await pongPromise;
check("ping/pong 心跳", pong.op === "pong");

a.close();
b.close();

console.log(failures === 0 ? "\n全部通过" : `\n${failures} 项失败`);
process.exit(failures === 0 ? 0 : 1);
