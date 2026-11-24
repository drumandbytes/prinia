# Install

## Requirements

An API key for a provider (or a local model server such as Ollama). Nothing else: prinia is a single binary that calls the provider's API directly.

## Install script

```bash
curl -fsSL https://github.com/drumandbytes/prinia/releases/latest/download/install.sh | sh
```

The script picks the release for your OS and architecture, verifies it against the published checksums and installs `prinia` to `~/.local/bin`. It refuses to install on a checksum mismatch.

| Variable | Effect |
| :--- | :--- |
| `PRINIA_VERSION=v0.1.0` | Install a specific release instead of the latest |
| `PRINIA_INSTALL_DIR=/usr/local/bin` | Install somewhere else |
| `PRINIA_DOWNLOAD_URL=...` | Download from a mirror |

## Updating

```bash
prinia update --check     # is a newer release out?
prinia update             # install it in place
```

`prinia update` downloads the installer from the latest release and runs it into the folder the current binary lives in, so it does exactly what the install script does, including the checksum check; the installer is the only copy of that logic. Nothing is checked or downloaded unless you run the command.

It refuses for a binary installed by Homebrew or cargo (use `brew upgrade prinia` or `cargo install`) and for a development build. It is not available on Windows. Running the install script again always works as well, for any version.

Note that the checksum comes from the same release as the binary: it catches a corrupt download, not a compromised release.

## Homebrew

```bash
brew install drumandbytes/tap/prinia
```

## From source

```bash
git clone https://github.com/drumandbytes/prinia
cd prinia
cargo install --path .
```

Building needs a Rust toolchain and a C compiler (SQLite and zstd are compiled from source). No OpenSSL or other system libraries are needed.

## Platforms

| | Prebuilt binary | Tested |
| :--- | :--- | :--- |
| macOS, arm64 and x86-64 | yes | yes |
| Linux, arm64 and x86-64 | yes (glibc 2.39 or newer: Ubuntu 24.04, Debian 13, Fedora 40) | yes |
| Windows | no | in CI only |

On older Linux systems, build from source. On Windows, build from source (`cargo install --path .`). It builds and passes the test suite in CI on every change, but nobody has used it by hand yet: the command approval prompt (`CONIN$` / `CONOUT$`) and the Credential Manager key store (`keychain = true`) have not run on a real Windows console.

The command is `prinia`. If you want something shorter, alias it in your shell: `alias ai=prinia`.

## Upgrade and uninstall

Run the install script again, or `brew upgrade prinia`. To uninstall, remove the binary. Your history and key are separate; see [History](history.md#where-things-are-stored) for where they live.
