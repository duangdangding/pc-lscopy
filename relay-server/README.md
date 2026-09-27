# lscopy 中继服务器（relay-server）

lscopy「共享剪贴板」的自建中继：设备不在同一局域网时，各自与本服务器保持
WebSocket 长连接，由服务器在同一**分组**内转发剪贴板条目。
设计文档见仓库根目录 `docs/relay-sync-design.md`。

当前进度 **M3**：WSS 接入 + 强制鉴权 + 分组转发 + peers 广播 + 离线暂存补拉。
不含端到端加密（M4，服务器无需感知加密内容）。

## 运行方式

### 使用预构建镜像（推荐，无需 Rust / 无需克隆仓库）

推 `v*` tag 时 CI 会自动构建并推送多架构镜像（`linux/amd64` + `linux/arm64`）到 GHCR：

```bash
docker pull ghcr.io/duangdangding/lscopy-relay:latest
docker run -d -p 8780:8780 \
  -e RELAY_ACCESS_KEYS="你的强随机密钥" \
  -v $(pwd)/data:/data --restart unless-stopped \
  ghcr.io/duangdangding/lscopy-relay:latest
```

或直接 `docker compose up -d`（本目录的 `docker-compose.yml` 默认使用预构建镜像）。
可用标签：`vX.Y.Z`（精确版本）/ `X.Y` / `latest`。

> 首次推送后需在 GitHub 仓库的 Packages 页面把 `lscopy-relay` 包设为 **Public**，
> 否则他人拉取需要登录。

### 手动运行

```bash
cargo build --release
./target/release/relay-server --port 8780 --access-key "你的强随机密钥"
```

### Docker 运行（从源码构建）

把 `docker-compose.yml` 里的 `image:` 注释掉、启用 `build: .`，然后：

```bash
docker compose up -d --build
# 或
docker build -t lscopy-relay .
docker run -d -p 8780:8780 -e RELAY_ACCESS_KEYS="你的强随机密钥" \
  -v $(pwd)/data:/data --restart unless-stopped lscopy-relay
```

### TLS（wss://）

服务器本身只监听明文 WS。公网部署时建议前面套一层 nginx / Caddy 反代终结 TLS，
客户端连接 `wss://你的域名/ws`。

## 配置（优先级：环境变量 < 配置文件 < 命令行）

| 项 | 环境变量 | 配置文件键 | 命令行 | 默认 |
|---|---|---|---|---|
| 监听端口 | `RELAY_PORT` | `port` | `--port` | `8780` |
| 接入密钥（必填，多个） | `RELAY_ACCESS_KEYS`（逗号分隔） | `access_keys`（数组） | `--access-key`（可重复） | 无 |
| 数据目录 | `RELAY_DATA_DIR` | `data_dir` | `--data-dir` | `data` |
| 单 IP 连接上限 | `RELAY_MAX_CONN_PER_IP` | `max_conn_per_ip` | `--max-conn-per-ip` | `20` |

配置文件默认路径 `relay-server.json`（可用 `RELAY_CONFIG` 或 `--config` 指定），示例：

```json
{
  "port": 8780,
  "access_keys": ["key-a", "key-b"],
  "data_dir": "data",
  "max_conn_per_ip": 20
}
```

**未配置接入密钥时拒绝启动**——鉴权是强制的，不存在"谁都能连"的模式。

## 协议（M1）

端点：`GET /ws`（WebSocket 升级）；`GET /healthz` 健康检查。

鉴权握手（连接建立后第一步，5 秒超时，不过即断）：

1. 服务器 → `{ "op": "challenge", "nonce": "<64位hex>" }`
2. 客户端 → `{ "op": "hello", "deviceId": "...", "name": "...", "group": "...",
   "ts": <毫秒时间戳>, "auth": "<hex(HMAC-SHA256(接入密钥, nonce+deviceId+ts))>" }`
   - `ts` 与服务器时间偏差超过 ±5 分钟视为重放，拒绝。
3. 服务器 → `{ "op": "welcome", ... }` 成功 / `{ "op": "error", "code": "auth_failed" }` 失败并断开。

鉴权通过后：

| 方向 | 消息 | 说明 |
|---|---|---|
| 客户端 → 服务器 | `{ "op": "push", "clip": {...} }` | 发送剪贴板条目；暂存后转发给分组内其他成员 |
| 客户端 → 服务器 | `{ "op": "pull", "sinceSeq": 456 }` | 补拉离线期间错过的条目（连接成功后发一次） |
| 客户端 → 服务器 | `{ "op": "ping" }` | 心跳，回 `{ "op": "pong", "ts": ... }` |
| 服务器 → 客户端 | `{ "op": "clip", "seq": 457, "clip": {...} }` | 分组内条目（实时或补拉），`seq` 为服务器分配的单调递增序号 |
| 服务器 → 客户端 | `{ "op": "acked", "remoteId": ..., "seq": 457 }` | push 送达确认，`seq` 供发送方推进自己的补拉游标 |
| 服务器 → 客户端 | `{ "op": "peers", "devices": [...] }` | 分组名单（成员变化时广播） |

`clip` 字段与 lscopy 现有 LAN 同步线上口径一致（`type` / `text` / `timestamp`（毫秒）/
`remoteDeviceId` / `remoteId` / `fileName` / `fileSize`）。

### 离线暂存

- 每条 `push` 落 SQLite 队列（`数据目录/relay-queue.db`，按分组隔离），分配 `seq`。
- TTL 7 天（按服务器接收时间），每组上限 5000 条，超限自动淘汰最旧。
- 客户端持久化 `last_seq` 游标，连接成功后 `pull` 补拉；补拉与实时推送撞车的重复
  由客户端内容哈希查重兜住。

## 防护

- 鉴权失败按 IP 计数：连续 5 次失败封禁 10 分钟。
- hello 5 秒超时强制断开。
- 单 IP 并发连接数上限（默认 20）。
- HMAC 常量时间比较；密钥从不上线传输。
