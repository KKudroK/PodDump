# PodDump

**Version 0.5.0** reduces process stop times and production disk impact while preserving full logical SHA-256 integrity and forensic coverage. See the consolidated [code, feature, and evidence guide](docs/CODEBASE_GUIDE.md) (Korean) for implementation details. Earlier v0.3.1 output, distro tables, and 2026-10-16 Nginx measurements below remain historical build records.

[한국어](README.ko.md) · [Codebase guide (KO)](docs/CODEBASE_GUIDE.md) · [Deployment](docs/deployment.md) · [Benchmarks](docs/benchmark_summary.md)

Capture a Kubernetes Pod's process memory from its node using one Pod UID. `poddump` discovers container and sandbox processes, writes ELF core files, and records process identities, missing/excluded ranges, SHA-256 digests, and collection status in `manifest.json`.

## What is new in v0.5.0

- **Two-stage zero-page skip & sparse write**: After verifying safe anonymous eligibility via `smaps` `VmFlags` (excluding `io`, `pf`, `ht`, `mm`, `sh`, `ui`, `uw`, `up`), Stage 1 uses `/proc/<pid>/pagemap` to skip reading unpopulated zero pages while Stage 2 uses AVX2 SIMD + `lseek` sparse holes for zero chunks that were read.
- **Forensic coverage preservation**: Retains `MADV_DONTDUMP` (`dd`) regions inside `PT_LOAD` and excludes only uncommitted `PROT_NONE` guard regions (`rss == 0 && swap == 0`), accounting excluded bytes explicitly in `manifest.json`.
- **Dynamic ELF header sizing & inline digest**: Dynamically sizes `PHDR` reservation (`MIN_PHDR_SLACK = 64` to `MAX_PHDR_SLACK = 1024`) to shrink small-container cores, and computes full-core `core_sha256` inline during buffered writes (`inline_write`) or via synced FD fallback (`post_sync_fd_read`).
- **Production node safeguards**: Adds `--max-write-mbps 1..10000` bandwidth pacing, `POSIX_FADV_DONTNEED` page-cache eviction, `posix_fallocate` manifest space reservation, `RLIMIT_NOFILE`-aware pending FD budgeting, and boot-ID / 3600s stale OFD lock cleanup (`gc_stale_locks`).

## Run

The release is one static Linux x86-64 executable. No GDB, glibc installation, container runtime CLI or adjacent files are required.

```sh
chmod 755 poddump
sudo ./poddump --pod FULL_POD_UID --output /evidence/new_collection
# Optional write bandwidth cap for busy production nodes
sudo ./poddump --pod FULL_POD_UID --output /evidence/new_collection --max-write-mbps 200
```

Production options: `--pod FULL_POD_UID` (or `--pod-uid`), `--output NEW_DIR`, `--request-id ID`, `--wait-timeout-ms 0..3600000` (default `60000`), `--max-write-mbps 1..10000`, `--format tree|json`, `--full-ids`. Use an existing output parent and a new collection directory. Run on the target node with its host proc/cgroup view and ptrace access. Output storage must support private permissions (`0o700`) and durable publication. [Supported environments and limits](docs/deployment.md).

Historical output from a two-process test Pod on Ubuntu 24.04.5, using v0.3.1 (output-path lines omitted):

```text
PodDump 0.3.1
Node: kkudrok-node  |  Scope: all_mapped_proc_mem  |  Durability: batch

Pod: 1f4123ca-cbac-4c40-8085-77efc8ca5629
├── Container: 2f7cf8919d5b…
│   └── PID 77316   [OK]      77316_1235239.core
└── Container: 55cf54e6f0ea…
    └── PID 77281   [OK]      77281_1235229.core

Result: COMPLETE  |  Processes: 2/2  |  Containers: 2  |  Elapsed: 0.235 s
```

[Full captured stdout (historical v0.3.1)](docs/examples/capture.stdout).

## Tested environments (historical release matrix)

The table below records tests of an earlier release binary; v0.5.0 A/B performance, `--no-pagemap-skip` SHA-256 equivalence, and `rrudrok` compatibility were verified on WSL2 `6.18.33.2` alongside offline unit tests (`81 passed`). Outside the Ubuntu Kubernetes row, Pod tests used synthetic cgroups. Debian, Rocky and Kali passed containerd/CRI-O/Docker path formats, which does not certify full installations of all three runtimes. [Test conditions](docs/distro_validation.md).

| Environment | Kernel | Result |
|---|---|---|
| Ubuntu 24.04.5, Kubernetes/containerd | 6.8.0-147 | Real Pod capture on ext4 |
| Ubuntu 26.04, WSL2 | 6.18.33.2 | Synthetic Pod capture on ext4 |
| Debian 12 | 6.1.0-53-cloud-amd64 | Capture + pattern check; ext4, cgroup v2 |
| Debian 13 | 6.12.111+deb13-cloud-amd64 | Capture + pattern check; ext4, cgroup v2 |
| Rocky Linux 8.10 | 4.18.0-553.el8_10.x86_64 | Capture + pattern check; xfs, cgroup v1 |
| Rocky Linux 9.8 | 5.14.0-687.10.1.el9_8.0.1.x86_64 | Capture + pattern check; xfs, cgroup v2 |
| Kali Rolling (cloud 2026.2) | 6.18.12+kali-cloud-amd64 | Capture + pattern check; ext4, cgroup v2 |
| Alpine 3.16.9, cgroup v1 | 5.15.148 | Synthetic Pod capture passed |
| Alpine 3.22.6, cgroup v2 | 6.12.110 | Synthetic Pod capture passed |

The Kali cloud archive is labelled 2026.2; its internal `VERSION_ID` is 2026.1.

## Measured results (historical 2026-10-16 comparison)

Historical Nginx comparison, 2026-10-16; one observation per condition. PodDump v7, ProcDump 3.5.3 with its gcore backend, and GDB with `--readnever`. Times below use wall time from the saved result files.

| Pods | Processes | PodDump | ProcDump | GDB |
|---:|---:|---:|---:|---:|
| 1 | 6 | **0.228 s** | 0.520 s | 0.550 s |
| 2 | 12 | **0.387 s** | 0.913 s | 1.022 s |
| 4 | 24 | **0.691 s** | 1.743 s | 1.944 s |

PodDump completed these multi-process Nginx captures in less time, though Redis cases were slower and collection policies/output sizes differ. [Results and source records](docs/benchmark_summary.md).

## Requests and results

- Different requests for the same Pod wait before discovery. `--wait-timeout-ms` defaults to 60000; zero rejects a busy Pod.
- The same active `--request-id` returns the existing job's location with exit **3**, without starting another collection. After completion, another invocation is a new capture.
- Exit **0**: complete under the recorded policy and persisted. **2**: partial. **1**: input/setup/persistence error. Preserve stderr and incomplete files.
- Targets are stopped per process; a Pod capture is not atomic. SIGKILL can leave fragments without a final manifest.

All cooperating collectors must share the same runtime directory and effective UID. Stop older versions before upgrading. [Coordination and recovery](docs/deployment.md#duplicate-requests-and-recapture).

## Build and verify

```sh
./poddump --version
./poddump --build-info
cargo test --offline --locked
python3 scripts/build_release.py --check-vendor
python3 scripts/build_release.py
```

Version **0.5.0**. The build script tests and emits `release/poddump` and a hash receipt. The receipt is optional at runtime. [Codebase guide (KO)](docs/CODEBASE_GUIDE.md) · [Development options](docs/development_options.md) · [v0.4.0 notes](docs/release_v0_4_0.md) · [Release checks](docs/release_validation.md) · [Earlier documentation](docs/history_readme_20261017_en.md).

MIT license. Core-file code derives from Microsoft ProcDump for Linux; see [NOTICE.txt](NOTICE.txt).
