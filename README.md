# Sendmer [![][img_crates]][crates] [![][img_doc]][doc]

English | [中文](README_ZH.md)

Sendmer is a small CLI and reusable Rust library for sending files and directories over the internet with [iroh](https://crates.io/crates/iroh) and [iroh-blobs](https://crates.io/crates/iroh-blobs).

It is based on [n0-computer/sendme v0.31.0](https://github.com/n0-computer/sendme/commit/6e50167a8a4d45736179cce3d8d5fd803c87c24e), with a codebase that has been gradually reorganized into a clearer library + CLI structure.

## Features

- Send a file or an entire directory with a single command
- NAT traversal and hole punching via iroh, with relay fallback when needed
- Verified streaming with blake3 through `iroh-blobs`
- Reusable Rust API via exported `send` and `receive` functions
- Versioned transfer events with ordered sessions and structured errors
- Optional shared sender upload limit, CLI progress output, and clipboard helper

sendmer uses 256-bit node IDs, so tickets remain valid even if IP addresses change during a session. Connections are encrypted with TLS.

## Installation

### Windows (PowerShell)

```powershell
# Install latest release
iwr https://raw.githubusercontent.com/bruceblink/sendmer/main/install.ps1 -useb | iex

# Or install a specific version
$env:SENDMER_VERSION="v0.9.0"
iwr https://raw.githubusercontent.com/bruceblink/sendmer/main/install.ps1 -useb | iex
```

For Git Bash, MSYS2, or Cygwin, use the shell installer instead. It requires `unzip`:

```bash
curl -fsSL https://raw.githubusercontent.com/bruceblink/sendmer/main/install.sh | bash
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
SENDMER_VERSION=v0.9.0 \
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

Receive-side data uses a temporary iroh store by default. Final files or directories are published only after every export reports completion; a failed receive cleans its temporary data without replacing an existing target. To keep verified ranges after a failure or cancellation, opt in with `--cache-dir <path>`; a later receive for the same content can reopen that store, while a successful export removes the completed cache entry.

Directories must contain regular files. sendmer rejects empty directories, directories with empty subdirectories, and symbolic links because the current collection format cannot preserve them safely.

## Useful Options

Common options are available on both `send` and `receive`:

- `--no-progress`: disable CLI progress output
- `--json-events`: emit versioned transfer event envelopes as flushed JSON Lines on stdout instead of a progress bar; logs and human-readable command output remain on stderr
- `-v` / `-vv`: increase log verbosity
- `--relay <default|disabled|url>`: control relay usage
- `--magic-ipv4-addr <addr>`: bind a fixed IPv4 address
- `--magic-ipv6-addr <addr>`: bind a fixed IPv6 address
- `--show-secret`: print the secret key used for the current process
- `--format <hex|cid>`: choose the hash format; it affects the hash printed by `send` and is ignored by `receive`

Receive-specific options:

- `--output-dir <path>`: set where received files are written (default: current working directory)
- `--retry-limit <count>`: maximum attempts for the blob download phase (default: `3`)
- `--retry-backoff-ms <milliseconds>`: delay between blob download attempts (default: `250`)
- `--connect-timeout-ms <milliseconds>`: optional timeout for each sender connection attempt
- `--metadata-timeout-ms <milliseconds>`: optional timeout for a collection metadata request
- `--download-idle-timeout-ms <milliseconds>`: optional timeout while the download stream produces no updates
- `--cache-dir <path>`: opt into a persistent, content-addressed receive cache for cross-process resume
- `--cache-ttl-seconds <seconds>`: record the cache entry lifetime for pruning (default: `604800`; requires `--cache-dir` to take effect)

### Receive Safety

The only conflict policy in this release is **fail**. If the destination root already exists as a file, directory, or symbolic link, receive fails and leaves it unchanged; sendmer never merges into, renames, skips, or overwrites an existing target. Collections with multiple top-level roots are also rejected because they cannot be committed as one atomic destination.

Retry reuses data already obtained by the current receive process. Cross-process resume is opt-in: configure the same private `--cache-dir` for a later receive of the same content. Failed or cancelled receives preserve verified ranges, successful receives remove their completed cache entry, and the sender still needs to be reachable.

Expired entries are pruned automatically when a persistent cache is opened. You can run the same maintenance explicitly:

```bash
sendmer cache prune --cache-dir <path>
```

Pruning uses each entry's recorded TTL, skips active entries, and preserves malformed or future-schema data instead of guessing. The cache format, privacy boundary, and interruption/restart E2E are defined in the [mainline design and development plan](docs/MAINLINE.md).

Send-specific options:

- `--ticket-type <id|RelayAndAddresses|relay|addresses>`: control how much addressing information is embedded in the ticket; the combined mode currently uses the exact `RelayAndAddresses` spelling
- `--max-upload-rate <bytes-per-second>`: optionally cap the sender's total payload upload rate shared by all receivers; protocol overhead is not included
- `--max-receivers <count>`: optionally cap the number of receiver connections active at the same time; a disconnected receiver releases its slot
- `--max-files <count>`: optionally reject a share before startup when it contains more regular files than the configured limit
- `--max-total-size <bytes>`: optionally reject a share before startup when regular files exceed the configured total payload size
- `--clipboard`: copy the generated `sendmer receive ...` command to the clipboard (available in the default `clipboard` feature build)

## Library Usage

The crate also exposes a small library API. New integrations should use `SendHandle` so the
temporary provider is closed explicitly without depending on iroh internals:

```rust
use std::path::PathBuf;
use sendmer::{send_handle, SendOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let handle = send_handle(PathBuf::from("./my-file"), SendOptions::default(), None).await?;
    println!("sendmer receive {}", handle.ticket());
    handle.close().await?;
    Ok(())
}
```

The legacy `send` function and `SendResult::shutdown` remain available for compatibility. A receiver
can use `receive` or `receive_with_cancellation` with `ReceiveOptions`.

`TransferEvent` is now the versioned `TransferEventEnvelope` alias. Each send or receive session has
one random `session_id`, strictly increasing `sequence` values, an explicit phase, and at most one of
completed, failed, or cancelled. Failed events contain `TransferErrorCode`, failure phase,
retryability, and a safe message. Consumers should match `event.event` and must not parse error text.
See the [v0.8.0 migration guide](docs/V0_8_MIGRATION.md) and the buildable
[`event_consumer` example](examples/event_consumer.rs).

The library re-exports:

- argument and option types
- transfer event types and `EventEmitter`
- `send`, `send_handle`, `receive`, and `receive_with_cancellation`
- preferred `SendHandle`, plus legacy `SendResult` and `ReceiveResult`

## Development

- [DEVELOPMENT.md](DEVELOPMENT.md)
- [Mainline Design and Development Plan](docs/MAINLINE.md)
- [v0.8.0 Migration Guide](docs/V0_8_MIGRATION.md)

## License

[GNU Affero General Public License v3.0 only (AGPL-3.0-only)](LICENSE)

[![Sponsor](https://img.shields.io/badge/sponsor-30363D?style=for-the-badge&logo=GitHub-Sponsors&logoColor=#EA4AAA)](https://github.com/sponsors/bruceblink) [![Buy Me Coffee](https://img.shields.io/badge/Buy%20Me%20Coffee-FF5A5F?style=for-the-badge&logo=coffee&logoColor=FFFFFF)](https://buymeacoffee.com/bruceblink)

## Contributors

<a href="https://github.com/bruceblink/sendmer/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=bruceblink/sendmer" alt="bruceblink/sendmer"/>
</a>

[img_crates]: https://img.shields.io/crates/v/sendmer.svg
[img_doc]: https://img.shields.io/badge/rust-documentation-blue.svg

[crates]: https://crates.io/crates/sendmer
[doc]: https://docs.rs/sendmer/
