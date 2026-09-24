# lscopy Windows 代码签名脚本
# 由 CI 在配置了 WINDOWS_CERT_PFX / WINDOWS_CERT_PASSWORD secrets 时，
# 注入 tauri.conf.json 的 bundle.windows.signCommand 调用（Tauri 对每个待签名文件调用一次）。
# 证书路径与密码通过环境变量传入，不写进配置文件。
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$FilePath
)
$ErrorActionPreference = 'Stop'

$certPath = $env:WINDOWS_CERT_PATH
$certPassword = $env:WINDOWS_CERT_PASSWORD
if (-not $certPath -or -not (Test-Path $certPath)) {
    throw "未找到代码签名证书（WINDOWS_CERT_PATH=$certPath）"
}

# 定位 signtool.exe：优先 TAURI_WINDOWS_SIGNTOOL_PATH，其次 PATH，最后搜索 Windows SDK 目录
$signtool = $env:TAURI_WINDOWS_SIGNTOOL_PATH
if (-not $signtool) {
    $signtool = (Get-Command signtool.exe -ErrorAction SilentlyContinue).Source
}
if (-not $signtool) {
    $signtool = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe' -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending |
        Select-Object -First 1 -ExpandProperty FullName
}
if (-not $signtool) {
    throw '未找到 signtool.exe，请安装 Windows SDK 或设置 TAURI_WINDOWS_SIGNTOOL_PATH'
}

& $signtool sign /fd sha256 /td sha256 /tr http://timestamp.digicert.com /f $certPath /p $certPassword $FilePath | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "signtool 签名失败: $FilePath"
}
Write-Host "已签名: $FilePath"
