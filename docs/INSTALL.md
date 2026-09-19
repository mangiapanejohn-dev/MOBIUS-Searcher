# Install

MØBIUS is a single binary, `mobius-searcher`. Pick one way to install it.

| Method | Command | Needs |
|---|---|---|
| **Install script** (macOS, Linux) | `curl -fsSL https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/scripts/install.sh \| sh` | `curl`, `tar` |
| **Install script** (Windows) | `irm https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/scripts/install.ps1 \| iex` | PowerShell |
| **npm** | `npm install -g mobius-searcher` | Node.js 18+ |
| **Prebuilt binary** | download from [Releases](https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/releases) | nothing |
| **Cargo** | `cargo install --git https://github.com/mangiapanejohn-dev/MOBIUS-Searcher mobius-searcher --locked` | Rust ≥ 1.91 and a C compiler |
| **From source** | `git clone` + `cargo build --release` | Rust ≥ 1.91 and a C compiler |

Every installer downloads the release archive for your OS and CPU, verifies it
against the release's `SHA256SUMS`, and refuses a mismatch.

Prebuilt binaries:

| Platform | Asset |
|---|---|
| macOS, Apple silicon | `mobius-searcher-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `mobius-searcher-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc ≥ 2.35: Ubuntu 22.04+, Debian 12+) | `mobius-searcher-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 (same glibc) | `mobius-searcher-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x64 | `mobius-searcher-x86_64-pc-windows-msvc.zip` |

Asset names carry no version, so `/releases/latest/download/<asset>` always
points at the newest release. Each archive holds a `mobius-searcher-<target>/`
folder with the binary, `README.md`, `CHANGELOG.md`, the licenses and
`config/mobius.toml`. MØBIUS is developed on macOS (Apple silicon); the other
builds are compiled and tested in CI but have seen little real use yet —
reports are welcome.

## Install script

macOS / Linux — installs to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/scripts/install.sh | sh
```

Windows (PowerShell) — installs to `%LOCALAPPDATA%\Programs\mobius` and adds it
to your user `PATH`:

```powershell
irm https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/scripts/install.ps1 | iex
```

Options (environment variables, both scripts): `MOBIUS_VERSION=v0.1.0` (a
specific release), `MOBIUS_INSTALL_DIR` (where to put the binary),
`MOBIUS_DOWNLOAD_BASE` (a mirror of the release files).

## npm

```bash
npm install -g mobius-searcher
```

```bash
mobius-searcher --doctor
```

The package downloads the release binary that matches your OS and CPU during
install, checks it against `SHA256SUMS`, and puts `mobius-searcher` on your
`PATH`. With `--ignore-scripts` it downloads on the first run instead.

## Prebuilt binary

macOS / Linux (replace the asset name with yours):

```bash
curl -fsSLO https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/releases/latest/download/mobius-searcher-aarch64-apple-darwin.tar.gz
```

```bash
tar -xzf mobius-searcher-aarch64-apple-darwin.tar.gz
```

```bash
sudo mv mobius-searcher-aarch64-apple-darwin/mobius-searcher /usr/local/bin/
```

Windows: download the `.zip`, extract it, and put the folder containing
`mobius-searcher.exe` on your `PATH`.

## Cargo

```bash
cargo install --git https://github.com/mangiapanejohn-dev/MOBIUS-Searcher mobius-searcher --locked
```

## From source

```bash
git clone https://github.com/mangiapanejohn-dev/MOBIUS-Searcher
```

```bash
cd MOBIUS-Searcher && cargo build --release
```

```bash
./target/release/mobius-searcher
```

## Where files live

| What | Path |
|---|---|
| Your settings | `~/.config/mobius/config.toml` (or `--config`, `$MOBIUS_CONFIG`) |
| Your secrets | `~/.config/mobius/.env` (`chmod 600`) |
| Bot wallets created by `--setup` | `~/.config/mobius/wallets/` (`0600`) |
| Recordings (`mobius.sqlite`) and latency reports (`bench/`) | the per-user data directory: `$MOBIUS_HOME/data` if `MOBIUS_HOME` is set, else `$XDG_DATA_HOME/mobius`, else `~/.local/share/mobius` |

On Windows the same paths live under `%USERPROFILE%\.config\mobius\` and
`%USERPROFILE%\.local\share\mobius`. Set `[general] data_dir` to use another
directory (a relative path is relative to where you start the program) or pass
`--db PATH`. `$MOBIUS_HOME` moves the whole per-user directory. Nothing is
written into the installation or the repository.

## Terminals

The UI runs in any modern terminal. For the best result use a GPU-accelerated
terminal with truecolor and an image protocol — then the logo shows as the real
image instead of character cells.

| OS | Recommended | Also good |
|---|---|---|
| macOS | [Ghostty](https://ghostty.org/) | iTerm2, WezTerm, kitty |
| Linux | [Ghostty](https://ghostty.org/) | kitty, WezTerm |
| Windows | [Warp](https://www.warp.dev/) | Windows Terminal, WezTerm |

Ghostty (macOS and Linux) and kitty speak the kitty graphics protocol; iTerm2
and WezTerm the iTerm2 protocol; Windows Terminal supports sixel; Warp
advertises the kitty and iTerm2 protocols. The Windows terminals have not been
tried with MØBIUS yet. Terminal.app and other terminals without an image
protocol show the logo in character cells; everything else works the same.

## Next

```bash
mobius-searcher --doctor    # checks config, secrets (names only) and every endpoint
```

```bash
mobius-searcher             # first run opens the setup; PAPER by default
```

Then see [USAGE.md](USAGE.md).

## Uninstall

`npm uninstall -g mobius-searcher`, `cargo uninstall mobius-searcher`, or
delete the binary (`~/.local/bin/mobius-searcher` from the install script). Your settings and recordings stay in `~/.config/mobius/`
and the data directory until you remove them.
