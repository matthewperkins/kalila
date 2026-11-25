# kalila improvement plan (2026-11-25)

Written from a read of the tree at b94a5e9 (updated the same day for 9306bf9: the macOS
port, `ci.yml` and the v0.1.0 release had landed meanwhile) and of the two reviews in `docs/reviews/`. The
13 ranked defects in those reviews are fixed; what is left is their structural advice, a few
small defects, and gaps that have opened since (CI, the new FT600 driver, the public repo).
Every "open" claim below was checked against the code on 2026-11-25, but each package starts
by re-checking its own, since line numbers move.

The plan is split into packages sized for one sub-agent each. Each package names the files
it owns, so packages in the same wave can run in parallel (in separate worktrees) without
editing the same file.

**Status, 2026-11-25:** waves 1–3 are done and merged (see CLAUDE.md, "Improvement plan,
waves 1–3"); 3e ended as an analysis and a smaller change. Wave 4 and the rig checks remain.

## Rules for every sub-agent

- Read `CLAUDE.md` first. Linux first, with a macOS port behind small `cfg` seams: code under
  `cfg(target_os = "macos")` cannot be compiled here, so leave it alone unless the package
  names it, and say so in the report when a change reaches it. Lower-case names.
- The tree is formatted with `cargo fmt` (`rustfmt.toml`, 120 columns); run it before reporting.
- **Never open the board, never launch the GUI or the simulator window, never play audio.**
  Verification is `cargo test` and `cargo clippy` only. `kalila-headless` has no `--help`:
  running it starts the simulator with sound.
- Builds: `nice -n 19 cargo … --offline`. Tests: `nice -n 19 cargo test --release --offline --workspace`.
- Do not commit or push. Leave the work in the worktree and report: what changed, what was
  run with its output, what was not verified.
- No new dependencies without saying so in the report.
- Behaviour must not change unless the package says it does. In particular nothing may change
  what is written to a recording, or which registers and USB requests reach the board.
- Stop and report instead of guessing when the code does not match the package's description.

## Model tiers

- **light** (Haiku): mechanical, local, checked by the compiler and existing tests.
- **medium** (Sonnet): a refactor or new tests within one module, with judgement.
- **heavy** (Opus/Fable): the acquisition → recorder path, hardware, audio texture,
  concurrency, or design work. Reviewed in the main session before it is kept.

## Wave 0: decisions for the user (no agent)

1. **rustfmt:** decided 2026-11-25, `max_width = 120`, one formatting commit before wave 1.
2. **`panic = "abort"`** (review suggestion): not recommended. The acquisition drop guard and
   the FT600 driver's `catch_unwind` exist so that a panic still finalises the recording.
3. Which features in wave 4 are wanted, and in what order.

## Wave 1: baseline (parallel; light unless marked)

| id | package | owns | tier |
|---|---|---|---|
| 1a | Toolchain and lints | `rust-toolchain.toml`, `Cargo.toml`, the 6 lint sites | light |
| 1b | CI on push | `.github/workflows/ci.yml` | light |
| 1c | Shared constants | `rhythm/src/{types,parser,sim_board}.rs`, `headless.rs` | light |
| 1d | GPU struct checks and dead fields | `app/src/{traces,spikes}.rs`, the two `.wgsl` | medium |
| 1e | `bytemuck` in the recorder | `engine/src/record.rs` (unsafe casts only) | medium |
| 1f | ARCHITECTURE.md | `ARCHITECTURE.md` (new) | medium |
| 1g | Project files for a public repo | `CHANGELOG.md`, `CONTRIBUTING.md`, `.github/ISSUE_TEMPLATE/` | light |

- **1a.** Pin Rust 1.95.0 in `rust-toolchain.toml` (the version `packaging/ci-build.sh`
  uses; keep the two in step with a comment in each). Add `[workspace.lints]` with clippy
  at warn, and `lints.workspace = true` in each crate. Fix the six current warnings
  (`needless_range_loop` ×2, `field_reassign_with_default` ×2, `manual_is_multiple_of`,
  one in the app's tests). Done when `cargo clippy --workspace --all-targets` is silent.
- **1b.** `ci.yml` exists since 2026-11-25 (build and test on macOS and Linux). Add to its
  Linux job `cargo fmt --all --check` and clippy with `-D warnings` over
  `--workspace --all-targets`. Depends on 1a for a clean clippy.
- **1c.** One definition each for: the headstage slot names (three copies), ADC volts per
  bit (`parser.rs` const, literals in `sim_board.rs`), chip IDs (split between `parser.rs`
  and `oni_board.rs`). Do **not** edit `oni_board.rs` beyond `use` lines and the slot-name
  const; the `0.195` literal in `apply_dac` belongs to 2d.
- **1d.** `const` asserts for the size of `IngestU`, `CutU`, `RenderU` and the spike
  shader's `U`, matching their WGSL twins. One `const` for the filter-section limit,
  mirrored in WGSL with a comment; `FilterBank::new` asserts instead of truncating. Remove
  `IngestU::nsec` (always 0, never read by the shader) and shrink `fsec: [u32; 4]` to the
  two filters that exist, in Rust and WGSL together. Cannot be verified without a GPU run:
  the report must say so, and the main session checks it on the simulator with a UI script.
- **1e.** `NpyFile::write<T: Copy>` becomes `T: bytemuck::Pod` with `cast_slice`; the two
  hand-written `from_raw_parts` casts in `write_block` likewise. The simulation test and
  the recorder tests must pass unchanged (they compare bytes on disk).
- **1f.** One to two pages: the crate map, the threads and queues with their bounds, the
  invariants (the list in `docs/reviews/2026-11-13-matklad.md` §2, corrected for what has
  changed: invariant 7 is now "through `RecSource::gaps`"), the hardware-safety rules
  (`ALLOWED_REQUESTS`, volatile registers only), and what is deliberately absent. Every
  statement cites a file and function. No code changes.
- **1g.** A changelog starting at 0.1.0 drawn from `git log`; a short CONTRIBUTING (build,
  tests, the no-board rule for CI, how to report a board problem: gateware version, board
  ID, `KALILA_USB_TRACE=1` log); bug-report issue template asking for the same.

## Wave 2: engine and driver hardening (parallel; files do not overlap)

| id | package | owns | tier |
|---|---|---|---|
| 2a | Recorder queue and durability | `engine/src/lib.rs` (rec channel), `engine/src/record.rs` | heavy |
| 2b | Signals | `app/src/main.rs`, `headless.rs`, a new `engine/src/signals.rs` | medium |
| 2c | CPU filter path | `engine/src/dsp.rs`, `engine/src/pyramid.rs` (`band_filter` only) | medium |
| 2d | Board settings on failure | `rhythm/src/oni_board.rs` | heavy |
| 2e | Audio callback | `engine/src/audio.rs` | heavy |
| 2f | FT600 driver under test | `crates/ft600/` | heavy |
| 2g | Golden register test | `rhythm/src/registers.rs` (tests), `rhythm/testdata/` | light |

- **2a.** Three changes, each its own commit-sized diff, after 1e:
  1. The recorder channel is `unbounded()` and held to `rec_limit` by a 200 µs polling loop
     in `acquisition_loop`. Make it `bounded(rec_limit)` with a blocking send, keeping the
     stall measurement (`rec_backpressure_us`) and the stop-on-recorder-death behaviour.
     Start/Stop/Message must never be lost and must keep their order relative to data.
  2. Turn the body of `run_recorder` into `Recorder::on(&mut self, msg)` and add a table
     test of message sequences (Data after Stop, Start without acquisition, Message before
     the first block, AcquisitionStopped while recording, write error mid-block). Count
     data that arrives after Stop.
  3. `sync_data()` on `continuous.dat` every few seconds from the recorder thread
     (sequential, cheap), so a power loss costs seconds instead of the kernel's writeback
     window. Measure the added stall in the simulation test's terms and report it.
  Also fix the stale doc at the top of `record.rs` ("receives every block while acquiring";
  it receives them only while recording). `acquisition_loop`'s 13 arguments become a
  `Fanout` struct and the unused `_health` goes. Done when the simulation test (all seeds,
  including disk-full 1000–1059) and `a_panic_in_acquisition_finalises_the_recording` pass.
  **Then the user runs a rig soak**; nothing replaces that.
- **2b.** SIGTERM and SIGINT stop the recording and acquisition and let files be finalised
  (today a logout or `kill` skips `on_exit`: npy headers up to 1 s stale, up to 8 MB of
  buffered data lost). A flag set in the handler (`libc::sigaction`, no new crate), polled
  in `App::logic` and in the headless loop. Test: a headless child process on the simulator
  with `--no-audio`, sent SIGTERM mid-recording, leaves files the existing checker accepts.
- **2c.** First the oracle test the review asked for: one synthetic block through the three
  CPU implementations of "CAR and band-pass" (`dsp_thread`, `pyramid::band_filter`, the
  slope trigger) must agree within float error. Then `FilterBank::process(data, nch, car,
  out)` that resolves CAR once per block, and move `dsp_thread` and `band_filter` to it.
  The audio tests and the pyramid brute-force test must pass unchanged. Delete
  `BandSpec::LFP` and `Biquad::notch` (no callers). Count spike-queue drops
  (`spikes.try_send` in `lib.rs` — coordinate that one line with 2a, or hand it to 2a).
- **2d.** `apply_settings` records the new settings before the registers are written, so
  after a failed write the GUI believes Apply succeeded. Record them after, or mark the
  board's state unknown on error. Replace the `0.195` literal with `ELECTRODE_UV_PER_BIT`
  and explain or remove the `.abs()` in the DAC threshold (the value written must be
  identical for every threshold: prove it with a test over the full range). **Hardware:**
  no new register writes and no change in their order; the report lists every write it
  moved. `regdump` output before and after must be identical.
- **2e.** The cpal callback `try_lock`s a mutex around the ring consumer only because
  `build` may run twice; give the callback the consumer outright. `audio-feed` and `dsp`
  clone their settings (with `Vec`s) about 940 times a second: clone only when a
  generation counter changes. All seven audio tests must pass unchanged (they encode the
  texture rule). Not verifiable by ear by an agent: the user listens on the rig.
- **2f.** `driver.rs` has four tests and its USB side is only exercised on the rig. Put the
  chip behind a small trait and add a scripted fake FT600 (announcements, short reads,
  timeouts, a stall, an unplug mid-read), then drive `Ctx` through open, register
  read/write, a stream of frames, and close. Seeded, in the style of
  `engine/tests/simulation.rs`. The allowlist tests and the workspace scan must still pass,
  and the fake must not add a second path to a control transfer.
- **2g.** Check in today's `regdump` output and compare it in a test, so any change to the
  register lists is visible in a diff. This locks the port; it does not re-prove it against
  Intan's C++ (the README's "byte-identical" claim was checked by hand). Rebuilding that
  C++ harness is a separate, optional job.

## Wave 3: GUI structure (sequential: these share `views.rs` and `app.rs`)

All medium. Each is behaviour-preserving and is checked by the main session with a UI
script on the simulator (with the user's OK) before the next starts.

- **3a. One CAR window.** `ViewsShared::car_palette` and `App::car_palette` are about 80
  near-identical lines each, each with a `CarTarget` arm that cannot run. Extract
  `car_sets_ui(ui, groups, &mut ViewCar) -> bool` and delete `CarTarget`.
- **3b. `views/model.rs`.** Move the pure functions of (layout, settings): `view_rows`,
  `chan_key`, `chan_filters`, `used_filters`, `set_row_filter`, `view_chinfo`, `band_key`,
  `pyramid_columns`, `sweep_hash`. Add table tests: a layout with a 16-ch RHD2132, an
  RHD2216, AUX and ADC, against rows, `chinfo` flags and band key. Precompute channel keys
  per layout (today `chan_key` formats a string and rescans, per channel, per frame).
- **3c. Review decisions apart from drawing.** In `frame.rs::review`, separate "pyramid,
  direct read, append or wait" into a function returning a plan, tested from a table like
  `ReviewView::accept`.
- **3d. `App` into parts.** `App` has 32 fields. Move the spike-tile state and the
  output-sync state into `SpikeGrid` and `Outputs`, each in its own file with its own
  `ingest`/`ui`. Give `Engine::last_error` one writer (`Engine::report`). Remove the 50 ms
  sleep between `stop_recording` and `stop_acquisition` (`app.rs`; the recorder queue is
  FIFO, so it orders nothing) — after 2a.
- **3e. Outputs have one owner.** `ttl_output_mode`, `dac_hpf_*` and `noise_slicer` live in
  `OutputSettings`, `BoardSettings` and `Running::settings`, with copying code between
  them. Heavy, and hardware-adjacent (it decides what `sync_outputs` sends): the report
  lists the live commands sent at start and on each edit, before and after.
- **3f. A note finished after its recording ended** is dropped from the recording although
  it was stamped inside it. Decide by the stamp, not by `is_recording()` at Enter.

## Wave 4: features (heavy; each needs the user's go-ahead and a design note first)

1. **Faster direct reads in review**, then **zero-phase review** — the order agreed on
   2026-11-11 and not yet built (parallel min/max for Raw rows, Band rows filtered in
   parallel time segments, a whole fill uploaded in one frame; target 6 s span in 50–60 ms).
2. **Triggering in review**, and the combined spike/laser latency view.
3. **GUI audio underruns on the simulator** (about 7/s against 0.25/s headless, cause
   unknown since 2026-11-11). Investigation, needs sound and the user present.
4. **Crash-truncation in the simulation test** (kill the recorder at a seeded byte; the
   files must still open and hold a correct prefix).
5. **Spike file writing**; impedance stays unported unless asked for.
6. **A user guide** beyond the README, now that others will install from packages.

## Only the user can do (rig)

- Stop/Start and Apply-while-acquiring in the GUI with the new FT600 driver.
- Outputs panel on a scope and the Grass AM10; DAC routing in 16-ch mode.
- Port-scan timeout with a headstage unplugged; ADC rows on the board; CAR on real data.
- A soak after wave 2 (2a, 2e, 2f change the acquisition and audio paths).
- Tell dimna's session that its board check should include `kalila-headless`.

## Order and merging

Wave 1 packages are independent of each other except 1b after 1a. Wave 2 packages are
independent except the one `spikes.try_send` line (2a/2c) and 2a after 1e. Wave 3 is one at
a time. The main session merges each package, runs the whole test suite and clippy on the
merged tree, and asks before any simulator run. Nothing is committed until the user says so.
