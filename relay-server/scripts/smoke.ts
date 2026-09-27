// relay-server 冒烟测试（M1 鉴权/转发 + M3 离线暂存补拉）
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

// 连接并完成鉴权；pullSeq 提供时在 welcome 后自动发 pull 补拉
function openAuthed(deviceId: string, name: string, pullSeq?: number): Promise<WebSocket> {
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
        if (pullSeq !== undefined) {
          ws.send(JSON.stringify({ op: "pull", sinceSeq: pullSeq }));
        }
        resolve(ws);
      } else if (msg.op === "error") {
        reject(new Error(`auth error: ${msg.code}`));
      }
    };
    ws.onerror = () => reject(new Error("connect error"));
  });
}

// 等待下一条指定 op 的消息（跳过 peers/pong 等其他消息）
function waitOp(ws: WebSocket, op: string, timeoutMs = 3000): Promise<any> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`timeout waiting op=${op}`)), timeoutMs);
    const handler = (e: MessageEvent) => {
      const msg = JSON.parse(String(e.data));
      if (msg.op === op) {
        clearTimeout(timer);
        ws.removeEventListener("message", handler);
        resolve(msg);
      }
    };
    ws.addEventListener("message", handler);
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

// 2) A 入网时 B 不在线：push 的条目应被暂存，B 后上线 pull 能补到
const a = await openAuthed("dev-a", "电脑A");
const clip = {
  type: 0,
  text: "中继冒烟测试",
  timestamp: Date.now(),
  remoteDeviceId: "dev-a",
  remoteId: 42,
};
const ackPromise = waitOp(a, "acked");
a.send(JSON.stringify({ op: "push", clip }));
const ack = await ackPromise;
check("A 收到 acked（remoteId=42）", ack.remoteId === 42);
check("acked 携带服务器 seq", typeof ack.seq === "number" && ack.seq >= 1);

// B 后上线并补拉 sinceSeq=0：应收到暂存的条目
const b = await openAuthed("dev-b", "手机B", 0);
const got = await waitOp(b, "clip");
check("B 补拉到 A 离线期间 push 的 clip", got.clip?.text === "中继冒烟测试");
check("补拉 clip 携带 seq", typeof got.seq === "number" && got.seq === ack.seq);
check("来源设备保留（remoteDeviceId=dev-a）", got.clip?.remoteDeviceId === "dev-a");

// 3) 实时转发：B 在线时 A 再 push，B 直接收到（不走补拉）
const clip2 = { ...clip, text: "实时条目", remoteId: 43, timestamp: Date.now() };
const livePromise = waitOp(b, "clip");
const ack2Promise = waitOp(a, "acked");
a.send(JSON.stringify({ op: "push", clip: clip2 }));
const [live, ack2] = await Promise.all([livePromise, ack2Promise]);
check("B 实时收到 A 的第二条 clip", live.clip?.text === "实时条目" && live.seq === ack2.seq);

// 4) ping/pong
const pongPromise = waitOp(b, "pong");
b.send(JSON.stringify({ op: "ping" }));
await pongPromise;
check("ping/pong 心跳", true);

a.close();
b.close();

console.log(failures === 0 ? "\n全部通过" : `\n${failures} 项失败`);
process.exit(failures === 0 ? 0 : 1);
