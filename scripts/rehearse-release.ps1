[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$')]
    [string]$Tag,

    [switch]$RequireRemoteTag,

    [switch]$SkipQualityGate
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Invoke-GitOutput {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorkingDirectory,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    Push-Location -LiteralPath $WorkingDirectory
    try {
        $output = & git @Arguments 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
        }

        return ($output -join "`n").Trim()
    }
    finally {
        Pop-Location
    }
}

function Invoke-CheckedCommand {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorkingDirectory,

        [Parameter(Mandatory = $true)]
        [string]$Description,

        [Parameter(Mandatory = $true)]
        [string]$Executable,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    # Each release check runs in the temporary tag worktree, never in the caller's checkout.
    Write-Host "[release-rehearsal] $Description"
    Push-Location -LiteralPath $WorkingDirectory
    try {
        & $Executable @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "$Description failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

function Get-TagPackageVersion {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorkingDirectory,

        [Parameter(Mandatory = $true)]
        [string]$ReleaseTag
    )

    # Read the package version from the tagged Cargo.toml, not from the current branch.
    $cargoToml = Invoke-GitOutput -WorkingDirectory $WorkingDirectory -Arguments @(
        "show",
        ("{0}:Cargo.toml" -f $ReleaseTag)
    )
    $inPackageSection = $false
    foreach ($line in ($cargoToml -split "`r?`n")) {
        if ($line -match '^\s*\[package\]\s*$') {
            $inPackageSection = $true
            continue
        }

        if ($inPackageSection -and $line -match '^\s*\[') {
            break
        }

        if ($inPackageSection -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }

    throw "Could not find the package version in $ReleaseTag`:Cargo.toml."
}

function Invoke-QualityGate {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorkingDirectory
    )

    # Keep this list aligned with the release workflow so local rehearsal catches the same failures.
    $checks = @(
        @{
            Description = "cargo fmt"
            Arguments   = @("fmt", "--all", "--", "--check")
        },
        @{
            Description = "cargo clippy"
            Arguments   = @("clippy", "--locked", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings")
        },
        @{
            Description = "cargo check"
            Arguments   = @("check", "--workspace", "--all-features", "--bins")
        },
        @{
            Description = "cargo package"
            Arguments   = @("package", "--locked", "--no-verify")
        },
        @{
            Description = "cargo test"
            Arguments   = @("test", "--locked", "--workspace", "--all-features", "--bins", "--tests", "--examples")
        }
    )

    foreach ($check in $checks) {
        Invoke-CheckedCommand `
            -WorkingDirectory $WorkingDirectory `
            -Description $check.Description `
            -Executable "cargo" `
            -Arguments $check.Arguments
    }
}

$repoRoot = Invoke-GitOutput -WorkingDirectory (Get-Location).Path -Arguments @("rev-parse", "--show-toplevel")
$tagCommit = Invoke-GitOutput -WorkingDirectory $repoRoot -Arguments @("rev-parse", "--verify", ("{0}^{{commit}}" -f $Tag))
$packageVersion = Get-TagPackageVersion -WorkingDirectory $repoRoot -ReleaseTag $Tag
$expectedTag = "v$packageVersion"

if ($Tag -ne $expectedTag) {
    throw "Tag $Tag does not match the tagged Cargo package version ($expectedTag)."
}

if ($RequireRemoteTag) {
    $remoteOutput = Invoke-GitOutput -WorkingDirectory $repoRoot -Arguments @(
        "ls-remote",
        "--tags",
        "origin",
        ("refs/tags/{0}" -f $Tag),
        ("refs/tags/{0}^{{}}" -f $Tag)
    )
    $remoteLines = $remoteOutput -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    $remoteLine = ($remoteLines | Where-Object { $_ -match '\^\{\}$' } | Select-Object -First 1)
    if ([string]::IsNullOrWhiteSpace($remoteLine)) {
        $remoteLine = $remoteLines | Select-Object -First 1
    }

    if ([string]::IsNullOrWhiteSpace($remoteLine)) {
        throw "Remote origin does not contain tag $Tag."
    }

    $remoteCommit = ($remoteLine -split '\s+')[0]
    if ($remoteCommit -ne $tagCommit) {
        throw "Remote tag $Tag points to $remoteCommit, but the local tag points to $tagCommit."
    }
}

$tempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$worktreePath = Join-Path $tempRoot ("sendmer-release-rehearsal-{0}" -f ([guid]::NewGuid().ToString("N")))
$worktreeFullPath = [System.IO.Path]::GetFullPath($worktreePath)
if (-not $worktreeFullPath.StartsWith($tempRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to create a rehearsal worktree outside the system temp directory."
}

$worktreeCreated = $false
try {
    Invoke-CheckedCommand `
        -WorkingDirectory $repoRoot `
        -Description "checkout $Tag into a temporary worktree" `
        -Executable "git" `
        -Arguments @("worktree", "add", "--detach", $worktreeFullPath, $Tag)
    $worktreeCreated = $true

    $checkedOutCommit = Invoke-GitOutput -WorkingDirectory $worktreeFullPath -Arguments @("rev-parse", "HEAD")
    if ($checkedOutCommit -ne $tagCommit) {
        throw "Temporary worktree checked out $checkedOutCommit instead of tag commit $tagCommit."
    }

    if ($SkipQualityGate) {
        Write-Host "[release-rehearsal] quality gate skipped by request."
    }
    else {
        Invoke-QualityGate -WorkingDirectory $worktreeFullPath
    }

    Write-Host "[release-rehearsal] tag $Tag passed the local no-upload rehearsal."
}
finally {
    if ($worktreeCreated) {
        try {
            Invoke-CheckedCommand `
                -WorkingDirectory $repoRoot `
                -Description "remove temporary worktree" `
                -Executable "git" `
                -Arguments @("worktree", "remove", "--force", $worktreeFullPath)
        }
        catch {
            Write-Warning "The rehearsal worktree could not be removed: $worktreeFullPath"
            Write-Warning $_.Exception.Message
        }
    }
}
