# AGENTS.md

本文件面向在本仓库中工作的 AI 编程助手，说明项目结构、常用命令与约定。

## 项目简介

**lscopy（剪贴板管家）**——一个 Windows 桌面剪贴板管理工具，基于 **Tauri 2 + Vanilla TypeScript + Rust**。

- 全局热键唤起剪贴板历史面板，点击/回车即粘贴
- 历史记录持久化在 SQLite（rusqlite，bundled）
- 支持文本、图片（含 CF_BITMAP/CF_DIB 截图软件兼容）、文件等类型，列表页按类型分标签页
- 面板无边框：工具栏/底栏空白处可拖动，右缘/下缘/右下角可调大小
- 📌 钉住桌面：失焦/粘贴不自动隐藏，可连续粘贴多条
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
src/
  main.ts           主窗口前端逻辑（列表渲染、类型标签页、粘贴交互）
  settings.ts       设置页逻辑（热键、自启、静默启动、数据库目录等）
  config.ts         前端共享配置
  confirm.ts        确认对话框组件
  styles.css        全局样式
src-tauri/
  src/lib.rs        后端主体（约 1800 行）：剪贴板监听、SQLite 存取、
                    托盘菜单、全局热键、窗口控制、模拟粘贴
  src/main.rs       入口（仅调用 lib）
  capabilities/     Tauri 权限声明
  tauri.conf.json   窗口/打包配置（identifier: com.administrator.lscopy）
vite.config.ts      多页面构建配置（index + settings）
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

- **构建必须走 Tauri CLI**（`bun run tauri build` / `tauri dev`），不要裸 `cargo build --release`：CLI 会开启 `custom-protocol` 特性并正确处理前端资源协议，裸 cargo 构建的 exe 会显示"无法访问页面"。
- Windows 为主要目标平台；`winreg` 仅 Windows 编译（`cfg(windows)`）。
- 剪贴板图片读取有 Windows 原生兜底逻辑（CF_BITMAP/CF_DIB），改动相关代码时注意不要回归截图软件兼容性。
- 配置文件 `lscopy-config.json` 与数据库 `lscopy.db` 默认都在 **exe 同目录**（便携模式）；旧版系统配置目录（`%APPDATA%`）的配置会在首次启动时自动迁移。
- 主窗口失焦自动隐藏是**延迟 150ms 复查**实现的（拖动/缩放会造成瞬时失焦）；改动窗口事件逻辑时注意 `dragging` / `panel_pinned` / `main_focused` 三个状态。
- 版本号需同步修改 `package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json` 三处（`Cargo.lock` 随构建自动更新）。
- 发布流程：推 `v*` tag 触发 `.github/workflows/release.yml`，Windows + macOS 构建并生成 draft release。
- `dist/`、`target/`、`node_modules/` 为构建产物，不要提交或编辑。
