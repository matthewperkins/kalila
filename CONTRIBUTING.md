# Contributing to kalila

kalila is GPL-3.0-or-later (see [LICENSE](LICENSE)). Report problems and send changes through the
[issue tracker](https://github.com/matthewperkins/kalila/issues).

## Build and test

You need Rust 1.85 or newer (CI builds with 1.95.0), a C compiler, clang, and the ALSA and
PipeWire headers. On Debian or Ubuntu:

```sh
sudo apt-get install -y build-essential pkg-config libasound2-dev libpipewire-0.3-dev libclang-dev
```

Then, from the repository root:

```sh
cargo build --release --workspace
cargo test --release --workspace
./target/release/kalila --sim      # the GUI on the simulated board
```

The simulation test (`crates/engine/tests/simulation.rs`) runs 200 seeds by default, about 6 s on
Linux and about 135 s on macOS. `KALILA_SIM_SEED=<n>` reruns one seed.

Format code with `cargo fmt` (`rustfmt.toml`: 120 columns). CI builds and tests on Linux and
macOS, and on Linux also checks the formatting and runs clippy with warnings as errors:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Tests and the board

Tests and CI never touch a board; the simulator stands in for one. A contribution must not need
an acquisition board to be tested or reviewed. If a change can only be shown to work on hardware,
say so in the pull request rather than leaving that unstated.

## Changes that reach the board

A change that alters what is sent to the board must say, in its pull request, which registers
or USB requests it writes.

Every FT600 control transfer goes through `Ft600::control_transfer`, which refuses anything not in
`ALLOWED_REQUESTS` in `crates/ft600/src/chip.rs`. That list is not to be widened without
discussion first. The chip-configuration write (0x40 0xcf) is refused on purpose. The tests
check the list and scan the workspace for control transfers made anywhere else, so a new one
fails them.

## Reporting a board problem

Use the bug report template, which asks for:

- the kalila version or commit, and whether it is a release package, the Mac app or a source build;
- the board's gateware version and board ID, and the headstage on each port;
- the Linux distribution (or macOS version), and the GPU;
- a log taken with `KALILA_USB_TRACE=1 RUST_LOG=debug` set. From a terminal the log goes to
  stderr. A desktop launch on Linux writes `~/.local/state/kalila/kalila.log`; an app started
  from Finder on macOS writes `~/Library/Logs/kalila/kalila.log`.

## Recordings and personal data

Do not attach recordings, or anything with personal data, to a public issue. Describe the
problem instead. Logs and config files can contain paths and names, so check them before you
attach them.
