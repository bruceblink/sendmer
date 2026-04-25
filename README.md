# Sendmer [![][img_crates]][crates] [![][img_doc]][doc]

English | [中文](README_ZH.md)

Sendmer is a small CLI and reusable Rust library for sending files and directories over the internet with [iroh](https://crates.io/crates/iroh) and [iroh-blobs](https://crates.io/crates/iroh-blobs).

It is based on [n0-computer/sendme v0.31.0](https://github.com/n0-computer/sendme/commit/6e50167a8a4d45736179cce3d8d5fd803c87c24e), with a codebase that has been gradually reorganized into a clearer library + CLI structure.

## Features

- Send a file or an entire directory with a single command
- NAT traversal and hole punching via iroh, with relay fallback when needed
- Verified streaming with blake3 through `iroh-blobs`
- Reusable Rust API via exported `send` and `receive` functions
- Optional CLI progress output and clipboard helper

sendmer uses 256-bit node IDs, so tickets remain valid even if IP addresses change during a session. Connections are encrypted with TLS.

## Installation

### Windows (PowerShell)

```powershell
# Install latest release
iwr https://raw.githubusercontent.com/bruceblink/sendmer/main/install.ps1 -useb | iex

# Or install a specific version
$env:SENDMER_VERSION="v0.4.2"
iwr https://raw.githubusercontent.com/bruceblink/sendmer/main/install.ps1 -useb | iex
```

Default install path:

```powershell
C:\Users\<username>\.sendmer\bin\sendmer.exe
```

After installation:

- Add `$InstallDir` to `PATH` if needed
- Restart your terminal
- Run `sendmer --help`

### Linux / macOS

```bash
# Install latest release
curl -fsSL https://raw.githubusercontent.com/bruceblink/sendmer/main/install.sh | bash

# Or install a specific version
SENDMER_VERSION=v0.4.2 \
curl -fsSL https://raw.githubusercontent.com/bruceblink/sendmer/main/install.sh | bash
```

Default install path:

```bash
~/.sendmer/bin/sendmer
```

If needed, add it to your shell profile:

```bash
export PATH="$HOME/.sendmer/bin:$PATH"
```

Then verify:

```bash
sendmer --help
```

### Cargo

```bash
cargo install sendmer --locked
```

## Usage

### Send

```bash
sendmer send <file-or-directory>
```

This starts a temporary iroh provider, imports the selected file or directory, and prints a receive command with a ticket.

Example:

```bash
sendmer send ./my-folder
```

Typical output:

```text
imported directory my-folder, 12.3 MiB, hash <hash>
to get this data, use
sendmer receive blob:...
```

The sender keeps running until you stop it with `Ctrl+C`. When it stops, it shuts down the temporary provider and removes its temporary blob store under the system temp directory.

### Receive

```bash
sendmer receive <ticket>
```

This downloads the data and writes it into the current working directory by default.
Use `--output-dir <path>` to choose a different destination.

Example:

```bash
sendmer receive <ticket>
```

Receive-side data is staged in a temporary directory under the system temp directory and cleaned up after completion.

## Useful Options

Common options are available on both `send` and `receive`:

- `--no-progress`: disable CLI progress output
- `-v` / `-vv`: increase log verbosity
- `--relay <default|disabled|url>`: control relay usage
- `--magic-ipv4-addr <addr>`: bind a fixed IPv4 address
- `--magic-ipv6-addr <addr>`: bind a fixed IPv6 address
- `--show-secret`: print the secret key used for the current process

Receive-specific options:

- `--output-dir <path>`: set where received files are written (default: current working directory)

Send-specific options:

- `--ticket-type <id|relay-and-addresses|relay|addresses>`: control how much addressing information is embedded in the ticket
- `--format <hex|cid>`: choose how the imported hash is printed
- `--clipboard`: copy the generated `sendmer receive ...` command to the clipboard

## Library Usage

The crate also exposes a small library API:

```rust
use sendmer::{receive, send, ReceiveOptions, SendOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // let send_result = send(path, SendOptions::default(), None).await?;
    // let receive_result = receive(ticket, ReceiveOptions::default(), None).await?;
    Ok(())
}
```

The library re-exports:

- argument and option types
- transfer event types and `EventEmitter`
- `send` and `receive`
- `SendResult` and `ReceiveResult`

## Development

[DEVELOPMENT.md](DEVELOPMENT.md)

## License

[MIT](LICENSE)

[![Sponsor](https://img.shields.io/badge/sponsor-30363D?style=for-the-badge&logo=GitHub-Sponsors&logoColor=#EA4AAA)](https://github.com/sponsors/bruceblink) [![Buy Me Coffee](https://img.shields.io/badge/Buy%20Me%20Coffee-FF5A5F?style=for-the-badge&logo=coffee&logoColor=FFFFFF)](https://buymeacoffee.com/bruceblink)

## Contributors

<a href="https://github.com/bruceblink/sendmer/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=bruceblink/sendmer" alt="bruceblink/sendmer"/>
</a>

[img_crates]: https://img.shields.io/crates/v/sendmer.svg
[img_doc]: https://img.shields.io/badge/rust-documentation-blue.svg

[crates]: https://crates.io/crates/sendmer
[doc]: https://docs.rs/sendmer/
