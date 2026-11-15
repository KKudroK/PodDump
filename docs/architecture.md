# Architecture and support boundaries

## Discovery, Identity, and Cloud Multi-Runtime Compatibility

The collector resolves one full Pod UID across both cgroup v2 unified hierarchies and
cgroup v1 / hybrid controllers (`/sys/fs/cgroup/pids`, `/sys/fs/cgroup/systemd`, `/sys/fs/cgroup/memory`).
It seamlessly discovers container layouts from **containerd**, **CRI-O** (`crio-<64hex>.scope`, excluding
external container monitor scopes `crio-conmon-*`), **Docker / cri-dockerd** (`docker-<64hex>.scope`),
and **Podman** (`libpod-<64hex>.scope`), across both systemd slices and cgroupfs paths.
In forensic DaemonSet environments, it automatically inspects host mounts at `/host/sys/fs/cgroup`
or follows custom locations specified by `PODDUMP_CGROUP_ROOT`.

It recursively enumerates TGIDs, including child processes and the sandbox. Known paths are
tried before the recursive fallback. Container attribution is derived from the observed hierarchy;
ambiguous attribution is an explicit error. Names and numeric PIDs alone are insufficient identities.

The manifest ties caller request ID, node hostname and boot ID to Pod UID,
container/cgroup, PID/start time, and captured thread start times. Bracketed
UTC/monotonic readings support correlation with caller evidence. The initial census is checked
for stability. Additional census rounds collect new identities, while remaining
uncaptured identities are reported when the configured bound is reached. Each
census batch drains through persistence before the next census, including
processes born while its files were being written. This is a bounded observation,
not continuous monitoring or a guarantee of capturing short-lived processes.

## Single maximum-coverage policy

The public and development binaries attempt mapped VMAs through a pinned
`/proc/PID/mem` descriptor under a single maximum-coverage policy. PF_R,
coredump_filter, DONTDUMP, file backing and shared/private status are not
selection filters. Committed PROT_NONE and write-only mappings are attempted.
Permissions remain unchanged in ELF flags. To eliminate false-positive partial flags
caused by unreadable kernel pages or unallocated virtual reservations, the policy
safely excludes kernel-special regions (`[vvar]`, `[vsyscall]`) and uncommitted
anonymous `PROT_NONE` reservations verified via `/proc/{pid}/smaps` (`Rss: 0`, `Swap: 0`).
These excluded ranges are not requested from `/proc/PID/mem` but are tracked in the
audit ledger under `excluded_kernel_special_bytes` and `excluded_uncommitted_prot_none_bytes`.
There is no `--scope` or `--reader` selector. The manifest uses schema 2,
`collection_policy: all_mapped_proc_mem`, and `policy_filter: uncommitted_anonymous_prot_none_and_kernel_special_excluded`.

After a large read fails, the current page is retried and an actual failure or
zero-length read is recorded with its address range and errno. Reading continues
at the next page and later VMAs. Only successfully read runs get PT_LOAD entries;
failed bytes never become a zero-filled load segment. File padding is unreferenced
memory, while sparse holes within PT_LOAD represent bytes actually read as zero.
The coverage ledger distinguishes total mapped, requested, captured, failed,
excluded and unattempted bytes. GDB may fall back to an executable/file mapping
for absent bytes; that is not captured evidence. Use the ELF segment table and
ledger to distinguish them.

A bounded table for at most 65,533 successful runs is reserved before payload
capture (about 3.7 MB logical space). Ledger entries are separately bounded and
reserved before attachment. Estimate/admission includes all mapped bytes and the
ELF header reserve; metadata and kernel page cache are outside snapshot payload
credits. Mapping growth cannot silently exceed pre-stop reservations. Ledger or
ELF limits abort with the remaining bytes reported as unattempted.

Memory-read gaps yield a durable `.core.partial`, process/manifest partial status
and exit 2; they are not promoted to completed `.core` files. Capture cancellation,
identity errors, writer faults and durability errors remain failures. Their partial
files may have incomplete ELF structure. Bounded live validation used two small Pods in buffered and forced-stream paths
for four captures. All 163,840 known oracle bytes matched, including non-readable,
file/shared, DONTDUMP and true-zero data; actual EOF gaps had no PT_LOAD and later
data remained available. All eight cores passed readelf. Isolated GDB opened both
paths, extracted every known recoverable byte, and rejected the missing EOF
address. Every target resumed with TracerPid 0 and the Pods remained Ready.

The first GDB missing-address check exposed host-library fallback, not an ELF
captured-byte claim. Core-only validation disabled automatic solib loading and
used an empty sysroot. Ordinary analysis must still distinguish fallback bytes
from captured evidence. This is a small functional check, not performance,
cross-tool coverage, arbitrary workload or fault-universe qualification. Embedded
build provenance describes the build-time checkpoint; later functional evidence
is maintained separately.

## Stop, capture, and release

The worker uses `PTRACE_SEIZE` and `PTRACE_INTERRUPT`, waits for observed threads
to stop, and repeats the thread census. The default `/proc/PID/mem` descriptor
is opened after the observed threads stop, avoiding capture through an old
address-space descriptor when `exec` races with attachment.

The capture paths preserve an observed pre-existing group stop and reinject
observed pending signal numbers. This is not restoration of complete siginfo,
signal history or every concurrent signal race. Cleanup failures are explicit.
Normal return and panic unwinding run the RAII detach path; SIGKILL and abort do
not. The live recovery tests do not establish every tracer-death scenario.
The implementation has a shared bounded
attach interval and a per-process userspace deadline; an uninterruptible kernel
operation can still exceed a userspace deadline.

Buffered capture copies the selected contents into an anonymous mapping that
holds the final ELF layout. The target is detached before sparse writing and
file synchronization. Streaming captures read and write while stopped, then
detach and synchronize. Actual zero bytes may become sparse holes. Read failures
never become fabricated successful zero data.

Admission reserves memory, output backlog and descriptor credits before stopping
the target. The mmap is also created before attachment. Credits include queued
snapshots and remain held until writing consumes and destroys the snapshot.
The local scheduler selects the oldest fitting target, allowing at most 16
bypasses before draining. Mapping growth beyond the reservation fails and
detaches; it does not wait for more credits while stopped.

Production admission permits one target to use the full 512 MiB snapshot pool.
Targets above the ceiling stream with 1 MiB scratch while stopped. The cooperative
stop cap is 5 seconds; an in-flight kernel syscall can exceed it. Streaming
never resumes between chunks. Shared memory can still change through other tasks.

The automatic shared budget uses a private owner-only regular file and Linux
OFD range locks to share capture, snapshot, backlog and file credits across
invocations. Nonblocking flock makes reservations all-or-none; retry waits check
cancellation and the original request deadline. Process death releases locks.
The production build supplies identical limits and the same per-owner path in
`/run` without caller tuning. Cross-process
admission uses registered per-request FIFO. One independent OFD owns a ticket
and resource locks; flock serializes registration and all-or-none admission.
Only the oldest live registered request may reserve capture/snapshot/backlog/file
credits. Waiting reserves no subset of those resources and occurs before attach.
Cancelled or exited waiters release OFD slot locks; later callers reap stale
records. There are256 registration slots; registration itself is deadline-aware,
but unregistered callers have no FIFO guarantee. Diagnostics include the ticket,
queue-ahead maximum, blocked queue/resource probes and admission time.

The implementation uses `/run/poddump-<euid>/credits-v3`, declaring
capture4, unfinished4, snapshot512MiB, backlog1024MiB and one core-fsync slot.
Unfinished/file credits remain held through finalization. One independent OFD
is reused while admission waits, with sleeping2/4/8ms backoff plus limited jitter.
Head-of-line blocking can delay a small fitting request behind an older larger
one. FIFO applies after per-process registration, not equally across Pods or
before the local oldest-fitting scheduler. Deadline/cancellation may still fail
a request; no hard-deadline or eventual-success guarantee is made.

Different automatic path versions cannot enforce a combined budget. Upgrade one
cooperating cohort only after old collectors have stopped; do not run v2/v3 as if
they shared limits. Different effective owners, paths and incompatible settings
also do not share bounds. The tool has no daemon, cluster coordinator or multi-Pod
CLI. Duplicate callers for the same PID may conflict at ptrace; requests are not
deduplicated. Existing inode/ownership fencing rejects credit-file replacement.

Linux release builds and51+51 unit tests include fitting-younger FIFO exclusion,
cancelled/killed head cleanup and impossible pre-stop reservations. Four live
two-Pod mixed-size tests (16/256/600MiB children) verified ticket order, admission
before attach, known pages, all credits returned, waiter cancellation, SIGKILL
while tracing, other-Pod progress and pre-existing stop preservation. Sources
were unchanged in the final metadata-only provenance refresh. This is bounded
validation; measured node Dirty/Writeback and collector RSS are not hard limits.
See [current measurements and methods](benchmarks.md).

## Capture, writing and durability scheduling

Capture workers pass owned snapshots to independent writers. Buffered targets
resume after the entire selected range is copied, before disk writing. Writers
pass the original open file description to independent finalizers. Snapshot
credits release after writing; backlog/file credits release only after finalization.
An output larger than the backlog limit holds all backlog credits exclusively;
its actual size can exceed that credit limit and is recorded. File limits also
respect the process descriptor limit. Kernel page cache is outside the snapshot
budget and must be assessed with node memory/pressure measurements.
These bounds limit queued payload, not total node RSS or memory consumption;
they cannot guarantee absence of OOM under other allocations or workloads.

The writeback policy is `hint`, which requests asynchronous writeback and never counts as a durable commit; failure is explicit. Finalizers verify the named inode, fsync the original
description and publish the core. A finalizer receives the first item and an
immediately ready group under one receiver-lock lifetime; it never reacquires
that lock while retaining a pending output. There is no fixed-size wave barrier.
The final manifest, output directory and its parent are synchronized before
exit 0. A manifest marked `complete` must be paired with successful invocation
exit to establish that the final persistence calls succeeded.

An independent OFD lock serializes the core-fsync/publication section across
participating invocations. It is a per-core fsync gate, not `syncfs`; every core
still receives its own fsync. Process death releases the gate. Waiting can exceed
capture cancellation, like an in-flight fsync, but no target is stopped while
waiting. Snapshot writing and reading can continue within the shared credits.
Other applications, kernel background writeback and the collector's separate
manifest/directory fsync calls are outside this per-core gate. Preventing a
node-wide dirty-writeback storm has not been demonstrated.

The production finalizer groups up to eight immediately ready items and fsyncs every file. Capture/write deadlines
are cooperative; final persistence attempts finish afterward. SIGINT/SIGTERM
observed during durability makes the invocation partial, while already captured
files finish persistence. This does not impose a hard bound on kernel I/O.

Retaining the original file description follows the kernel's
[writeback error-reporting contract](https://docs.kernel.org/filesystems/vfs.html#handling-errors-during-writeback).
The writeback hint's limitations are described by
[sync_file_range(2)](https://man7.org/linux/man-pages/man2/sync_file_range.2.html).
The injected fault checks exercise syscall-level error handling; they are not
a physical power-loss or storage-device failure certification.

Per-thread CPU and stage durations measure aggregate work. Overlapping worker
durations must not be divided by total wall time as phase percentages. Resource
comparisons include external durability work when a baseline lacks built-in
fsync. Collection completion and final durable process exit are separate boundaries.

## ELF and evidence

The writer derives from ProcDump-Linux `corex`. It stores GP/FP/x86 XSAVE register
sets, auxiliary vectors, file mappings, and host thread IDs. Some inherited
`PRSTATUS` signal masks/accounting fields and `PRPSINFO` state fields are
placeholders, not measured signal or CPU-accounting evidence. `NT_SIGINFO` is
omitted because the acquisition did not observe a fatal signal.

The validated format is Linux ELF64 x86-64, little-endian. More than 65,533
selected VMAs is rejected because extended program-header numbering is not
implemented. Empty, overlapping, non-page-aligned, or unsupported selected
ranges and malformed register/auxiliary-vector payloads produce explicit errors.

Preserve the original files and manifest. A successful acquisition is not a
claim that the process was benign, malicious, or internally application-consistent.
Different processes have different capture times, and external writers can
change shared memory while an individual process is stopped.

## GDB and container thread IDs

GDB can discover namespace TIDs through `libthread_db` in addition to host TIDs
in the core notes. To inspect only the captured host-TID contexts:

```sh
gdb --nx -iex 'set libthread-db-search-path /nonexistent' /path/to/executable core
```

Then use `info threads` and `thread apply all bt`. Use the executable and library
versions from the captured container. This workaround does not supply missing
symbols or guarantee reliable unwinding for every runtime.

## CLI presentation

`poddump --pod UUID --output NEW_DIR` prints a container/process tree after the
manifest and directory persistence barrier succeeds. The renderer consumes
that manifest; it does not query targets again or change memory collection.
It orders full container IDs and PID/start-time identities deterministically.
Abbreviated container prefixes expand on collisions. Failed and uncaptured
identities, census errors and cancellation remain explicit in partial output.
Host PIDs and sandbox inclusion are identified in the footer.

The durable manifest is the machine-readable interface. The CLI accepts only
`--pod` and `--output` (along with `-h`/`--help` and `-V`/`--version`); capture,
budget, writeback, and durability policies use fixed defaults and internally
generated request IDs.
