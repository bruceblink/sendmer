Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$installerPath = Join-Path $PSScriptRoot "..\install.ps1"
$originalVersion = $env:SENDMER_VERSION

try {
    $env:SENDMER_VERSION = "v0.5.0-preview+build.7"
    $result = & {
        param([string]$Path)

        $state = [pscustomobject]@{ DownloadCount = 0 }
        function Invoke-WebRequest {
            param(
                [Parameter(Position = 0, Mandatory = $true)]
                [string]$Uri,

                [Parameter(Mandatory = $true)]
                [string]$OutFile
            )

            $state.DownloadCount += 1
            if ($state.DownloadCount -eq 1) {
                [System.IO.File]::WriteAllBytes($OutFile, [byte[]](1, 2, 3))
                return
            }

            throw "simulated checksum download failure"
        }

        $caught = $null
        try {
            . $Path
        }
        catch {
            $caught = $_
        }

        [pscustomobject]@{
            Error         = $caught
            TempDir       = $TempDir
            DownloadCount = $state.DownloadCount
        }
    } $installerPath

    if ($null -eq $result.Error) {
        throw "installer should fail when the checksum download fails"
    }
    if ($result.Error.Exception.Message -notmatch "simulated checksum download failure") {
        throw "installer did not preserve the download failure"
    }
    if ($result.DownloadCount -ne 2) {
        throw "installer should attempt both artifact downloads"
    }
    if (Test-Path -LiteralPath $result.TempDir) {
        throw "installer left its temporary directory behind: $($result.TempDir)"
    }

    $env:SENDMER_VERSION = "v1.2.3.rc1"
    $invalidResult = & {
        param([string]$Path)

        function Invoke-WebRequest {
            throw "invalid release version should not start a download"
        }

        $caught = $null
        try {
            . $Path
        }
        catch {
            $caught = $_
        }
        $caught
    } $installerPath
    if ($null -eq $invalidResult) {
        throw "installer should reject a dotted prerelease without a hyphen"
    }
    if ($invalidResult.Exception.Message -notmatch "Invalid release version") {
        throw "installer returned the wrong invalid-version error"
    }

    Write-Host "PowerShell installer failure cleanup passed."
}
finally {
    $env:SENDMER_VERSION = $originalVersion
}
