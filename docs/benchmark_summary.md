# Measurement notes

The README's Nginx table uses `wall_s` from [PodDump v7 results](benchmark_data/poddump_v7_results.json) and [baseline results](benchmark_data/baseline_results.json). It does not mix PodDump's smaller `cmd_span_s` with baseline wall time. Each is one observation from the 2026-10-16 runs. ProcDump uses the gcore backend; GDB uses `--readnever`. These old binaries are not the current 0.3.1 build.

The saved baseline Nginx core sizes are smaller than v7's. Consequently the table is an observed runtime comparison under those collection policies, not an assertion of equal byte coverage. The raw files retain all nine conditions, including slower PodDump Redis runs. Historical service-error counts and freeze results remain there; no zero-downtime guarantee is inferred.

## 2026-10-17 regression smoke test

| Tool | Before wall | After wall | CPU before → after | Max RSS before → after |
|---|---:|---:|---:|---:|
| poddump | 0.320 s | 0.267 s | 0.28 → 0.23 s | 92.32 → 90.23 MiB |
| kerneldump | 2.690 s | 2.532 s | 3.39 → 3.14 s | 11.25 → 11.25 MiB |

One old/new capture per tool on Ubuntu 24.04.5/Linux 6.8.0-147, using the existing Kubernetes VM and ext4 storage. The old executable ran first; there was no cache reset or repeat. Wall time is measured around process launch through exit (including durable publication); CPU/max RSS come from GNU time and exclude evidence export. This check found no gross regression; it does not establish a speedup or statistical equivalence.

PodDump used the same 64-MiB patterned, four-thread fixture process plus sandbox, with unchanged PID/start-time identities. Both produced 101,797,888 logical core bytes; all 128 MiB of fixture pattern across the two cores matched. KernelDump's live PFN payload changed from 739,835,904 to 715,538,432 bytes, so its timing is not a fixed-byte comparison. Both images verified. No performance optimization was made.

[Exact smoke observations and binary hashes](performance_smoke.json). Earlier comparison sources and SHA-256 values: [benchmark_sources.json](benchmark_sources.json).
