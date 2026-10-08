## 修复

- **便携版应用内更新不再弹出 PowerShell 窗口**：替换 exe 的辅助脚本由 PowerShell 改为自身 exe 的临时副本（GUI 程序，天然无窗口），彻底消除更新时的黑色/蓝色控制台窗口（此前 `CREATE_NO_WINDOW` 在 Windows Terminal 作为默认终端时仍会弹窗）

## 升级提醒

- 本次从旧版升级仍由旧版执行替换，PowerShell 窗口会最后出现一次；装上本版后，往后的更新将彻底无窗口
