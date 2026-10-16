# Development-only experiment controls

Build with `cargo build --release --offline --locked --features development-options --target-dir target-dev`. These options are rejected by the ordinary product build. They exist for fault injection and causal experiments, not caller policy. The production build always manages its shared budgets internally.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--pod UUID`, `--output NEW_DIR` | Required | Local Pod UID and new destination |
| `--format tree\|json`, `--full-ids` | `tree`, off | Human tree or machine summary |
| `--jobs 1..16` |4 | Capture workers |
| `--writers 1..16`, `--sync-jobs 1..16` |2,1 | Independent write/finalize workers |
| `--memory-mib N`, `--buffer-mib N` |512,512 | Snapshot pool and per-target upper limit |
| `--buffer-policy overlap\|latency` | `overlap` | Overlap ceiling or full-pool admission |
| `--pending-mib N` |1024 | Output backlog credits through fsync/publication |
| `--shared-budget FILE`, `--global-jobs 1..16` | Off,4 | Daemon-free cooperative node credits |
| `--global-pending 1..64` |4 | Unfinished targets retained through durable publication |
| `--sync-coordination node\|none` | `node` | One shared core-fsync slot when a shared budget path is provided |
| `--stream` | Off | Force bounded scratch streaming while stopped |
| `--stop-timeout-ms N` |5000 | Cooperative stop cap,1..60000ms |
| `--timeout-ms N` |60000 | Capture/write deadline after local queue admission,1..3600000ms |
| `--durability batch\|per-process` | `batch` | Take up to8 ready files or one per finalizer receive; both fsync every file |
| `--writeback none\|hint` | `none` | Optional non-durable writeback experiment |
| `--census-rounds 0..8` |2 | Additional bounded process census rounds |
| `--trace-level stages\|basic` | `stages` | Detailed syscall/phase timings or reduced instrumentation |
| `--request-id ID` | Generated local ID | Caller correlation label, not an authenticity claim |

Options and values are separate arguments. `--pod-uid` remains an alias for
`--pod`. All mapped ranges use the same fixed proc-mem policy. `--scope` and
`--reader` are not accepted. See `--help` for syntax.

Unlike the ordinary binary's automatic `credits-v3` path, the development build
has no shared budget unless `--shared-budget FILE` is explicit. Always supply a
common private path for multi-process comparisons, plus `--buffer-policy latency`
to match public full-pool admission. Different paths do not share bounds. These
switches are experiment controls and are rejected by the ordinary public build.

