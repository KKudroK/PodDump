# PodDump

PodDump collects the memory of every process belonging to a Kubernetes Pod on a
Linux node. Give it a **full Pod UID** and it discovers the Pod's containers,
child processes, and sandbox process through cgroup v2. It writes an ELF core
file for each collected process and a JSON manifest describing the result.

PodDump is a native Rust tool derived from the `corex` implementation in
[Microsoft ProcDump for Linux 3.5.3](https://github.com/microsoft/ProcDump-Linux/tree/8e34cbfaf57b38ca6b73160031e360edcbe6b9c1).
It performs discovery, scheduling, memory capture, and ELF writing itself; it
does not launch a ProcDump subprocess for each PID.

## Build

The validated platform is **Linux x86-64, little-endian**, with cgroup v2 and
containerd. Rust/Cargo 1.99.0 was used for the validated build. Dependencies and
their licenses are vendored; the build can run without network access once the
Rust toolchain is installed.

```sh
cargo build --release --locked --offline
cargo test --locked --offline -- --test-threads=1
./target/release/poddump --help
```

The resulting executable is `target/release/poddump`. Python, GDB, and the stock
ProcDump executable are not runtime dependencies. The unit suite alone does not
exercise the complete live Kubernetes acquisition and fault-recovery pipeline.

## Collect a Pod

Find the Pod UID and the node that hosts it:

```sh
kubectl -n my-namespace get pod my-pod \
  -o jsonpath='{.metadata.uid}{"\t"}{.spec.nodeName}{"\n"}'
```

Run PodDump as root **on that node**, using the host's `/proc` and cgroup mounts:

```sh
sudo ./target/release/poddump \
  --pod 12345678-1234-1234-1234-123456789abc \
  --output ./case_001
```

The output directory must not already exist. A Pod name, container ID, or sandbox
ID is not a substitute for the full Pod UID. PodDump does not query the Kubernetes
API or locate remote nodes. Its scope is the selected Pod on the local node.
Collection begins on request; CPU/memory threshold trigger polling is not included.

For a payload policy comparable to stock ProcDump 3.5.3's `-mc 32`:

```sh
sudo ./target/release/poddump \
  --pod 12345678-1234-1234-1234-123456789abc \
  --output ./case_002 --scope legacy32 --jobs 4
```

## Terminal output

The default output is a Unicode tree: Pod → container ID → host PID.
Each collected process shows its result and core filename. The footer reports
complete/total processes, container groups, elapsed time and the manifest path.
Container IDs are abbreviated to unique prefixes by default; full identities
remain in the manifest. Sandbox processes are included.

- `--full-ids` prints complete container IDs.
- `--format json` preserves the original one-line JSON result for scripts.
- `--pod-uid` remains a compatibility alias for `--pod`; specify only one.
- `--pod` accepts the full local Pod UID, as shown below.

This is an actual run on a temporary Pod with three app containers, each with
two processes, plus one sandbox process. It demonstrates the CLI; its elapsed
time is not part of the controlled performance comparison.

```text
$ sudo poddump --pod 278e2ad6-fa13-4b1e-8e94-24899d0e6622 --output ./case_001

PodDump
Node: localhost.localdomain  |  Scope: readable  |  Durability: batch

Pod: 278e2ad6-fa13-4b1e-8e94-24899d0e6622
├── Container: 4f23b29401d1…
│   └── PID 5152    [OK]      5152_33261.core
├── Container: 57e30ac92d78…
│   ├── PID 5177    [OK]      5177_33269.core
│   └── PID 5191    [OK]      5191_33273.core
├── Container: 6205e0cade6c…
│   ├── PID 5205    [OK]      5205_33275.core
│   └── PID 5217    [OK]      5217_33277.core
└── Container: e7421c7df2bb…
    ├── PID 5231    [OK]      5231_33280.core
    └── PID 5242    [OK]      5242_33283.core

Result: COMPLETE  |  Processes: 7/7  |  Containers: 4  |  Elapsed: 0.049 s
Output: ./case_001
Manifest: ./case_001/manifest.json
Container IDs abbreviated; full IDs are in the manifest (--full-ids to display).
PIDs are host PIDs; sandbox processes are included.
```

[Copyable command and output](docs/cli_example.txt).

## How collection works

1. Locate the exact Pod UID cgroup and enumerate all descendant thread groups.
2. Validate process identity using the boot ID, cgroup/container attribution,
   PID, start time, and pidfd where used by the capture path.
3. Seize and interrupt the process's observed threads, repeat the thread census,
   and capture registers and selected memory mappings.
4. Resume the target and write its core. Start data writeback while workers
   continue with other processes in the Pod. A bounded in-memory snapshot allows
   resuming before writing; oversized captures stream while stopped.
5. Synchronize every original output file description in bounded windows,
   publish complete cores, repeat the bounded process census, and persist the
   manifest and output/parent directories before successful return.

The default snapshot memory budget is 512 MiB **per PodDump invocation**. Running
several instances multiplies that budget and the worker count unless an external
scheduler divides them. See [architecture and limits](docs/architecture.md).

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--pod UUID` | Required | Exact full Pod UID on the local node |
| `--output NEW_DIR` | Required | New output directory |
| `--format tree\|json` | `tree` | Container/process tree or the original one-line JSON result |
| `--full-ids` | Off | Show full container IDs in the tree |
| `--jobs 1..16` | `4` | Maximum concurrent process workers |
| `--scope readable\|legacy32` | `readable` | Mapping selection policy |
| `--reader proc_mem\|readv` | `proc_mem` | Memory read implementation |
| `--memory-mib N` | `512` | Aggregate snapshot admission budget; `0` forces streaming |
| `--buffer-mib N` | `512` | Additional per-worker buffer ceiling |
| `--stream` | Off | Read and write while the target remains stopped |
| `--durability batch\|per-process` | `batch` | Overlap Pod capture with writeback, or synchronize within each capture worker |
| `--census-rounds 0..8` | `2` | Additional process census rounds |
| `--timeout-ms 1..3600000` | `60000` | Cooperative per-process capture/write deadline; final persistence may finish later |

The effective per-worker snapshot limit also accounts for the aggregate budget
divided by the worker count. `readable` selects readable VMAs and excludes kernel
special mappings such as `vvar` and `vsyscall`. Write-only and `PROT_NONE` mappings
are outside this policy. `legacy32` reproduces the relevant stock mapping policy;
neither mode should be described as a universal superset of the other.

## Outputs and exit status

The output directory uses mode `0700`; core and manifest files use mode `0600`.
`manifest.json` records the Pod UID, node/boot identity, process and thread
identities, memory ranges, timings, census results, and errors. Individual
processes may have different capture times.

| Exit code | Meaning |
| --- | --- |
| `0` | All targets completed, final census had no uncaptured identities/errors, and file/manifest/directory persistence succeeded |
| `2` | Partial acquisition; inspect process results and census errors |
| `1` | Setup or manifest-persistence failure |

Manifest `status` describes capture completeness. A successful process exit is
also required to confirm the final persistence barrier. Preserve stderr as well
as the output directory. If the normal manifest cannot be persisted, an emergency
JSON manifest is emitted to stderr. Partial core
files are unvalidated artifacts, not complete attributed evidence.

## Correctness boundaries

- Collection stops each process independently. It is **not a Pod-wide atomic
  snapshot**. Other processes can still modify shared mappings.
- Exiting or replaced processes, unreadable memory, failed identity checks, and
  late arrivals beyond the bounded census are reported rather than replaced
  with successful zero-filled data.
- SIGINT/SIGTERM request controlled cleanup. SIGKILL or power loss can leave
  partial files. A userspace deadline cannot guarantee a bound on a kernel call
  blocked in uninterruptible I/O.
- The validated ELF format includes GP, FP, and x86 XSAVE register state. Some
  inherited signal/accounting and process-state fields are placeholders;
  `NT_SIGINFO` is omitted. Extended ELF program-header numbering is unsupported:
  more than 65,533 selected VMAs produces an explicit error.
- Valid core structure does not guarantee symbols or perfect stack unwinding
  for every language runtime. See the GDB note in
  [architecture and limits](docs/architecture.md).

## Performance and validation

The 48 complete comparison runs tested six workload shapes, two repeats, tuned
ProcDump PID workers, earlier baseline PodDump, PodDump, and a
no-collection control. Both collectors captured the same Pod process sets
with the matched `legacy32` mapping policy and a global 4-target limit.
Timing includes final file and directory synchronization. One additional
attempt had an incomplete load-generator record and is retained separately;
a control-file publication fix preceded its replacement, with the collector
binary and measurement settings unchanged (49 attempts in total).

| Workload | ProcDump PID (s) | Baseline PodDump (s) | PodDump (s) | Paired speedup |
| --- | ---: | ---: | ---: | ---: |
| 1 Pod × 32 containers / 8 MiB resident bulk per app process | 1.297 | 0.687 | 0.508 | 2.61× (2.27–2.95) |
| 8 Pods × 16 containers / 8 MiB resident bulk per app process | 4.139 | 4.589 | 3.637 | 1.13× (1.06–1.20) |
| 32 Pods × 1 container / 8 MiB resident bulk per app process | 1.730 | 0.713 | 0.720 | 2.42× (2.20–2.65) |
| 64 Pods × 4 containers / 8 MiB resident bulk per app process | 6.774 | 7.199 | 8.436 | 0.82× (0.73–0.91) |
| 8 Pods × 4 containers / 64 MiB resident bulk per app process | 6.365 | 9.141 | 3.066 | 2.12× (1.33–2.91) |
| 8 Pods × 8 containers / nginx+Redis | 5.437 | 3.293 | 6.843 | 0.79× (0.54–1.04) |

Times are two-repeat medians; speedup is the median of same-block
ProcDump/PodDump ratios. The range contains both observed ratios.
This is a single 8-vCPU/12-GiB VMware node, not a claim about every cluster.
MiB labels describe the seeded resident bulk; other fixture mappings,
stacks, libraries, child processes and sandbox processes are also included.
One Pod with 32 containers was 2.27–2.95× faster than ProcDump and
1.26–1.49× faster than the earlier baseline. At 64 Pods, PodDump lost to ProcDump in both
repeats; nginx/Redis also regressed against the baseline in both repeats.
Synthetic output sizes matched exactly. Dynamic service mappings differed
slightly; the second optimized service run was 0.85% larger than the earlier baseline.
See [benchmark methods, per-repeat results and costs](docs/benchmarks.md)
for workload details, PSI, CPU, context switches, request latency and limits.
The collector candidate also passed 20 offline unit tests and eight controlled
persistence/cancellation interventions. 

## Repository layout

```text
src/                     Native collector and ProcDump-derived core writer
vendor/                  Locked dependency sources and their license files
.cargo/config.toml       Offline vendor configuration
Cargo.toml, Cargo.lock   Package and dependency lock
build_provenance.json    Provenance embedded in the current CLI build
docs/                    Design, support boundaries, and benchmark documentation
licenses/procdump_mit.txt Original upstream MIT license
NOTICE.txt               Upstream attribution and modifications
```

The embedded provenance describes the current CLI build. The measured benchmark
record is preserved in benchmarks/build_provenance_v0_2_0.json. Repository
documentation and packaging updates do not replace that measured build record.

## License

PodDump is distributed under the [MIT license](LICENSE). The ProcDump-derived
code retains the [Microsoft MIT notice](licenses/procdump_mit.txt) and
[upstream attribution](NOTICE.txt). Dependencies retain their individual license
files under `vendor/`. PodDump is not an official Microsoft product.
