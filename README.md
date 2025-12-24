# Sendmer

This project is based on [n0-computer/sendme v0.31.0](https://github.com/n0-computer/sendme/commit/6e50167a8a4d45736179cce3d8d5fd803c87c24e)

Crate: <https://crates.io/crates/sendmer>
Documentation: <https://docs.rs/sendmer>

It is an example application using [iroh](https://crates.io/crates/iroh) with
the [iroh-blobs](https://crates.io/crates/iroh-blobs) protocol to send files and
directories over the internet.

This repository serves two purposes: it provides a command-line application
(CLI) for end users, and it also exposes library APIs so other Rust projects
can reuse the iroh-based transfer functionality.

It is also useful as a standalone tool for quick copy jobs.

Iroh will take care of hole punching and NAT traversal whenever possible,
and fall back to a relay if hole punching does not succeed.

Iroh-blobs will take care of [blake3](https://crates.io/crates/blake3) verified
streaming, including resuming interrupted downloads.

sendmer works with 256 bit node ids and is, therefore, location transparent. A ticket
will remain valid if the IP address changes. Connections are encrypted using
TLS.

# Installation

```
cargo install sendmer
```

# Usage

## Send side

```
sendmer send <file or directory>
```

This will create a temporary [iroh](https://crates.io/crates/iroh) node that
serves the content in the given file or directory. It will output a ticket that
can be used to get the data.

The provider will run until it is terminated using `Control-C`. On termination, it
will delete the temporary directory.

This currently will create a temporary directory in the current directory. In
the future this won't be needed anymore.

### Receive side

```
sendmer receive <ticket>
```

This will download the data and create a file or directory named like the source
in the **current directory**.

It will create a temporary directory in the current directory, download the data
(single file or directory), and only then move these files to the target
directory.

On completion, it will delete the temp directory.

All temp directories start with `.sendmer-`.

develop guid: [DEVELOPMENT.md](DEVELOPMENT.md)

## Examples

### Basic send

```bash
# publish a directory
sendmer send ./my-folder
```

The command prints a ticket you can share. Keep the provider running until
the receiver finishes (Ctrl-C to stop).

### Basic receive

```bash
# download using a ticket
sendmer receive <ticket>
```

By default the data is downloaded into the current directory using a
temporary `.sendmer-...` folder and moved into place when complete.

### Disable progress output

```bash
sendmer send ./file --no-progress
sendmer receive <ticket> --no-progress
```

### Use as a library (Rust)

You can embed `sendmer` in other Rust programs by calling the exported
library functions `start_share` and `download`:

```rust
use sendmer::{start_share, download, SendOptions, ReceiveOptions};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
	// start_share(path, SendOptions { ... }, Some(event_emitter)).await?;
	// download(ticket, ReceiveOptions { ... }, Some(event_emitter)).await?;
	Ok(())
}
```
