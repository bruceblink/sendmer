$ErrorActionPreference = "Stop"

$Repo = "bruceblink/sendmer"
$Bin = "sendmer"
$CosignVersion = if ($env:SENDMER_COSIGN_VERSION) { $env:SENDMER_COSIGN_VERSION } else { "v3.1.4" }
$CosignBin = $env:SENDMER_COSIGN_BIN

# 允许用户指定版本
$Version = $env:SENDMER_VERSION

if (-not $Version -and $env:SENDMER_ALLOW_LATEST -eq "1") {
    Write-Host "Fetching latest release version (explicitly enabled)..."
    $Api = "https://api.github.com/repos/$Repo/releases/latest"
    $Response = Invoke-RestMethod -Uri $Api -Headers @{
        "User-Agent" = "sendmer-installer"
    }
    $Version = $Response.tag_name
}

if (-not $Version) {
    throw "SENDMER_VERSION is required (for example: v0.11.2). Set SENDMER_ALLOW_LATEST=1 only when floating latest is intentional."
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

$InstallDir = if ($env:SENDMER_INSTALL_DIR) { $env:SENDMER_INSTALL_DIR } else { "$env:USERPROFILE\.sendmer\bin" }
$ZipName = "$Bin-$Version-$Arch-pc-windows-msvc.zip"
$DownloadBaseUrl = if ($env:SENDMER_DOWNLOAD_BASE_URL) { $env:SENDMER_DOWNLOAD_BASE_URL.TrimEnd('/') } else { "https://github.com/$Repo/releases/download" }
$TempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$TempDir = [System.IO.Path]::GetFullPath((Join-Path $TempRoot (
            "sendmer-install-{0}" -f [Guid]::NewGuid().ToString("N")
        )))
if (-not $TempDir.StartsWith($TempRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to create installer files outside the system temp directory"
}

$ZipPath = Join-Path $TempDir $ZipName
$Url = "$DownloadBaseUrl/$Version/$ZipName"
$ChecksumPath = "$ZipPath.sha256"
$ChecksumUrl = "$Url.sha256"
$SbomPath = "${ZipPath}.spdx.json"
$ArchiveSignaturePath = "${ZipPath}.sigstore.json"
$SbomSignaturePath = "${ZipPath}.spdx.json.sigstore.json"
$ProvenancePath = "${ZipPath}.attestation.json"
$CosignAsset = "cosign-windows-amd64.exe"
$CosignSha256 = "cbe953787074611f1334cbf37dc1cf953995bb8c45398eb4fc7bb781d29c1f2a"
$StageDir = $null

# Verify each signed blob with the release workflow identity before extraction.
function Invoke-SigstoreVerification {
    param(
        [Parameter(Mandatory = $true)][string]$BlobPath,
        [Parameter(Mandatory = $true)][string]$BundlePath,
        [Parameter(Mandatory = $true)][string]$IdentityRegex
    )

    & $CosignBin verify-blob $BlobPath `
        --bundle $BundlePath `
        --certificate-identity-regexp $IdentityRegex `
        --certificate-oidc-issuer "https://token.actions.githubusercontent.com"
    if ($LASTEXITCODE -ne 0) {
        throw "Sigstore verification failed for $(Split-Path -Leaf $BlobPath)"
    }
}

function Invoke-ProvenanceVerification {
    param(
        [Parameter(Mandatory = $true)][string]$BlobPath,
        [Parameter(Mandatory = $true)][string]$BundlePath,
        [Parameter(Mandatory = $true)][string]$IdentityRegex
    )

    & $CosignBin verify-blob-attestation $BlobPath `
        --bundle $BundlePath `
        --certificate-identity-regexp $IdentityRegex `
        --certificate-oidc-issuer "https://token.actions.githubusercontent.com" `
        --certificate-github-workflow-repository $Repo `
        --certificate-github-workflow-ref "refs/tags/$Version" `
        --certificate-github-workflow-name "release" `
        --type "https://slsa.dev/provenance/v1"
    if ($LASTEXITCODE -ne 0) {
        throw "Provenance verification failed for $(Split-Path -Leaf $BlobPath)"
    }
}

Write-Host "Installing sendmer $Version"
Write-Host "Downloading $Url"

try {
    New-Item -ItemType Directory -Path $TempDir | Out-Null
    Invoke-WebRequest $Url -OutFile $ZipPath
    Invoke-WebRequest $ChecksumUrl -OutFile $ChecksumPath
    Invoke-WebRequest "${Url}.spdx.json" -OutFile $SbomPath
    Invoke-WebRequest "${Url}.sigstore.json" -OutFile $ArchiveSignaturePath
    Invoke-WebRequest "${Url}.spdx.json.sigstore.json" -OutFile $SbomSignaturePath
    Invoke-WebRequest "${Url}.attestation.json" -OutFile $ProvenancePath

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

    if ($CosignBin) {
        $resolvedCosign = Get-Command $CosignBin -ErrorAction SilentlyContinue
        if (-not $resolvedCosign -and -not (Test-Path -LiteralPath $CosignBin)) {
            throw "SENDMER_COSIGN_BIN does not point to an executable: $CosignBin"
        }
        if ($resolvedCosign) {
            $CosignBin = if ($resolvedCosign.Source) { $resolvedCosign.Source } else { $resolvedCosign.Name }
        }
    } else {
        if ($CosignVersion -ne "v3.1.4") {
            throw "Unsupported SENDMER_COSIGN_VERSION=$CosignVersion; use v3.1.4 or set SENDMER_COSIGN_BIN"
        }
        $CosignPath = Join-Path $TempDir $CosignAsset
        Write-Host "Cosign not found; downloading pinned $CosignVersion verifier..."
        Invoke-WebRequest "https://github.com/sigstore/cosign/releases/download/$CosignVersion/$CosignAsset" -OutFile $CosignPath
        $CosignActualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $CosignPath).Hash.ToLowerInvariant()
        if ($CosignActualHash -ne $CosignSha256) {
            throw "Cosign SHA-256 verification failed"
        }
        $CosignBin = $CosignPath
    }
    foreach ($TrustPath in @($SbomPath, $ArchiveSignaturePath, $SbomSignaturePath, $ProvenancePath)) {
        if (-not (Test-Path -LiteralPath $TrustPath) -or (Get-Item -LiteralPath $TrustPath).Length -eq 0) {
            throw "Release trust material is empty: $(Split-Path -Leaf $TrustPath)"
        }
    }

    $EscapedRepo = [regex]::Escape($Repo)
    $EscapedVersion = [regex]::Escape($Version)
    $IdentityRegex = "^https://github\.com/$EscapedRepo/\.github/workflows/release\.yml@refs/tags/$EscapedVersion$"
    Invoke-SigstoreVerification -BlobPath $ZipPath -BundlePath $ArchiveSignaturePath -IdentityRegex $IdentityRegex
    Invoke-SigstoreVerification -BlobPath $SbomPath -BundlePath $SbomSignaturePath -IdentityRegex $IdentityRegex
    Invoke-ProvenanceVerification -BlobPath $ZipPath -BundlePath $ProvenancePath -IdentityRegex $IdentityRegex

    Write-Host "Extracting..."
    $InstallParent = Split-Path -Parent $InstallDir
    New-Item -ItemType Directory -Force -Path $InstallParent | Out-Null
    $StageDir = Join-Path $InstallParent (".sendmer-install-{0}" -f [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $StageDir | Out-Null
    Expand-Archive -Path $ZipPath -DestinationPath $StageDir -Force

    $StageExePath = Join-Path $StageDir "$Bin.exe"
    $StageMetadataPath = Join-Path $StageDir "BUILD-METADATA.txt"
    if (-not (Test-Path -LiteralPath $StageExePath)) {
        Write-Error "sendmer.exe not found after extraction"
    }
    if (-not (Test-Path -LiteralPath $StageMetadataPath)) {
        throw "BUILD-METADATA.txt not found after extraction"
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $MetadataTmp = Join-Path $InstallDir ".BUILD-METADATA.txt.tmp-$PID"
    $ExeTmp = Join-Path $InstallDir ".sendmer.exe.tmp-$PID"
    Copy-Item -LiteralPath $StageMetadataPath -Destination $MetadataTmp -Force
    Move-Item -LiteralPath $MetadataTmp -Destination (Join-Path $InstallDir "BUILD-METADATA.txt") -Force
    Copy-Item -LiteralPath $StageExePath -Destination $ExeTmp -Force
    Move-Item -LiteralPath $ExeTmp -Destination (Join-Path $InstallDir "$Bin.exe") -Force
}
finally {
    # Every install attempt owns isolated directories, so failure cleanup cannot clash.
    if ($StageDir -and (Test-Path -LiteralPath $StageDir)) {
        Remove-Item -LiteralPath $StageDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $TempDir -Recurse -Force -ErrorAction SilentlyContinue
}

# 添加到 PATH（用户级）
$UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
$PathEntries = @($UserPath -split ';' | Where-Object { $_ })
$PathAlreadyContainsInstallDir = $PathEntries | Where-Object {
    [System.StringComparer]::OrdinalIgnoreCase.Equals(
        [System.IO.Path]::GetFullPath($_),
        [System.IO.Path]::GetFullPath($InstallDir)
    )
}
if (-not $PathAlreadyContainsInstallDir) {
    Write-Host "Adding sendmer to PATH"
    [Environment]::SetEnvironmentVariable(
            "PATH",
            (($PathEntries + $InstallDir) -join ';'),
            "User"
    )
}

# Make the command available in the current PowerShell process as well.
$CurrentPathEntries = @($env:PATH -split ';' | Where-Object { $_ })
$CurrentPathContainsInstallDir = $CurrentPathEntries | Where-Object {
    [System.StringComparer]::OrdinalIgnoreCase.Equals(
        [System.IO.Path]::GetFullPath($_),
        [System.IO.Path]::GetFullPath($InstallDir)
    )
}
if (-not $CurrentPathContainsInstallDir) {
    $env:PATH = (($CurrentPathEntries + $InstallDir) -join ';')
}

Write-Host ""
Write-Host "sendmer $Version installed successfully!"
Write-Host "Run:"
Write-Host "  sendmer --help"
