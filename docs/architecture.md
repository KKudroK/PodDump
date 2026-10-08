# Architecture and support boundaries

## Discovery and identity

The collector resolves a full Pod UID beneath the host cgroup-v2 hierarchy,
supporting the validated containerd systemd/cgroupfs layouts. It recursively
enumerates TGIDs, including child processes and the sandbox. Container
attribution is derived from the observed hierarchy; ambiguous attribution is
an explicit error. Names and numeric PIDs alone are insufficient identities.

The manifest ties the node hostname and boot ID to Pod UID, container/cgroup,
PID/start time, and captured thread start times. The initial census is checked
for stability. Additional census rounds collect new identities, while remaining
uncaptured identities are reported when the configured bound is reached.

## Stop, capture, and release

The worker uses `PTRACE_SEIZE` and `PTRACE_INTERRUPT`, waits for observed threads
to stop, and repeats the thread census. The default `/proc/PID/mem` descriptor
is opened after the observed threads stop, avoiding capture through an old
address-space descriptor when `exec` races with attachment.

The capture paths preserve real pending-signal delivery and a pre-existing group
stop. Cleanup failures are explicit. The implementation has a shared bounded
attach interval and a per-process userspace deadline; an uninterruptible kernel
operation can still exceed a userspace deadline.

Buffered capture copies the selected contents into an anonymous mapping that
holds the final ELF layout. The target is detached before sparse writing and
file synchronization. Streaming captures read and write while stopped, then
detach and synchronize. Actual zero bytes may become sparse holes. Read failures
never become fabricated successful zero data.

Each invocation admits a bounded number of workers and snapshot bytes. Multiple
invocations require an external global scheduler to share these limits. Version
0.2.0 has no cluster coordinator or multi-Pod command-line interface.

## Pod capture and durability scheduling in 0.2.0

The default `batch` mode separates capture workers from final persistence.
Each worker writes a `.core.partial`, requests `sync_file_range(WRITE)` data
writeback, and proceeds to another process. The original open file description
remains owned until its final `fsync`; successful publication also checks that
the partial name still refers to that inode. Windows of pending files are capped
at 256 and reduced according to the process file-descriptor limit.

The hint can overlap disk work with later captures. It may block under queue
pressure and never counts as a durable commit. Unsupported hints are recorded
and fall back to final `fsync`; actual I/O errors become explicit failed rows.
After all files in a window are synchronized, complete cores are renamed.
The final manifest, output directory and its parent are synchronized before
exit 0. A manifest marked `complete` must be paired with successful invocation
exit to establish that the final persistence calls succeeded.

`--durability per-process` keeps synchronization in each capture worker as a
mechanism control. It is not the default speed result. Capture/write deadlines
are cooperative; final persistence attempts finish afterward. SIGINT/SIGTERM
observed during durability makes the invocation partial, while already captured
files finish persistence. This does not impose a hard bound on kernel I/O.

Retaining the original file description follows the kernel's
[writeback error-reporting contract](https://docs.kernel.org/filesystems/vfs.html#handling-errors-during-writeback).
The writeback hint's limitations are described by
[sync_file_range(2)](https://man7.org/linux/man-pages/man2/sync_file_range.2.html).
The injected fault checks exercise syscall-level error handling; they are not
a physical power-loss or storage-device failure certification.

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

## CLI presentation in 0.2.1

`poddump --pod UUID --output NEW_DIR` prints a container/process tree after the
manifest and directory persistence barrier succeeds. The renderer consumes
that manifest; it does not query targets again or change memory collection.
It orders full container IDs and PID/start-time identities deterministically.
Abbreviated container prefixes expand on collisions. Failed and uncaptured
identities, census errors and cancellation remain explicit in partial output.
Host PIDs and sandbox inclusion are identified in the footer.

`--format json` retains the earlier machine-readable stdout contract;
`--full-ids` expands the tree IDs. `--pod-uid` is a compatibility alias for
`--pod`. Error diagnostics, manifest schema and exit codes are preserved.
