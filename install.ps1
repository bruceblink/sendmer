$ErrorActionPreference = "Stop"

$Repo = "bruceblink/sendmer"
$Bin = "sendmer"

# 允许用户指定版本
$Version = $env:SENDMER_VERSION

if (-not $Version) {
    Write-Host "Fetching latest release version..."
    $Api = "https://api.github.com/repos/$Repo/releases/latest"
    $Response = Invoke-RestMethod -Uri $Api -Headers @{
        "User-Agent" = "sendmer-installer"
    }
    $Version = $Response.tag_name
}

# 架构检测
$Arch = if ([Environment]::Is64BitOperatingSystem) {
    "x86_64"
} else {
    Write-Error "32-bit Windows is not supported"
}

$InstallDir = "$env:USERPROFILE\.sendmer\bin"
$ZipName = "$Bin-$Version-$Arch-pc-windows-msvc.zip"
$ZipPath = Join-Path $env:TEMP $ZipName
$Url = "https://github.com/$Repo/releases/download/$Version/$ZipName"

Write-Host "Installing sendmer $Version"
Write-Host "Downloading $Url"

Invoke-WebRequest $Url -OutFile $ZipPath

Write-Host "Extracting..."
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Expand-Archive -Path $ZipPath -DestinationPath $InstallDir -Force

$ExePath = Join-Path $InstallDir "$Bin.exe"
if (-not (Test-Path $ExePath)) {
    Write-Error "sendmer.exe not found after extraction"
}

# 添加到 PATH（用户级）
$UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($UserPath -notlike "*$InstallDir*") {
    Write-Host "Adding sendmer to PATH"
    [Environment]::SetEnvironmentVariable(
            "PATH",
            "$UserPath;$InstallDir",
            "User"
    )
}

Write-Host ""
Write-Host "✅ sendmer $Version installed successfully!"
Write-Host "Restart your terminal and run:"
Write-Host "  sendmer --help"
