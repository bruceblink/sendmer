#![warn(clippy::all, clippy::nursery)]

//! Non-blocking release checks and user-confirmed self-updates.
//!
//! The updater deliberately delegates installation to the signed release
//! installers. It never replaces the running executable in-process.

use anyhow::Context;
use serde_json::Value;
use std::cmp::Ordering;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const API_URL: &str = "https://api.github.com/repos/bruceblink/sendmer/releases/latest";
const RAW_BASE_URL: &str = "https://raw.githubusercontent.com/bruceblink/sendmer";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

pub async fn check_and_offer_update() -> anyhow::Result<bool> {
    if std::env::var("SENDMER_NO_UPDATE_CHECK").as_deref() == Ok("1") {
        return Ok(false);
    }

    let latest = match fetch_latest_version().await {
        Ok(version) => version,
        Err(error) => {
            tracing::debug!(%error, "release update check skipped");
            return Ok(false);
        }
    };

    let current = Version::current();
    if latest <= current {
        return Ok(false);
    }

    if !prompt_for_update(current, latest)? {
        return Ok(false);
    }

    schedule_update(latest)?;
    println!("Update scheduled. sendmer will exit and install {latest} in the background.");
    Ok(true)
}

async fn fetch_latest_version() -> anyhow::Result<Version> {
    let output = if cfg!(windows) {
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            tokio::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Invoke-RestMethod -TimeoutSec 2 -Headers @{Accept='application/vnd.github+json'} -Uri 'https://api.github.com/repos/bruceblink/sendmer/releases/latest' | ConvertTo-Json -Compress",
                ])
                .output(),
        )
        .await
        .context("latest release request timed out")?
        .context("latest release request failed")?
    } else {
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            tokio::process::Command::new("curl")
                .args(["-fsSL", "--max-time", "2", API_URL])
                .output(),
        )
        .await
        .context("latest release request timed out")?
        .context("latest release request failed")?
    };

    if !output.status.success() {
        anyhow::bail!("latest release request returned {}", output.status);
    }
    let payload: Value =
        serde_json::from_slice(&output.stdout).context("decode latest release response")?;
    let tag = payload
        .get("tag_name")
        .and_then(Value::as_str)
        .context("latest release has no tag_name")?;
    Version::parse(tag).context("latest release tag is not stable semver")
}

fn prompt_for_update(current: Version, latest: Version) -> anyhow::Result<bool> {
    print!("A newer sendmer version is available ({current} -> {latest}). Update now? [y/N] ");
    io::stdout().flush().context("flush update prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read update choice")?;

    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn schedule_update(version: Version) -> anyhow::Result<()> {
    let version = version.to_string();
    let pid = std::process::id();
    let temp_dir = std::env::temp_dir();
    let script_name = if cfg!(windows) {
        format!("sendmer-update-{pid}.ps1")
    } else {
        format!("sendmer-update-{pid}.sh")
    };
    let script_path = temp_dir.join(script_name);
    let log_path = temp_dir.join(format!("sendmer-update-{pid}.log"));

    let script = if cfg!(windows) {
        windows_script(&version, pid, &log_path)
    } else {
        unix_script(&version, pid, &log_path)
    };
    std::fs::write(&script_path, script).context("write update helper")?;

    let result = if cfg!(windows) {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    } else {
        Command::new("sh")
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };

    result
        .map(|_| ())
        .with_context(|| format!("start update helper {}", script_path.display()))
}

fn unix_script(version: &str, pid: u32, log_path: &Path) -> String {
    let installer = format!("{RAW_BASE_URL}/{version}/install.sh");
    format!(
        "#!/bin/sh\n\
         while kill -0 {pid} 2>/dev/null; do sleep 0.2; done\n\
         export SENDMER_VERSION='{version}'\n\
         curl -fsSL '{installer}' | bash > '{log}' 2>&1\n\
         rm -f -- \"$0\"\n",
        log = log_path.display()
    )
}

fn windows_script(version: &str, pid: u32, log_path: &Path) -> String {
    let installer = format!("{RAW_BASE_URL}/{version}/install.ps1");
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         while (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }}\n\
         $env:SENDMER_VERSION = '{version}'\n\
         try {{ Invoke-WebRequest -UseBasicParsing -Uri '{installer}' | Select-Object -ExpandProperty Content | Invoke-Expression *>> '{log}' }}\n\
         catch {{ $_ | Out-File -FilePath '{log}' -Append }}\n\
         Remove-Item -LiteralPath $MyInvocation.MyCommand.Path -Force -ErrorAction SilentlyContinue\n",
        log = log_path.display()
    )
}

impl Version {
    const fn current() -> Self {
        Self::parse_const(env!("CARGO_PKG_VERSION"))
    }

    const fn parse_const(value: &str) -> Self {
        let bytes = value.as_bytes();
        let mut part = 0_u64;
        let mut index = 0;
        let mut parts = [0_u64; 3];
        while index < bytes.len() {
            let byte = bytes[index];
            if byte == b'.' {
                part += 1;
                index += 1;
                continue;
            }
            if byte >= b'0' && byte <= b'9' {
                let slot = part as usize;
                parts[slot] = parts[slot]
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u64);
            }
            index += 1;
        }
        Self {
            major: parts[0],
            minor: parts[1],
            patch: parts[2],
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        let value = value.strip_prefix('v').context("missing v prefix")?;
        let core = value
            .split(['-', '+'])
            .next()
            .context("missing version core")?;
        let mut parts = core.split('.');
        let version = Self {
            major: parts.next().context("missing major")?.parse()?,
            minor: parts.next().context("missing minor")?.parse()?,
            patch: parts.next().context("missing patch")?.parse()?,
        };
        if parts.next().is_some() {
            anyhow::bail!("version has too many components");
        }
        Ok(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stable_release_tag() {
        assert_eq!(
            Version::parse("v1.2.3").unwrap(),
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
        );
    }

    #[test]
    fn ignores_prerelease_suffix() {
        assert_eq!(
            Version::parse("v1.2.3-rc.1").unwrap(),
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
        );
    }

    #[test]
    fn rejects_invalid_tag() {
        assert!(Version::parse("release-1.2.3").is_err());
    }

    #[test]
    fn renders_version() {
        assert_eq!(
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
            .to_string(),
            "v1.2.3"
        );
    }
}
