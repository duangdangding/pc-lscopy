# 发布签名配置指南（SIGNING）

lscopy 的发布流程（`.github/workflows/release.yml`）支持 Windows / macOS 数字签名与 SHA-256 校验。

- **不配任何 secret**：正常构建并发布，产物不签名（与旧行为一致），仅额外产出 3 个 `sha256sums-*.txt` 校验文件。
- **配置 secret 后**：自动启用签名（macOS 含公证），无需改代码。

secret 配置入口：GitHub 仓库 → **Settings → Secrets and variables → Actions → New repository secret**。

---

## Windows 签名（2 个 secret）

需要一个**代码签名证书**，来源二选一：

### A. 购买商业证书（推荐，能消除 SmartScreen 警告）

- 从 DigiCert、Sectigo、SSL.com 等 CA 购买「代码签名证书」（OV 约 ¥1000–3000/年，EV 更贵但即时获得 SmartScreen 信誉）。
- 性价比高的小众选择：Certum（约 €100/年，需用其硬件密钥或云签名，流程稍麻烦）。
- 购买后 CA 会交付或让你导出 `.pfx` 文件。

### B. 自签名证书（仅测试用，Windows 仍会警告）

```powershell
# PowerShell（管理员）生成自签名证书并导出 pfx
$cert = New-SelfSignedCertificate -Type CodeSigningCert `
  -Subject "CN=lscopy" -CertStoreLocation Cert:\CurrentUser\My `
  -NotAfter (Get-Date).AddYears(3)
$pwd = ConvertTo-SecureString -String "你的密码" -Force -AsPlainText
Export-PfxCertificate -Cert $cert -FilePath "lscopy-cert.pfx" -Password $pwd
```

### 生成 secret

拿到 `.pfx` 后：

```powershell
# WINDOWS_CERT_PFX：pfx 转 base64（一行，复制后粘贴到 GitHub）
[Convert]::ToBase64String([IO.File]::ReadAllBytes("lscopy-cert.pfx")) | Set-Clipboard
```

| Secret | 值 |
|---|---|
| `WINDOWS_CERT_PFX` | 上面的 base64 字符串 |
| `WINDOWS_CERT_PASSWORD` | 导出 pfx 时设的密码 |

签名实现：CI 解出证书后向 `tauri.conf.json` 注入 `bundle.windows.signCommand`，调用
`src-tauri/sign-windows.ps1` 给 exe / NSIS 安装包 / MSI 逐个签名（sha256 摘要 + DigiCert 时间戳）。

---

## macOS 签名 + 公证（6 个 secret）

前提：**加入 Apple Developer Program**（$99/年，个人即可）。没有付费账号拿不到以下任何一项。

| Secret | 获取方式 |
|---|---|
| `APPLE_CERTIFICATE` | ① [developer.apple.com/account](https://developer.apple.com/account) → Certificates 创建 **Developer ID Application** 证书（需先用「钥匙串访问」生成 CSR）；② 下载后双击导入钥匙串；③ 在钥匙串中右键该证书 → 导出为 `.p12`（设导出密码）；④ `base64 -i cert.p12 \| pbcopy` 得 base64 |
| `APPLE_CERTIFICATE_PASSWORD` | 导出 .p12 时设的密码 |
| `APPLE_SIGNING_IDENTITY` | 终端执行 `security find-identity -v -p codesigning`，复制引号内完整字符串，形如 `Developer ID Application: 张三 (ABCD123456)` |
| `APPLE_ID` | Apple ID 邮箱 |
| `APPLE_PASSWORD` | **不是登录密码**——[appleid.apple.com](https://appleid.apple.com) → 登录与安全 → App 专用密码，生成一个 |
| `APPLE_TEAM_ID` | [developer.apple.com/account](https://developer.apple.com/account) → Membership details 里的 Team ID（10 位字母数字） |

tauri-action 会基于这些环境变量自动完成：证书导入临时钥匙串 → Developer ID 签名 → 公证 → staple。

---

## SHA-256 校验（无需配置）

每次发布自动为各平台全部产物生成并上传：

- `sha256sums-windows.txt`（setup.exe / msi / portable exe）
- `sha256sums-macos-arm64.txt`（dmg / app.tar.gz）
- `sha256sums-macos-x86_64.txt`（dmg / app.tar.gz）

用户验证方式：

```powershell
# Windows
(Get-FileHash lscopy_x.y.z_x64-setup.exe -Algorithm SHA256).Hash
```

```bash
# macOS / Linux
shasum -a 256 -c sha256sums-macos-arm64.txt
```

---

## 安全提醒

- `.pfx` / `.p12` 本地留档即可，**绝不提交进仓库**；base64 值只粘贴进 GitHub Secrets。
- Apple App 专用密码可随时在 appleid.apple.com 吊销重建，泄露时优先吊销它。
