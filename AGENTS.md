# AGENTS.md

本文件面向在本仓库中工作的 AI 编程助手，说明项目结构、常用命令与约定。

## 项目简介

**lscopy（共享剪贴板-卢）**——一个 Windows 桌面共享剪贴板管理工具，基于 **Tauri 2 + Vanilla TypeScript + Rust**。

- 全局热键唤起剪贴板历史面板，点击/回车即粘贴
- 历史记录持久化在 SQLite（rusqlite，bundled）
- 支持文本、图片（含 CF_BITMAP/CF_DIB 截图软件兼容）、文件等类型，列表页按类型分标签页
- 面板无边框：工具栏/底栏空白处可拖动，右缘/下缘/右下角可调大小
- 📌 钉住桌面：失焦/粘贴不自动隐藏，可连续粘贴多条
- 局域网多设备同步：与安卓端 ClipDitto 及其他电脑互相同步剪贴板（设置页「局域网同步」标签）
- 局域网互传文件：独立窗口（托盘菜单「互传文件」/ 同步页设备卡按钮进入），免配对直发，
  支持手动输入 IP；接收默认需手动确认，可开「自动接收」；保存路径沿用同步的「文件存储路径」
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
transfer.html       互传文件窗口页面（设备选择/IP 直发/接收确认/传输记录）
src/
  main.ts           主窗口前端逻辑（列表渲染、类型标签页、粘贴交互）
  settings.ts       设置页逻辑（热键、自启、数据库目录、局域网同步等）
  blocked.ts        黑名单窗口逻辑（列表 + 移出黑名单）
  transfer.ts       互传文件窗口逻辑（实时扫描、发送、接收确认队列、传输记录）
  config.ts         前端共享配置
  confirm.ts        确认对话框组件（confirmDialog 二选一 / choiceDialog 多选一）
  styles.css        全局样式
src-tauri/
  src/lib.rs        后端主体（约 2300 行）：剪贴板监听、SQLite 存取、
                    托盘菜单、全局热键、窗口控制、模拟粘贴
  src/lan.rs        局域网多设备同步（约 2700 行）：HTTP 服务（8765）、
                    UDP beacon 发现（8766 / 组播 239.255.60.60:8767）、
                    配对码鉴权（支持对方开「自动同意」时免码配对）、
                    增量同步客户端、黑名单
  src/lan/transfer.rs  局域网互传文件：POST /recv 接收端（流式落盘、手动/
                    自动确认）、发送端（免配对、IP 直发、配对时加密）、
                    transfers 表传输记录
  src/relay.rs      云端中继客户端（M2）：WSS 长连接、挑战-响应 HMAC 鉴权、
                    本机文本 push、收到 clip 复用 lan::store_remote 入库、
                    指数退避重连、配置代际变更自动重连
  src/main.rs       入口（仅调用 lib）
relay-server/       自建中继服务器（独立 Rust 工程）：WSS 接入、强制鉴权、
                    分组转发、Dockerfile / GHCR 镜像发布 workflow
docs/relay-sync-design.md  中继 + 局域网双通道同步设计文档
  capabilities/     Tauri 权限声明（windows 列表需包含新增窗口 label）
  tauri.conf.json   窗口/打包配置（identifier: com.lsh.lscopy）
vite.config.ts      多页面构建配置（index + settings + blocked + transfer）
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
- **README 同步**：每次新增功能都要同步重写 `README.md`（更新功能特性等对应小节），写入文件后与功能代码一并提交。

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
  - 需手动确认的配对请求：`ask_pair_approval` 会先自动弹出设置窗口（mac 还要 `app.show()`），
    前端 `lan-pair-request` 监听里切到「设备同步」页再弹确认框（30s 超时按需确认失败）。
  - 同步入库时间一律用**本机当前时间**（`import_clip` / relay 收 clip 都是 `now_secs()`），
    记录作为新条目排在列表最前；远端 `timestamp` 只用于增量游标、文件命名去重和 relay E2E 的 AAD。
  - 手动同步「最近 N 条」：先按扩展参数 `order=desc&limit=N` 请服务端倒序取 N 条；
    服务端不认识该参数（升序返回，如安卓端）时回退为客户端游标分页拉全量、按 id 去重后
    本地倒序取 N 条（分页带 max_ts 进度保护，防参数被忽略导致死循环）。
  - 移除设备（解除配对 `lan_unpair` / 删除残留 `lan_forget_device`）默认**不删已同步记录**；
    前端三选一弹窗可选「连同删除」，走 `delete_device_clips`：只删 `remote_device_id` 匹配
    且未置顶的记录，文件类仅删配置文件存储路径内的文件（路径外绝不碰），删完发 `clip-added`
    让主面板刷新。
  - 同步页前端每 2s 轮询重建设备列表：配对表单展开期间（`pairingDeviceId` 非空）必须跳过
    列表重建，否则输入框会被刷掉；新增实时刷新类 UI 时注意同样的坑。
- **互传文件**（`lan/transfer.rs`，与同步共用 HTTP 服务但**免配对**）：
  - 端点 `POST /recv?name=&size=`：接收方校验黑名单后直接流式落盘（64KB 块，不入内存）；
    保存目录沿用同步的 `download_dir`（空则回落系统下载目录），重名自动加 ` (n)`。
  - 接收确认：`lan.transfer_auto_accept`（默认 false）关闭时弹窗等用户答复
    （`pending_recvs` 通道 + `transfer-incoming` 事件，60s 超时自动拒绝），答复命令
    `transfer_respond_recv`；收到请求时自动弹出 transfer 窗口（mac 还要 `app.show()`）。
    拒绝/超时后排空请求体（上限 64MB）再断连，让发送方读到明确错误而非写失败。
  - 发送：在线设备直接选（`transfer_send`）或手动 IP（`transfer_send_ip`，探测配置端口 +
    8765 的 `/info`）；已配对设备用对方配对码做 XOR 流加密（X-Enc-Nonce 头），未配对明文；
    旧版应用/安卓端无 `/recv`，发送方按 404 提示「对方版本过旧」。
  - 传输记录只记**接收成功**的条目（SQLite `transfers` 表，保留最近 200 条；发送结果走
    弹窗/toast 不入库，查询按 `direction='recv' AND ok=1` 过滤）；前端靠 `transfer-progress` /
    `transfer-changed` 事件刷新；设备列表只显示在线设备，窗口内每 3s 轮询 + 每 10s 深度扫描。
    单条删除（`transfer_delete`）与清空（`transfer_clear_history`）都会三选一询问是否连同
    文件删除——只删接收保存的文件，**绝不碰发送源文件**（`remove_recv_file` 按 direction 拦截）。
  - 发送成功的结果提示是底部 toast（默认 30s 倒计时自动关闭），有失败时仍用 alert 手动关闭。
  - 密钥流 8 字节块全局对齐（`xor_crypt_at` 带 chunk_base），收发两端块大小都必须是 8 的倍数。
- **批量删除三选一**：范围内有置顶记录时用 `choiceDialog` 提供「取消 / 只删非置顶 / 连同置顶删除」，
  不要退回二选一弹窗（取消语义会被占用）。
- **构建必须走 Tauri CLI**（`bun run tauri build` / `tauri dev`），不要裸 `cargo build --release`：CLI 会开启 `custom-protocol` 特性并正确处理前端资源协议，裸 cargo 构建的 exe 会显示"无法访问页面"。
- Windows 为主要目标平台；`winreg` 仅 Windows 编译（`cfg(windows)`）。
- 剪贴板图片读取有 Windows 原生兜底逻辑（CF_BITMAP/CF_DIB），改动相关代码时注意不要回归截图软件兼容性。
- 配置统一保存在 `lscopy-config.json`（顶层 `app` + `lan` + `relay` 三键），默认数据目录由 `default_data_dir()` 决定：
  **Windows = exe 同目录**（便携模式）；**macOS = `~/Library/Application Support/com.lsh.lscopy`**
  （mac 更新会整体替换 .app，包内的配置/数据库/目录指针每次更新都会丢，改写包内容还会破坏代码签名）。
  实际目录由默认数据目录下的指针文件 `lscopy-config-dir.txt` 决定（设置页「配置文件」可自定义，
  改动时询问是否迁移旧文件；读取时兼容 exe 同目录的旧指针）。数据库 `lscopy.db` 默认同目录；旧版系统配置目录
  （`%APPDATA%`）的配置会在首次启动时自动迁移。落盘统一走 `persist_config`（锁顺序固定 config → lan.settings → relay.settings → config_file）。
- 主窗口失焦自动隐藏是**延迟 150ms 复查**实现的（拖动/缩放会造成瞬时失焦）；改动窗口事件逻辑时注意 `dragging` / `panel_pinned` / `main_focused` 三个状态。
- **mac 面板圆角**：macOS 无边框窗口没有系统圆角（Windows 由 DWM 自动圆角），`main.ts` 检测 `isMac` 给 `<html>` 加 `.mac` 类，`styles.css` 把背景与圆角移到 `.app` 并 overflow 裁切（body 背景会传播到画布、不受圆角裁切，必须清掉，用 `!important` 压过主题/材质规则）；固定定位的 `.bg-layer` / `.confirm-overlay` 不受祖先 overflow 裁切，各自带圆角。
- **mac 粘贴防剪贴文件**：`paste_worker` 模拟按键前用 `lsappinfo`（`frontmost_is_finder`）检测前台是访达则跳过 ⌘V——桌面/访达窗口没有文本粘贴目标，直接 ⌘V 会让访达在桌面生成「文本剪贴」文件；剪贴板仍会更新，用户可到目标处手动粘贴。
- 版本号需同步修改 `package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json` 三处（`Cargo.lock` 随构建自动更新）。
- **应用内更新（Tauri updater）**：`tauri.conf.json` 已开启 `createUpdaterArtifacts` 并配置 pubkey/endpoints，
  私钥在本机 `~/.tauri/lscopy.key`（密码在同目录 `.password` 文件，务必备份）；GitHub secrets 已配置
  `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。本地 `bun run tauri build` 若因缺
  签名私钥报错，设环境变量 `TAURI_SIGNING_PRIVATE_KEY_PATH` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 即可。
  **注意**：`APPLE_*` 签名环境变量未配置时绝不能以空字符串传入 workflow（Tauri CLI 只判断变量存在性），
  需要 macOS 签名时按 SIGNING.md 把 env 行加回。
  - **便携版就地更新**（lib.rs「便携版应用内更新」区）：`update_install_kind` 按注册表
    Uninstall 项（lscopy / com.lsh.lscopy）区分安装版与便携版。**Windows 安装版**
    （lib.rs「安装版应用内更新」区）：不走官方静默 updater——`installer_update_path`
    把目标定到系统「下载」目录（`dirs::download_dir`，好找、可留档），复用
    `portable_update_download` / `portable_update_verify` 下载并校验
    `lscopy_{ver}_x64-setup.exe`，确认后 `installer_update_run` 启动安装包并退出
    （NSIS 装回原目录并重启新版）；macOS 仍走官方 updater 静默替换 .app。
    便携版走自研链路：`portable_update_begin`（目标 = exe 同目录
    lscopy_new.exe 或用户指定目录的 lscopy.exe）→ `portable_update_download`（ureq 流式
    下载 + portable-update-progress 事件）→ 与 Release 的 sha256sums-windows.txt 做
    SHA-256 比对（`http_get_text` + `portable_update_verify`）→ `portable_update_apply`
    把自身 exe 拷为 `%TEMP%/lscopy-updater.exe` 并以 `--apply-update <new> <old>` 启动
    （GUI 子系统无窗口；不再用 PowerShell 脚本——CREATE_NO_WINDOW 在 Windows Terminal
    为默认终端时仍会弹窗），辅助进程轮询 rename 等旧进程退出后替换 exe 并重启，
    正常启动时会顺手清理该临时副本。
    下载 URL 依赖 workflow 的产物命名约定 `lscopy_v{ver}_x64_portable.exe` /
    `lscopy_{ver}_x64-setup.exe`，改名要同步 settings.ts。
- **发布流程**（新会话发布按此完整执行）：
  1. 验证：`bun run build` + `cargo check` / `cargo clippy` 全绿；版本号三处已同步。
  2. 提交并推送：`git add -A && git commit` → `git push origin main` → `git tag vX.Y.Z && git push origin vX.Y.Z`。
     推 `v*` tag 触发 `.github/workflows/release.yml`，Windows + macOS 并行构建（约 10–20 分钟）。
     - **代码签名**（可选，配了 secrets 才生效）：Windows 需 `WINDOWS_CERT_PFX`（base64 的 .pfx）+
       `WINDOWS_CERT_PASSWORD`，CI 会向 tauri.conf.json 注入 `signCommand` 调 `src-tauri/sign-windows.ps1`
       给 exe / NSIS / MSI 签名；macOS 需 `APPLE_CERTIFICATE`（base64 的 .p12）/ `APPLE_CERTIFICATE_PASSWORD` /
       `APPLE_SIGNING_IDENTITY` / `APPLE_ID` / `APPLE_PASSWORD`（App 专用密码）/ `APPLE_TEAM_ID`，
       tauri-action 自动完成签名 + 公证。未配置则构建未签名产物，不报错。
     - **SHA-256 校验**：每个构建任务生成并上传 `sha256sums-windows.txt` / `sha256sums-macos-arm64.txt` /
       `sha256sums-macos-x86_64.txt`，覆盖该任务的全部产物。
  3. **GitHub API 凭据**：本机无 `gh` CLI，token 在 Windows 凭据管理器（git credential manager）里，
     用 `printf "protocol=https\nhost=github.com\n\n" | git credential fill` 取 `password=` 行即为
     token（用户 duangdangding，scope 含 repo/workflow）。**任何时候不得明文打印 token**：
     输出前先 `sed 's/password=.*/password=**FOUND**/'`；传给 Python 用环境变量
     （`export GH_TOKEN=$(...)`，Windows 上 Python 的 `os.popen` 走 cmd.exe，git-bash 管道不可用）。
  4. **构建结果由用户自行查看确认**（GitHub Actions 页面），助手不要自动轮询构建状态；
     用户确认构建成功后才继续下一步。
  5. 写 release notes（用户确认构建成功后执行）：先在工作区写 `release-notes-X.Y.Z.md`
     （新功能 / 升级提醒 / 其他），`GET /releases/tags/vX.Y.Z` 拿 release id →
     `PATCH /releases/{id}` 写入 `body`；若 `draft: true` 再 PATCH `{"draft": false}` 发布
     （当前 workflow 产出即非 draft，写 body 即生效）。
     核对：重新 GET，确认 body 首尾完整、assets 数量正确（通常 14 个：setup/msi/portable + 双 dmg + 双 app.tar.gz + 3 个 updater 签名 .sig + latest.json + 3 个 sha256sums 校验文件）。
- `dist/`、`target/`、`node_modules/` 为构建产物，不要提交或编辑。
