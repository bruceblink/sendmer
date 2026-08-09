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

if ($Version -notmatch '^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
    throw "Invalid release version: $Version"
}

# 架构检测
$Arch = if ([Environment]::Is64BitOperatingSystem) {
    "x86_64"
} else {
    Write-Error "32-bit Windows is not supported"
}

$InstallDir = "$env:USERPROFILE\.sendmer\bin"
$ZipName = "$Bin-$Version-$Arch-pc-windows-msvc.zip"
$TempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$TempDir = [System.IO.Path]::GetFullPath((Join-Path $TempRoot (
            "sendmer-install-{0}" -f [Guid]::NewGuid().ToString("N")
        )))
if (-not $TempDir.StartsWith($TempRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to create installer files outside the system temp directory"
}

$ZipPath = Join-Path $TempDir $ZipName
$Url = "https://github.com/$Repo/releases/download/$Version/$ZipName"
$ChecksumPath = "$ZipPath.sha256"
$ChecksumUrl = "$Url.sha256"

Write-Host "Installing sendmer $Version"
Write-Host "Downloading $Url"

try {
    New-Item -ItemType Directory -Path $TempDir | Out-Null
    Invoke-WebRequest $Url -OutFile $ZipPath
    Invoke-WebRequest $ChecksumUrl -OutFile $ChecksumPath

    $ChecksumFields = @((Get-Content -Raw -LiteralPath $ChecksumPath).Trim() -split '\s+')
    $ExpectedHash = $ChecksumFields[0].ToLowerInvariant()
    $ExpectedFile = if ($ChecksumFields.Count -gt 1) { $ChecksumFields[1] } else { "" }
    if ($ExpectedHash -notmatch '^[0-9a-f]{64}$' -or $ExpectedFile -ne $ZipName) {
        throw "Invalid checksum file for $ZipName"
    }

    $ActualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $ZipPath).Hash.ToLowerInvariant()
    if ($ActualHash -ne $ExpectedHash) {
        throw "Checksum verification failed for $ZipName"
    }

    Write-Host "Extracting..."
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Expand-Archive -Path $ZipPath -DestinationPath $InstallDir -Force

    $ExePath = Join-Path $InstallDir "$Bin.exe"
    if (-not (Test-Path $ExePath)) {
        Write-Error "sendmer.exe not found after extraction"
    }
}
finally {
    # Every install attempt owns one isolated directory, so failure cleanup cannot clash.
    Remove-Item -LiteralPath $TempDir -Recurse -Force -ErrorAction SilentlyContinue
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
Write-Host "sendmer $Version installed successfully!"
Write-Host "Restart your terminal and run:"
Write-Host "  sendmer --help"
