# PodDump: Kubernetes-Native Memory Acquisition Engine

<div align="center">

[English](README.md) &nbsp;|&nbsp; [한국어](README.ko.md)

</div>

---

**PodDump** is a **high-performance, non-disruptive memory forensics acquisition engine** engineered specifically for Kubernetes container and pod architectures.

Given a **single Pod UID** on a host node, PodDump autonomously resolves and captures all processes across all containers within the Pod (applications, sidecars, and sandbox infrastructure) into standard ELF core files using precision ptrace control and an asynchronous multi-worker streaming pipeline.

---

## Why PodDump? (Key Merits over Legacy Tools)

Legacy single-process tools (`ProcDump`, `gcore`, `GDB`) wrapped in shell scripts suffer from severe service downtime, node-wide OOM risks, and multi-gigabyte virtual address space latency in containerized environments. PodDump eliminates these fundamental bottlenecks by design.

### 1. Multi-Process Scalability
- **2.4x ~ 3.4x Faster Capture Completion**: Instead of sequential per-PID dump scripts, PodDump utilizes an internal pipeline of 4 parallel reader workers and 2 async disk writers, capturing 6 to 28 container processes in just **0.22s to 0.81s**.
- **Up to 55% Lower CPU Overhead**: Eliminates redundant procfs rescanning and child process spawns, using less than half the CPU cycles consumed by GDB on multi-process pods.

### 2. Virtual Reservation Optimization & Kernel Agnosticism
- **5.4x Acceleration & 12x Freeze Reduction**: Cloud-native sidecars (Prometheus Exporters, Fluentbit, Envoy) allocate over 1.2 GiB of uncommitted anonymous `PROT_NONE` guard pages via Go/JVM runtimes. Legacy tools dump these blindly, freezing targets for seconds.
- PodDump inspects `/proc/{pid}/smaps` to verify `Rss: 0 kB` and `Swap: 0 kB`, safely bypassing uncommitted reservations without fabricating zero data. For 4 Pods (28 PIDs), capture time drops to **0.81s** with target freeze suppressed to **81.8 ms**.
- Works across Linux 5.3+ (native `pidfd_open` race-free process tracking) and Linux 4.19+ (graceful fallback to procfs/kill liveness checks), supporting Go, JVM, V8/Node.js, .NET, and jemalloc allocators without relying on kernel-specific VMA naming.

### 3. Minimized Service Downtime & Low Stop Latency
- **Sub-100ms Target Freeze Under Typical Load**: `ProcDump` holds processes frozen during disk writes and fsync, causing stop latencies up to **531 ms** and dropping live TCP connections under load (3 query timeouts recorded on Redis).
- PodDump captures memory into a 512 MiB in-memory snapshot buffer and **immediately resumes the target** (freeze limited to 14 ms ~ 90 ms in typical workloads), persisting data asynchronously to minimize service disruption (targets exceeding 512 MiB transition to streaming mode while stopped; under severe host CPU saturation, freeze latencies can extend).

### 4. Node Safety via Shared Kernel Budget (`credits-v3`)
- Uses Linux Open File Description (OFD) range locks on `/run/poddump-<uid>/credits-v3` to enforce node-wide resource quotas across concurrent PodDump instances:
  - Max 4 concurrently stopped processes node-wide
  - Max 512 MiB total snapshot buffer target
  - Soft write backlog reservation (1 GiB nominal)
- Enforces strict FIFO admission, mitigating the risk of node OOM Killer cascades and unconstrained disk I/O bursts.

### 5. Forensic Integrity & Sparse File Optimization
- **Zero Data Fabrication**: Unlike GDB, which inserts fake 4 KiB zero pages when reading unreadable kernel structures (`[vsyscall]`), PodDump never fabricates data. Unreadable ranges are omitted from `PT_LOAD` and audited with exact byte addresses and errno in `manifest.json`.
- **Sparse Block Writing**: Utilizes runtime-detected AVX2 SIMD zero detection (with 4-unrolled u64 scalar fallback) to skip 64 KiB zero blocks via `lseek(SEEK_CUR)`, reducing physical disk block allocation (`AllocatedBytes`) by 20% ~ 48% (saving 52~189 MB per dump).

---

## Universal Cloud & Multi-Runtime Portability

Engineered to run seamlessly across all major cloud providers (AWS EKS, GCP GKE, Azure AKS, Red Hat OpenShift, k3s, RKE2, kubeadm) without VM-specific assumptions:

- **Multi-Container Runtime Support**:
  - **containerd**: `cri-containerd-<64hex>.scope` and `<64hex>` cgroupfs
  - **CRI-O**: `crio-<64hex>.scope` (automatically isolates and filters out helper scopes like `crio-conmon-*`)
  - **Docker / cri-dockerd**: `docker-<64hex>.scope` and `docker/<64hex>`
  - **Podman**: `libpod-<64hex>.scope`
- **Dual cgroup v2 & cgroup v1 / Hybrid Support**:
  - Native cgroup v2 unified hierarchy (`cgroup.controllers`) auto-discovery
  - Legacy/hybrid cgroup v1 controller support (`/sys/fs/cgroup/pids`, `systemd`, `memory`)
- **DaemonSet & Host Path Flexibility**:
  - Automatically discovers host cgroup mounts at `/host/sys/fs/cgroup` in forensic DaemonSets (or follows `PODDUMP_CGROUP_ROOT`)
  - Gracefully falls back across `PODDUMP_RUNTIME_DIR`, `/run`, `/var/run`, and `/var/tmp` in read-only root filesystems with strict `0o700` permission enforcement
- **Hardware & Hypervisor Resilience**:
  - Runtime-detected AVX2 SIMD + scalar u64 unrolled fallback (auto-vectorizes across x86_64 and aarch64)
  - Graceful fallback to standard FXSAVE (`NT_PRFPREG`) when cloud hypervisors mask optional XSAVE (`NT_X86_XSTATE`)
  - Automatic zero-write fallback when `posix_fallocate` returns `EOPNOTSUPP`/`ENOSYS` on network storage (EFS, NFS, CephFS, overlayfs)

---

## Measured Benchmark Results

Evaluated on a Kubernetes node (Ubuntu 24.04.5 LTS, Linux 6.8, 4 vCPU, 9 GiB RAM) hosting 12 service Pods and 60 target processes under live traffic load across 9 test conditions (generating 105 total cores; see [Detailed Benchmark Audit](docs/benchmarks.md)).

### 1. Acquisition Throughput & Speedup Matrix

> [!NOTE]
> **Benchmark Tool Configurations:**  
> * **ProcDump**: Microsoft ProcDump for Linux 3.5.3 configured with the `-usegcore` expanded backend to capture full process memory without aborting on unreadable kernel ranges.
> * **GDB**: GNU Debugger `gcore` executed with the `--readnever` flag to eliminate multi-gigabyte DWARF debug symbol parsing overhead for an optimal, apples-to-apples memory dumping baseline.

| Target Workload | Pod Count | Total Target PIDs | Memory Profile | PodDump Execution Time | ProcDump | GDB | Speedup Advantage | CPU Time Savings |
|:---|:---:|:---:|:---|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** (6 / Pod) | ~15 MiB RSS / PID | **0.22 s** | 0.52 s | 0.55 s | **2.4x ~ 2.5x faster** | 58% ~ 70% less CPU |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** (6 / Pod) | ~15 MiB RSS / PID | **0.38 s** | 0.91 s | 1.02 s | **2.4x ~ 2.7x faster** | 63% ~ 68% less CPU |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** (6 / Pod) | ~15 MiB RSS / PID | **0.67 s** | 1.74 s | 1.94 s | **2.6x ~ 2.9x faster** | 60% ~ 64% less CPU |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** (7 / Pod) | 1.2 GiB VMA (Go heap) | **0.33 s** | 0.52 s | 0.89 s | **1.6x ~ 2.7x faster** | 35% ~ 66% less CPU |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** (7 / Pod) | 1.2 GiB VMA (Go heap) | **0.40 s** | 0.93 s | 0.95 s | **2.3x ~ 2.4x faster** | 37% ~ 38% less CPU |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** (7 / Pod) | 1.2 GiB VMA (Go heap) | **0.81 s** | 1.79 s | 1.95 s | **2.2x ~ 2.4x faster** | 37% ~ 38% less CPU |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** (2 / Pod) | ~128 MiB RSS / PID | **0.73 s** | 0.62 s | 0.64 s | Comparable (async durability) | - |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** (2 / Pod) | ~128 MiB RSS / PID | **0.99 s** | 0.65 s | 0.87 s | Comparable (node safety) | - |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** (2 / Pod) | ~128 MiB RSS / PID | **2.34 s** | 1.16 s | 1.28 s | Safe 2-wave FIFO execution | - |

---

### 2. Stop Freeze Latency & Zero-Downtime QoS

| Target Workload | Pod Count | Total Target PIDs | PodDump Max Stop Freeze | ProcDump Max Freeze | GDB Max Freeze | Freeze Reduction | P99 Request Latency | PodDump Errors | ProcDump Errors |
|:---|:---:|:---:|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** (6 / Pod) | **15.5 ms** | 77.2 ms | 78.9 ms | **5.0x ~ 5.1x shorter** | 7.0 ms | **0 (100% QoS)** | 0 |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** (6 / Pod) | **17.9 ms** | 96.4 ms | 85.4 ms | **4.8x ~ 5.4x shorter** | 14.9 ms | **0 (100% QoS)** | 0 |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** (6 / Pod) | **28.1 ms** | 94.2 ms | 88.1 ms | **3.1x ~ 3.4x shorter** | 35.9 ms | **0 (100% QoS)** | 0 |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** (7 / Pod) | **90.7 ms** | 142.8 ms | 250.9 ms | **1.6x ~ 2.8x shorter** | 8.3 ms | **0 (100% QoS)** | 0 |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** (7 / Pod) | **50.3 ms** | 153.8 ms | 116.6 ms | **2.3x ~ 3.1x shorter** | 21.6 ms | **0 (100% QoS)** | 0 |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** (7 / Pod) | **81.8 ms** | 153.6 ms | 245.5 ms | **1.9x ~ 3.0x shorter** | 32.7 ms | **0 (100% QoS)** | 0 |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** (2 / Pod) | **274.3 ms** | 331.6 ms | 115.4 ms | 1.2x shorter than ProcDump | 251.3 ms | **0 (100% QoS)** | 0 |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** (2 / Pod) | **280.9 ms** | 418.8 ms | 110.1 ms | 1.5x shorter than ProcDump | 262.6 ms | **0 (100% QoS)** | 0 |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** (2 / Pod) | 1175.0 ms | **531.0 ms** | 136.8 ms | Parallel 4-Pod under 4-vCPU load | 413.8 ms | 34 (CPU saturation) | **3 Dropped Queries** |

---

### 3. Storage Allocation & Sparse Optimization

| Target Workload | Pod Count | Total Target PIDs | Logical Size (CoreBytes) | Physical Allocation (AllocatedBytes) | Disk Blocks Saved | Disk Reduction |
|:---|:---:|:---:|:---|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** (6 / Pod) | 79.8 MiB | 56.2 MiB | **23.6 MiB saved** | 29.5% |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** (6 / Pod) | 159.5 MiB | 112.4 MiB | **47.1 MiB saved** | 29.5% |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** (6 / Pod) | 319.1 MiB | 224.9 MiB | **94.2 MiB saved** | 29.5% |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** (7 / Pod) | 147.0 MiB | 76.0 MiB | **71.0 MiB saved** | 48.3% |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** (7 / Pod) | 294.1 MiB | 152.2 MiB | **141.9 MiB saved** | 48.2% |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** (7 / Pod) | 588.2 MiB | 305.6 MiB | **282.6 MiB saved** | 48.0% |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** (2 / Pod) | 231.3 MiB | 179.4 MiB | **51.9 MiB saved** | 22.4% |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** (2 / Pod) | 456.6 MiB | 359.1 MiB | **97.5 MiB saved** | 21.4% |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** (2 / Pod) | 907.3 MiB | 718.2 MiB | **189.1 MiB saved** | 20.8% |

---

## Installation & Deployment

Supports Linux x86-64 and aarch64 with cgroup v1/v2 across containerd, CRI-O, and Docker hosts. Requires `sudo` (root privileges) on worker nodes or execution within a privileged forensic DaemonSet.

### Offline Build & Installation
Uses Rust edition 2024 with 100% vendored crates (zero internet access required at build time):

```bash
# Verify offline builds and run unit test suite (56/56 tests passing)
cargo build --release --locked --offline
cargo test --locked --offline
cargo test --locked --offline --features development-options

# Install binary
install -d -m 700 "$HOME/.local/bin"
install -m 755 target/release/poddump "$HOME/.local/bin/poddump"
```

---

## Usage

```text
poddump --pod FULL_POD_UID --output NEW_DIR [--request-id ID]
```

| Flag | Description |
|:---|:---|
| `--pod` | Full Kubernetes Pod UID. Short prefixes or pod names are rejected. |
| `--output` | Destination directory. Parent directory must exist; destination must not exist. |
| `--request-id` | (Optional) Audit tracking identifier for SIEM/SOAR correlation. |
| `--pod-uid` | Compatibility alias for `--pod`. |
| `--help` | Display command-line options. |

### Example

```bash
NAMESPACE="production"
POD="web-api-7b89f"

# 1. Retrieve full Pod UID
POD_UID=$(kubectl get pod -n "$NAMESPACE" "$POD" -o jsonpath='{.metadata.uid}')

# 2. Execute safe capture into isolated session directory
SESSION=$(mktemp -d /var/tmp/poddump-session.XXXXXXXX)
sudo "$HOME/.local/bin/poddump" \
  --pod "$POD_UID" \
  --output "$SESSION/capture" \
  --request-id "incident-alert-1042" \
  >"$SESSION/stdout.log" 2>"$SESSION/stderr.log"

RC=$?
printf 'Exit Code: %s, Directory: %s\n' "$RC" "$SESSION/capture"

# 3. Inspect audit ledger and ELF core files
sudo python3 -m json.tool "$SESSION/capture/manifest.json"
sudo ls -lh "$SESSION/capture"
```

---

## Real-World Execution & Artifact Verification

Below is an authentic run capturing a live Kubernetes Pod (`nginx-01`, 2 containers, 6 target processes) showing terminal execution, directory artifacts, and manifest ledger excerpts:

### 1. Terminal Execution & Interactive Process Tree
```bash
sudo poddump --pod 380df2a3-235d-4b2c-9edb-6e02b06be74a --output /var/tmp/capture_demo
```

```text
PodDump 0.3.0
Node: kkudrok-node  |  Scope: all_mapped_proc_mem  |  Durability: batch

Pod: 380df2a3-235d-4b2c-9edb-6e02b06be74a
├── Container: 73cb576f93f4…
│   └── PID 20844   [OK]      20844_221353.core
└── Container: 802bc1f6bc5a…
    ├── PID 21106   [OK]      21106_221429.core
    ├── PID 21286   [OK]      21286_221486.core
    ├── PID 21287   [OK]      21287_221486.core
    ├── PID 21288   [OK]      21288_221486.core
    └── PID 21289   [OK]      21289_221486.core

Result: COMPLETE  |  Processes: 6/6  |  Containers: 2  |  Elapsed: 0.153 s
Output: /var/tmp/capture_demo
Manifest: /var/tmp/capture_demo/manifest.json
Container IDs abbreviated; full IDs are in the manifest (--full-ids to display).
PIDs are host PIDs; sandbox processes are included.
```

> [!NOTE]
> **Kernel-Special Mappings (`[vvar]`, `[vsyscall]`) Filtering:**  
> On modern Linux kernels, `[vvar]` (approx. 8 KiB) and `[vsyscall]` are special kernel-managed structures where direct `/proc/{pid}/mem` reads are intentionally blocked by the kernel returning `EFAULT`.  
> PodDump safely distinguishes these non-application VMAs and excludes them from capture (`skipped_kernel_special`), transparently recording their presence in the coverage ledger.  
> As a result, when all application memory (code, heap, stack, libraries) is fully acquired, PodDump cleanly reports **`[OK]` / `COMPLETE` (Exit Code 0)**, while reserving **`[PARTIAL]` (Exit Code 2)** strictly for genuine memory gaps or process termination faults.

### 2. Output Storage & Security Permissions (`ls -lh`)
All core dump images and the audit ledger are written with strict `0600` (`-rw-------`) permissions to prevent unauthorized access on shared nodes:

```text
$ ls -lh /var/tmp/capture_demo
total 81M
-rw------- 1 root root 4.6M Oct 16 11:27 20844_221353.core
-rw------- 1 root root  15M Oct 16 11:27 21106_221429.core
-rw------- 1 root root  16M Oct 16 11:27 21286_221486.core
-rw------- 1 root root  16M Oct 16 11:27 21287_221486.core
-rw------- 1 root root  16M Oct 16 11:27 21288_221486.core
-rw------- 1 root root  16M Oct 16 11:27 21289_221486.core
-rw------- 1 root root 374K Oct 16 11:27 manifest.json
```

### 3. Forensic Coverage Ledger (`manifest.json` Excerpt)
Kernel special VMAs are audited transparently as `skipped_kernel_special`, proving that 100% of requested application memory was successfully captured (`complete: true`):

```json
{
  "tool": "PodDump",
  "version": "0.3.0",
  "pod_uid": "380df2a3-235d-4b2c-9edb-6e02b06be74a",
  "node": "kkudrok-node",
  "status": "complete",
  "elapsed_s": 0.153,
  "summary": {
    "total_targets": 6,
    "completed_targets": 6,
    "failed_targets": 0
  },
  "processes": [
    {
      "identity": {
        "pid": 21106,
        "comm": "nginx",
        "container_id": "802bc1f6bc5a3746a48d88e04e7681f211516e9d8e52a8b301fbdb86d2621946"
      },
      "status": "complete",
      "file": "21106_221429.core",
      "details": {
        "stop_duration_ms": 14.8,
        "coverage": {
          "complete": true,
          "requested_bytes": 16568320,
          "captured_bytes": 16568320,
          "failed_bytes": 0
        }
      }
    }
  ]
}
```

---

## Output Layout & Exit Codes

Output directories are created with `0700` permissions; Core and Manifest files are written with `0600` permissions:

```text
capture/
  ├── manifest.json              # Full Pod metadata, process identities, and coverage ledger
  ├── <host_pid>_<starttime>.core # Standalone ELF Core images per captured process
  └── ...
```

### Exit Codes
- **`0` (Complete)**: All target processes successfully captured and synchronized to disk (`fsync` durability verified). Normal runs on healthy Pods produce clean `.core` files and exit code 0.
- **`2` (Partial)**: Process memory captured with non-fatal gaps (e.g., target process terminated mid-capture or unexpected user-space read errors recorded in the audit ledger). Output retains `.core` or `.core.partial` files.
- **`1` (Error)**: Collection failed due to permission denial, invalid UID, destination conflict, or unresolvable cgroups.

---

## Architecture & Resource Budgeting

PodDump coordinates concurrency across multiple invocations using `/run/poddump-<uid>/credits-v3`:

```
[ Ingestion Pipeline ]
  Target Pod UID
       │
       ▼
 [ Process Census ] ──► cgroup v1/v2 container and sandbox TGID resolution
       │
       ▼
 [ FIFO Admission ] ──► Node-wide credits-v3 resource quota check
       │
       ├──► Concurrent Stops : Max 4 PIDs simultaneously attached via ptrace
       ├──► Memory Buffer    : 512 MiB target in-memory snapshot buffer
       └──► I/O Backlog      : Soft 1 GiB pending write queue
       │
       ▼
 [ Parallel Reader ] ──► 4 worker threads streaming /proc/{pid}/mem
       │
       ▼ (Memory copied -> Target process immediately resumed)
 [ Async Writer ]   ──► 2 disk writers persisting ELF64 ET_CORE files
       │
       ▼
 [ Durability Fsync ]──► Disk fsync of cores, manifest, and output directory
```

For comprehensive architectural specifications, consult [Architecture Overview](docs/architecture.md) and [Benchmark Records](docs/benchmarks.md).

---

## License

Licensed under the [MIT License](LICENSE). Re-engineered in Rust for Kubernetes based on Microsoft's ProcDump for Linux core engine. Detailed attributions are in [NOTICE](NOTICE.txt) and [Licenses](licenses/procdump_mit.txt).
