# Device HUB を常駐アプリとしてインストールする (管理者権限不要)。
#
#   powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 [-Exe <path>] [-Uninstall]
#
# - %LOCALAPPDATA%\Programs\DeviceHub\device-hub.exe にコピー
# - ログオン時に `--minimized` (トレイ格納) で自動起動 (HKCU\...\Run)
# - スタートメニューにショートカットを作成
# - すぐに起動する
param(
    [string]$Exe = "",
    [switch]$Uninstall
)
$ErrorActionPreference = "Stop"
$dir = Join-Path $env:LOCALAPPDATA "Programs\DeviceHub"
$dst = Join-Path $dir "device-hub.exe"
$run = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$lnk = Join-Path ([Environment]::GetFolderPath("Programs")) "Device HUB.lnk"

Get-Process device-hub -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300

if ($Uninstall) {
    Remove-ItemProperty -Path $run -Name "DeviceHub" -ErrorAction SilentlyContinue
    Remove-Item $lnk -ErrorAction SilentlyContinue
    Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Device HUB を削除しました"
    exit 0
}

if (-not $Exe) {
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot "..\target" }
    $Exe = Join-Path $target "release\device-hub.exe"
}
if (-not (Test-Path $Exe)) { throw "device-hub.exe が見つかりません: $Exe (cargo build --release -p device-hub)" }

New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $Exe $dst -Force
Set-ItemProperty -Path $run -Name "DeviceHub" -Value "`"$dst`" --minimized"

$sh = New-Object -ComObject WScript.Shell
$s = $sh.CreateShortcut($lnk)
$s.TargetPath = $dst
$s.WorkingDirectory = $dir
$s.Description = "Device HUB"
$s.Save()

Start-Process $dst -ArgumentList "--minimized"
Write-Host "インストール完了: $dst (ログオン時にトレイへ自動起動)"
