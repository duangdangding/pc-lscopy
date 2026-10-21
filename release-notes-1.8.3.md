## 修复

**未启用「安全锁定」却弹出解锁遮罩** —— 修复了没有设置密码 / 组合键时，打开窗口仍会弹出解锁遮罩、要求输入密码的问题：

- 根因：样式表中 `.lock-overlay` 的 `display: flex` 压过了浏览器对 `hidden` 属性的默认 `display: none`，导致前端 `overlay.hidden = true` 失效，遮罩始终显示
- 同步修复设置页「安全锁定」区块中受 `hidden` 控制的输入行（如「当前密码」）无法隐藏的问题（`.row` 同类问题）
- 现已在全局样式中声明 `[hidden] { display: none !important }`，统一保证 `hidden` 属性生效，避免同类问题再次出现

## 升级提醒

- 未设置锁定时，托盘菜单「立即锁定」为置灰（不可用）状态，属正常表现；如需手动锁定，请先到「设置 → 安全锁定」设置密码或组合键

## 其他

- 校验文件：`sha256sums-windows.txt` / `sha256sums-macos-arm64.txt` / `sha256sums-macos-x86_64.txt` 覆盖本次全部产物，下载后可核对 SHA-256
