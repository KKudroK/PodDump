# PodDump

The 2026-11-05 source review fixes collection omissions and identity checks. See [changes and validation limits](docs/capture_review.md). Earlier release captures and performance figures below describe previous builds.

[한국어](README.ko.md) · [Deployment](docs/deployment.md) · [Benchmarks](docs/benchmark_summary.md)

Capture a Kubernetes Pod's process memory from its node using one Pod UID. `poddump` discovers container and sandbox processes, writes ELF core files, and records process identities, missing ranges and collection status in `manifest.json`.


Captured Core files now carry `details.core_sha256`, computed from the held capture inode after synchronization and before publication. Hash timing is recorded separately. Synthetic PRPSINFO scheduler state is unknown; the timestamped census state remains in the manifest. Older captures without this hash remain distinguishable.

## Run

The release is one static Linux x86-64 executable. No GDB, glibc installation, container runtime CLI or adjacent files are required.

```sh
chmod 755 poddump
sudo ./poddump --pod FULL_POD_UID --output /evidence/new_collection
```

Use an existing output parent and a new collection directory. Run on the target node with its host proc/cgroup view and ptrace access. Output storage must support private permissions and durable publication. [Supported environments and limits](docs/deployment.md).

Actual output from a two-process test Pod on Ubuntu 24.04.5, using v0.3.1. Output-path lines are omitted here.

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

[Full captured stdout](docs/examples/capture.stdout).

## Tested environments

The same x86-64 release binary was used in every row.

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

Outside the Ubuntu Kubernetes case, Pod tests used synthetic cgroups. Debian, Rocky and Kali each passed containerd/CRI-O/Docker path formats; this does not certify actual installations of all three runtimes. [Test conditions](docs/distro_validation.md).

The Kali cloud archive is labelled 2026.2; its internal `VERSION_ID` is 2026.1.

## Measured results

Historical Nginx comparison, 2026-10-16; one observation per condition. PodDump v7, ProcDump 3.5.3 with its gcore backend, and GDB with `--readnever`. Times below use wall time from the saved result files.

| Pods | Processes | PodDump | ProcDump | GDB |
|---:|---:|---:|---:|---:|
| 1 | 6 | **0.228 s** | 0.520 s | 0.550 s |
| 2 | 12 | **0.387 s** | 0.913 s | 1.022 s |
| 4 | 24 | **0.691 s** | 1.743 s | 1.944 s |

PodDump completed these multi-process Nginx captures in less time. This does not apply to every workload: Redis cases were slower. Collection policies and output sizes differ; these are not byte-identical memory captures. [Results and source records](docs/benchmark_summary.md).

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
# Build host only: Rust 1.90.0 + x86_64-unknown-linux-musl target
python3 scripts/build_release.py
```

Version **0.4.0**. The build script tests and emits `release/poddump` and a hash receipt. The receipt is optional at runtime. [Changes and verified scope](docs/release_v0_4_0.md) · [Release checks](docs/release_validation.md) · [Earlier documentation](docs/history_readme_20261017_en.md).

MIT license. Core-file code derives from Microsoft ProcDump for Linux; see [NOTICE.txt](NOTICE.txt).
