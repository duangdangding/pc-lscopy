# AGENTS.md

本文件面向在本仓库中工作的 AI 编程助手，说明项目结构、常用命令与约定。

## 项目简介

**lscopy（共享剪贴板）**——一个 Windows 桌面剪贴板管理工具，基于 **Tauri 2 + Vanilla TypeScript + Rust**。

- 全局热键唤起剪贴板历史面板，点击/回车即粘贴
- 历史记录持久化在 SQLite（rusqlite，bundled）
- 支持文本、图片（含 CF_BITMAP/CF_DIB 截图软件兼容）、文件等类型，列表页按类型分标签页
- 面板无边框：工具栏/底栏空白处可拖动，右缘/下缘/右下角可调大小
- 📌 钉住桌面：失焦/粘贴不自动隐藏，可连续粘贴多条
- 局域网多设备同步：与安卓端 ClipDitto 及其他电脑互相同步剪贴板（设置页「局域网同步」标签）
- 可选「记住窗口大小」：重启后恢复上次调整的长宽
- 系统托盘、开机自启、单实例、静默启动

## 技术栈

| 层 | 技术 |
|---|---|
| 前端 | Vite 6 + TypeScript 5.6（无框架，原生 DOM） |
| 后端 | Rust（Tauri 2），edition 2021 |
| 包管理 | **bun**（锁定文件为 `bun.lock`，不要用 npm/yarn/pnpm） |
| 数据 | SQLite（rusqlite bundled）、`arboard` 剪贴板、`enigo` 模拟粘贴 |

## 目录结构

```
index.html          主窗口页面（剪贴板历史面板，无边框/置顶/默认隐藏）
settings.html       设置窗口页面
blocked.html        黑名单管理窗口页面（局域网同步拉黑设备的独立管理页）
src/
  main.ts           主窗口前端逻辑（列表渲染、类型标签页、粘贴交互）
  settings.ts       设置页逻辑（热键、自启、数据库目录、局域网同步等）
  blocked.ts        黑名单窗口逻辑（列表 + 移出黑名单）
  config.ts         前端共享配置
  confirm.ts        确认对话框组件（confirmDialog 二选一 / choiceDialog 多选一）
  styles.css        全局样式
src-tauri/
  src/lib.rs        后端主体（约 1900 行）：剪贴板监听、SQLite 存取、
                    托盘菜单、全局热键、窗口控制、模拟粘贴
  src/lan.rs        局域网多设备同步（约 2200 行）：HTTP 服务（8765）、
                    UDP beacon 发现（8766 / 组播 239.255.60.60:8767）、
                    配对码鉴权（支持对方开「自动同意」时免码配对）、
                    增量同步客户端、黑名单
  src/main.rs       入口（仅调用 lib）
  capabilities/     Tauri 权限声明（windows 列表需包含新增窗口 label）
  tauri.conf.json   窗口/打包配置（identifier: com.administrator.lscopy）
vite.config.ts      多页面构建配置（index + settings + blocked）
.github/workflows/  CI / 发布流程
```

## 常用命令

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

## 代码约定

- **前端**：原生 TypeScript，无框架；DOM 操作为主，保持与现有 `main.ts` / `settings.ts` 风格一致；不引入 UI 框架或新依赖，除非确有必要。
- **后端**：功能集中在 `src-tauri/src/lib.rs`，按 `// ---------- xxx ----------` 注释分区；新增 Tauri command 需同步在 `capabilities/default.json` 中放行（如适用）。
- **注释与 UI 文案**：中文。
- **配置持久化**：`AppConfig`（serde）+ SQLite，新增配置字段注意 `#[serde(default)]` 向后兼容。
- **粘贴链路敏感**：粘贴提速、焦点等待时间（当前 50ms）等时序参数改动需在真机验证，勿随意调大/调小。

## 验证方式

- 前端改动：`bun run build` 通过 tsc 检查。
- 后端改动：`cargo check` / `cargo clippy` 通过后，`bun run tauri dev` 真机验证热键唤起、粘贴、托盘菜单。
- 本项目无自动化测试，以手动验证为主。

## 注意事项

- **局域网同步**：协议与安卓端 ClipDitto 对齐（端口 8765、beacon 8766/8767、6 位配对码、
  X-Token 头鉴权、自动反向配对）。同步设置（LanSettings）已合并进统一配置文件
  `lscopy-config.json`（顶层 `lan` 键），不再单独持久化；旧版独立 `lscopy-lan.json`
  会在首次启动时读取并改名为 `.bak`。线上时间戳为**毫秒**（安卓端口径），库内
  `created_at` 仍是秒，出入线时在 `lan.rs` 换算。
  环回防护靠 `clips.remote_device_id` / `remote_id` 两列；文件类记录（本机路径）不对其他设备同步。
  - mac 适配：默认设备名取 `scutil --get ComputerName`；本机 IP 与子网广播地址通过
    `if-addrs` 枚举网卡获得（mac 上 UDP connect 8.8.8.8 技巧不可靠）；组播按接口逐个加入。
  - 免码配对：`/info` 暴露 `autoAccept` 字段；对方开「自动同意配对」时 `/pair` 免配对码，
    成功响应附带本机配对码（`{"result":"ok","token":…}`），请求方存下供后续 `/clips` 鉴权。
  - 同步页前端每 2s 轮询重建设备列表：配对表单展开期间（`pairingDeviceId` 非空）必须跳过
    列表重建，否则输入框会被刷掉；新增实时刷新类 UI 时注意同样的坑。
- **批量删除三选一**：范围内有置顶记录时用 `choiceDialog` 提供「取消 / 只删非置顶 / 连同置顶删除」，
  不要退回二选一弹窗（取消语义会被占用）。
- **构建必须走 Tauri CLI**（`bun run tauri build` / `tauri dev`），不要裸 `cargo build --release`：CLI 会开启 `custom-protocol` 特性并正确处理前端资源协议，裸 cargo 构建的 exe 会显示"无法访问页面"。
- Windows 为主要目标平台；`winreg` 仅 Windows 编译（`cfg(windows)`）。
- 剪贴板图片读取有 Windows 原生兜底逻辑（CF_BITMAP/CF_DIB），改动相关代码时注意不要回归截图软件兼容性。
- 配置统一保存在 `lscopy-config.json`（顶层 `app` + `lan` 两键），默认在 **exe 同目录**（便携模式）；
  实际目录由 exe 同目录的指针文件 `lscopy-config-dir.txt` 决定（设置页「配置文件」可自定义，
  改动时询问是否迁移旧文件）。数据库 `lscopy.db` 默认也在 exe 同目录；旧版系统配置目录
  （`%APPDATA%`）的配置会在首次启动时自动迁移。落盘统一走 `persist_config`（锁顺序固定 config → lan.settings → config_file）。
- 主窗口失焦自动隐藏是**延迟 150ms 复查**实现的（拖动/缩放会造成瞬时失焦）；改动窗口事件逻辑时注意 `dragging` / `panel_pinned` / `main_focused` 三个状态。
- 版本号需同步修改 `package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json` 三处（`Cargo.lock` 随构建自动更新）。
- 发布流程：推 `v*` tag 触发 `.github/workflows/release.yml`，Windows + macOS 构建并生成 draft release。
- `dist/`、`target/`、`node_modules/` 为构建产物，不要提交或编辑。
