# lscopy（共享剪贴板-卢）

一个 Windows 桌面共享剪贴板管理工具，基于 **Tauri 2 + Vanilla TypeScript + Rust**。

## 功能特性

### 剪贴板历史

- 全局热键唤起剪贴板历史面板，点击 / 回车即粘贴
- 支持文本、图片（含 CF_BITMAP / CF_DIB 截图软件兼容）、文件等类型，列表页按类型分标签页
- 历史记录持久化在 SQLite（rusqlite，bundled）
- 面板无边框：工具栏 / 底栏空白处可拖动，右缘 / 下缘 / 右下角可调大小
- 📌 钉住桌面：失焦 / 粘贴不自动隐藏，可连续粘贴多条
- 可选「记住窗口大小」：重启后恢复上次调整的长宽
- 系统托盘、开机自启、单实例、静默启动

### 局域网多设备同步

- 与安卓端 ClipDitto 及其他电脑互相同步剪贴板（设置页「局域网同步」标签）
- HTTP 服务（8765）+ UDP beacon 设备发现（8766 / 组播 239.255.60.60:8767）
- 6 位配对码鉴权（支持对方开「自动同意」时免码配对）
- 黑名单管理（独立窗口）

### 局域网互传文件

- 独立窗口（托盘菜单「互传文件」/ 同步页设备卡按钮进入）
- 免配对直发，支持手动输入 IP
- 接收默认需手动确认，可开「自动接收」；保存路径沿用同步的「文件存储路径」
- 已配对设备传输加密

### 应用内更新

- 安装版走 Tauri 官方 updater（NSIS 装回原目录）
- 便携版走自研链路：流式下载 → SHA-256 校验 → 替换 exe 并重启

### 云端中继同步（开发中，M3）

- 自建中继服务器（`relay-server/`），设备不在同一局域网时也能互相同步剪贴板
- 客户端通道已打通：设置页「设备同步」标签的中继区块，填服务器地址 / 分组 ID / 接入密钥即连
- WebSocket 长连接 + 强制鉴权（挑战-响应 HMAC，未过鉴权直接断连），断线自动重连
- 离线暂存：服务器 SQLite 队列暂存 7 天（每组 5000 条上限），设备上线自动补拉错过的条目
- 与局域网同步双通道并存、统一内容哈希去重；当前仅文本走中继，图片/文件仍走局域网
- 服务器支持手动运行与 Docker 部署（推 tag 自动发布 GHCR 多架构镜像）
- 规划中：端到端加密（M4）
- 设计文档：`docs/relay-sync-design.md`

## 技术栈

| 层 | 技术 |
|---|---|
| 前端 | Vite 6 + TypeScript 5.6（无框架，原生 DOM） |
| 后端 | Rust（Tauri 2），edition 2021 |
| 包管理 | **bun**（锁定文件为 `bun.lock`，不要用 npm / yarn / pnpm） |
| 数据 | SQLite（rusqlite bundled）、`arboard` 剪贴板、`enigo` 模拟粘贴 |

## 目录结构

```
index.html          主窗口页面（剪贴板历史面板，无边框/置顶/默认隐藏）
settings.html       设置窗口页面
blocked.html        黑名单管理窗口页面
transfer.html       互传文件窗口页面
src/
  main.ts           主窗口前端逻辑（列表渲染、类型标签页、粘贴交互）
  settings.ts       设置页逻辑（热键、自启、数据库目录、局域网同步等）
  blocked.ts        黑名单窗口逻辑（列表 + 移出黑名单）
  transfer.ts       互传文件窗口逻辑（实时扫描、发送、接收确认队列、传输记录）
  config.ts         前端共享配置
  confirm.ts        确认对话框组件（confirmDialog 二选一 / choiceDialog 多选一）
  styles.css        全局样式
src-tauri/
  src/lib.rs        后端主体：剪贴板监听、SQLite 存取、托盘菜单、全局热键、窗口控制、模拟粘贴
  src/lan.rs        局域网多设备同步：HTTP 服务、UDP beacon 发现、配对码鉴权、增量同步、黑名单
  src/relay.rs      云端中继客户端：WSS 连接、挑战-响应鉴权、push 发送、clip 入库、断线重连
  src/lan/transfer.rs  局域网互传文件：接收端（流式落盘、手动/自动确认）、发送端（免配对、IP 直发、配对时加密）
  src/main.rs       入口（仅调用 lib）
  capabilities/     Tauri 权限声明
  tauri.conf.json   窗口/打包配置（identifier: com.lsh.lscopy）
relay-server/       自建中继服务器（M1 骨架）：WSS 接入、挑战-响应鉴权、分组转发，
                    Dockerfile / docker-compose.yml，scripts/smoke.ts 冒烟测试
docs/relay-sync-design.md  中继 + 局域网双通道同步设计文档
vite.config.ts      多页面构建配置（index + settings + blocked + transfer）
.github/workflows/  CI / 发布流程
```

## 开发

```bash
bun install            # 安装依赖
bun run dev            # 仅起 Vite 前端（http://localhost:1420）
bun run build          # tsc 类型检查 + Vite 构建（提交前必跑）
bun run tauri dev      # 启动完整桌面应用（前端 + Rust 后端）
bun run tauri build    # 打包发布产物
```

Rust 侧（在 `src-tauri/` 下）：

```bash
cargo check            # 快速类型检查
cargo clippy           # lint
```

## 打包与发布

- 构建必须走 Tauri CLI（`bun run tauri build`），不要裸 `cargo build --release`。
- 版本号需同步修改 `package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json` 三处。
- 推 `v*` tag 触发 `.github/workflows/release.yml`，Windows + macOS 并行构建，产物包含安装包、便携版、macOS dmg / app.tar.gz、updater 签名与 SHA-256 校验文件。
- 配置统一保存在 `lscopy-config.json`（顶层 `app` + `lan` 两键），默认在 exe 同目录（便携模式）；数据库 `lscopy.db` 默认也在 exe 同目录。

## 环境要求

- Windows（主要目标平台；macOS 有适配）
- [bun](https://bun.sh/)、Rust 工具链（edition 2021）

## 相关项目

- ClipDitto — 安卓端同步客户端
