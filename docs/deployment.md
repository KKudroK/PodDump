# Standalone deployment — 2026-10-17

The executable is `poddump` (0.3.1), a static PIE for Linux x86-64. Copy this one file to the node and make it executable. Rust, Python, glibc, GDB, kubectl, container runtime CLIs, a sidecar, and adjacent metadata files are not runtime dependencies. `scripts/build_release.py` and `vendor/` are for build hosts only.

```sh
chmod 755 poddump
./poddump --version
./poddump --build-info
sudo ./poddump --pod FULL_POD_UID --output /evidence/new_collection --request-id incident_001
```

## Supported deployment boundary

- This release targets x86-64 only. The [README](../README.md#tested-environments) lists the tested distribution/kernel pairs; the [additional VM matrix](distro_validation.md) records Debian 12/13, Rocky 8/9 and Kali. Synthetic cgroup tests and actual Ubuntu Kubernetes/containerd integration are distinguished. ARM64 and arbitrary cloud distributions are not certified by these results.
- Run against the host's PID/proc/cgroup view with permission to ptrace every selected process. Copying the binary cannot grant kernel capabilities or bypass Yama, SELinux, AppArmor, seccomp, or namespace restrictions. No security settings are changed.
- The output parent must already exist. Use persistent Linux storage supporting private directory/file modes, exclusive creation, `renameat2(RENAME_NOREPLACE)`, and file/directory `fsync`. Local ext4 was exercised. Network filesystems are not qualified by this test. tmpfs was used only for bounded failure tests; it is volatile even when fsync returns success.
- A writable private runtime directory is needed for node credits and Pod locks. It is created automatically; no daemon/configuration file is required. All cooperating invocations must use the same effective UID and runtime directory in a shared mount view. `PODDUMP_RUNTIME_DIR` selects a common path when needed. Do not remove/replace active lock files. Stop all older collectors before upgrading: older binaries do not participate in Pod admission.

## Duplicate requests and recapture

One collection at a time is admitted for each Pod UID. A different active request waits before discovery, without attaching or stopping targets; after admission it discovers current PID/start-time/cgroup identities again. The default wait limit is 60 seconds, adjustable with `--wait-timeout-ms 0..3600000`; zero rejects a busy Pod immediately. SIGINT/SIGTERM cancels admission. OFD locks are released by the kernel on exit, including SIGKILL.

An invocation with the same Pod UID and **same active `--request-id`** returns JSON containing `status: already_running`, the active request and its output path, and exits **3**. It does not create the requested output or claim that evidence is complete. Follow the original job's exit status and manifest. This is active-request coalescing, not a persistent incident cache: invoking after the prior job has finished starts a fresh observation, even with the same label. Use a fresh request ID for a different observation time.

Exit 0 means successful completion and publication under the recorded collection policy; exit 2 means partial collection; exit 1 means invalid input, setup or final persistence failure. Preserve stderr. A hard kill can leave fragments without a final manifest; these are incomplete evidence. Per-process stopping, bounded streaming, non-atomic Pod snapshots, externally changing shared mappings, and cooperative kernel-I/O cancellation remain limitations. Runtime budgets are not a total host RSS/Dirty/space guarantee.

## Build identity and rebuilding

`--build-info` and collection manifests contain automatically generated source SHA-256 values, a vendored-tree digest, actual features, target and compiler. `runtime_executable_sha256` hashes `/proc/self/exe` on that invocation. The binary never embeds a claimed hash of itself. `release/manifest.json` records the independently computed final file hash; neither that file nor source metadata is needed to execute the binary. Rehash after stripping or otherwise modifying a binary.

With Rust 1.90.0 and its `x86_64-unknown-linux-musl` standard library already installed, run `python3 scripts/build_release.py`. It builds/tests offline, rejects dynamic dependencies and development features, and emits the binary plus a receipt. Use the generic x86-64 target, not host-specific CPU flags, for distribution. Build-host Python is not a deployment dependency.

The 2026-10-17 qualification is stability and deployability work. Earlier benchmark tables are historical measurements, not current performance, cloud-wide compatibility, exhaustive memory coverage or service-availability guarantees.
