# Code reviews

Written when the program was called oers; it is now kalila.

Two independent read-only reviews of commit `31aa188` (2026-11-13). Each was written by a
separate Claude agent that couldn't see the other's output. Nothing was built or run for either.

- [2026-11-13-claude.md](2026-11-13-claude.md): Claude's own review, aimed at
  correctness, concurrency, data-loss risks and FFI soundness.
- [2026-11-13-matklad.md](2026-11-13-matklad.md): Claude reviewing through the engineering
  values Aleksey Kladov (matklad) has published: architecture, asserted invariants,
  explicit bounds, and deterministic simulation testing. It isn't by him and doesn't quote him.

## Verification

The main findings were then checked against the source in the main session. Each one below
matches the code as described; the severities are the reviewers'. Timings and
memory-growth rates are estimates from the code's own sizes, not measurements. Nothing was
reproduced by running it.

## Combined priorities

Both reviews agree that the acquisition → recorder core is sound, and that no finding puts
recorded samples at risk in normal operation. In rough order of value:

| # | Finding | Found by | Where |
|---|---|---|---|
| 1 | If the recorder thread dies, nothing notices: `let _ = rec.send(..)`, `recording` stays true. Lossy workers dying (`Disconnected`) isn't counted either. A panic in the acquisition thread leaves a recording the GUI can't stop. | both | `engine/src/lib.rs:520,524,385-396` |
| 2 | Raw pyramid not reset on acquisition restart; after a recording stops, live blocks keep going into the pyramid. Review can show data that isn't in the file, and the budget can release the real recording's detail. | both | `engine/src/pyramid.rs:456-491`, `record.rs:417` |
| 3 | Backfill band-filters while holding the store lock that every GUI frame takes; the holder runs at SCHED_IDLE (priority inversion). | Claude | `engine/src/pyramid.rs:607-626` |
| 4 | DAC routes, threshold → TTL out, CAR sets and audio channels are stored as global electrode indices, and silently move to other electrodes after a layout change (e.g. the 16-ch toggle). | Claude | `app/src/config.rs:68,310` |
| 5 | Review and backfill treat file offset as sample number, so everything after a gap is shifted against the TTL lanes and notes. | both | `app/src/review.rs:91`, `pyramid.rs:602` |
| 6 | `App::spike_new` grows without bound while the main window is hidden. | matklad | `app/src/app.rs:227,860` |
| 7 | Changing the recording root between recordings has no effect until a new session starts. | Claude | `engine/src/record.rs:496-511` |
| 8 | The block pool (256) is smaller than the queues it feeds, so the RT thread allocates whenever a consumer lags. | both | `engine/src/lib.rs:489` |
| 9 | A config that fails to parse is replaced by defaults, which are then saved over it. | both | `app/src/config.rs:388-395` |
| 10 | Stop and port scan can hang on a board that goes silent (unbounded join and read). | both | `lib.rs:416`, `oni_board.rs:532-550,1062` |
| 11 | A repeated or backwards Rhythm counter adds ~4.3e9 to `lost` and offsets sample numbers. | Claude | `rhythm/src/parser.rs:84-99` |
| 12 | Write errors and `finish`: no best-effort finalise; metadata written before the data is synced. | both | `record.rs:370-379,461-466` |
| 13 | Session directories can collide within one second, and `File::create` truncates. | Claude | `record.rs:499-512` |

Structural recommendations (matklad review): an ARCHITECTURE.md with an invariant list,
assertions in `write_block` and the parser, `bounded(rec_limit)` for the recorder channel,
a deterministic simulation test that runs the simulator through the real parser and
recorder, and a `views/model.rs` with table tests. Both reviews flag the duplicated CAR
window.
