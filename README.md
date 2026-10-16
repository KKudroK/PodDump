# PodDump: Kubernetes-Native Memory Acquisition Engine

<div align="center">

[**🇰🇷 한국어 설명서로 전환**](#-한국어-korean) &nbsp;&nbsp;|&nbsp;&nbsp; [**🇺🇸 Switch to English Readme**](#-english)

</div>

---

<details open id="-한국어-korean">
<summary><b>🌐 한국어 (Korean)</b></summary>

<br>

**PodDump**는 쿠버네티스(Kubernetes) 환경의 컨테이너 및 파드 구조에 특화된 **고성능·무중단 메모리 포렌식 수집 엔진**입니다.

호스트 노드에서 **단일 Pod UID**를 입력받아 파드 내 모든 컨테이너(애플리케이션, 사이드카, 인프라 샌드박스)의 프로세스를 자동으로 식별하고, ptrace 기반 정밀 제어와 다중 작업자 파이프라인을 통해 프로세스 메모리를 표준 ELF Core 파일로 안전하게 수집합니다.

---

## 왜 PodDump인가? (레거시 도구 대비 핵심 강점)

기존의 단일 프로세스 기반 도구(`ProcDump`, `gcore`, `GDB`)를 스크립트로 감싸서 쿠버네티스 메모리를 수집하는 방식은 서비스 다운타임, 노드 OOM, 기가바이트 단위 가상 메모리 지연 등의 한계를 지닙니다. PodDump는 이를 근본적으로 해결하기 위해 설계되었습니다.

### 1. 다중 프로세스 확장성 (Multi-Process Scalability)
- **최대 2.4x ~ 3.4x 빠른 수집 완료**: 단일 PID를 순차적으로 덤프하는 방식과 달리, 4개 리더 작업자와 2개 라이터 작업자로 구성된 비동기 스트리밍 파이프라인을 통해 파드 내 6~28개 프로세스를 단 **0.22초 ~ 0.75초** 만에 수집 완료합니다.
- **CPU 오버헤드 최대 55% 절감**: 중복된 procfs 파싱 및 프로세스 생성 비용을 제거하여 대규모 다중 프로세스 수집 시 GDB 대비 절반 이하의 CPU 시간만 소모합니다.

### 2. 가상 메모리 예약 최적화 & 커널·런타임 범용성
- **5.4배 속도 향상 & 12배 정지 시간 단축**: Prometheus Exporter, Fluentbit 등 클라우드 네이티브 사이드카는 Go/JVM 런타임의 주소 공간 예약으로 인해 1.2 GiB 이상의 익명 `PROT_NONE` 매핑을 가집니다. 기존 도구는 이를 무차별 수집하여 수 초간의 프로세스 멈춤을 초래했습니다.
- PodDump는 `/proc/{pid}/smaps`를 검사하여 실제 물리 메모리가 할당되지 않은(`Rss: 0 kB` 및 `Swap: 0 kB`) 미커밋 익명 예약을 안전하게 선별 건너뜁니다. 그 결과 4파드(28개 PID) 수집 시간이 **0.75초(순수 명령 소요 시간)**로 단축되고, 프로세스 정지 시간은 **83.6ms** 수준으로 억제됩니다.
- Linux 4.x, 5.x, 6.x 커널 및 커널 VMA 이름 지정 유무와 관계없이 Go, JVM, V8/Node.js, .NET, jemalloc 등 모든 런타임 예약에 범용적으로 동작합니다.

### 3. 실서비스 무중단 보장 (Zero-Downtime QoS)
- **실시간 요청 유실 0건 (100% 가용성)**: `ProcDump`는 디스크 쓰기와 fsync가 끝날 때까지 대상을 멈춰두기 때문에 530ms 이상의 멈춤이 발생하며, 실시간 트래픽 환경(Redis 등)에서 **커넥션 타임아웃 및 요청 유실(3건 오류 실측)**을 발생시킵니다.
- PodDump는 512 MiB 인메모리 스냅샷 버퍼를 활용해 메모리 복사 직후 대상을 즉시 재개(StopMax 14~90ms 수준)하고 디스크 저장을 비동기로 처리하여 **운영 중인 서비스의 요청 실패를 0건**으로 방지합니다.

### 4. 공유 예산 프로토콜 (`credits-v3`) 기반 노드 안전성
- 여러 파드를 동시에 수집하더라도 리눅스 OFD(Open File Description) 락 기반의 노드 공유 예산을 통해 **동시 멈춤 프로세스 수(최대 4개)**, **스냅샷 메모리(최대 512 MiB)**, **I/O 백로그(최대 1 GiB)**를 엄격히 제한합니다.
- 예산이 고갈되면 FIFO 대기열에서 안전하게 대기하므로, 수집 도구로 인해 쿠버네티스 노드가 OOM Killer에 빠지거나 디스크 I/O가 마비되는 사고를 원천 방지합니다.

### 5. 포렌식 무결성 & 희소 파일(Sparse File) 디스크 최적화
- GDB나 gcore는 커널 특수 매핑(`[vsyscall]`) 읽기 실패 시 임의의 0으로 채워진 4 KiB 가짜 페이지를 삽입하지만, PodDump는 **단 1바이트의 허위 데이터도 날조하지 않습니다**.
- 실제 성공적으로 읽은 바이트만 ELF `PT_LOAD`에 매핑하며, 누락 구간은 `manifest.json` 원장에 주소 범위와 errno를 투명하게 기록하여 법적 증거 능력을 보장합니다.
- 런타임 감지 AVX2 SIMD 및 스칼라 폴백 기반의 64 KiB 단위 제로 블록 감지로 디스크 블록 할당(`AllocatedBytes`)을 물리 메모리 크기 수준으로 압축 스킵(Redis 231MB의 경우 45~50MB 블록 절약)합니다.

---

## 클라우드 및 다중 런타임 범용 호환성

특정 VM이나 단일 컨테이너 런타임에 종속되지 않고, 주요 클라우드(AWS EKS, GCP GKE, Azure AKS, OpenShift, k3s, kubeadm) 및 다양한 런타임 환경에서 즉시 동작하도록 설계되었습니다:

- **다중 컨테이너 런타임 지원**:
  - **containerd**: `cri-containerd-<64hex>.scope`, `<64hex>` cgroupfs
  - **CRI-O**: `crio-<64hex>.scope` (컨테이너 외부 모니터인 `crio-conmon-` 격리 배제), `crio-<64hex>`
  - **Docker / cri-dockerd**: `docker-<64hex>.scope`, `docker/<64hex>`
  - **Podman**: `libpod-<64hex>.scope`
- **cgroup v2 및 cgroup v1 / Hybrid 동시 지원**:
  - 최신 cgroup v2 통합 계층 구조(`cgroup.controllers`) 자동 탐색
  - 기존 cgroup v1 / 하이브리드 노드풀(`/sys/fs/cgroup/pids`, `/sys/fs/cgroup/systemd`, `/sys/fs/cgroup/memory`) 완벽 지원
- **DaemonSet 및 호스트 경로 유연성**:
  - 포렌식 DaemonSet 파드에서 호스트 cgroup 마운트 시 `/host/sys/fs/cgroup` 자동 감지 및 `PODDUMP_CGROUP_ROOT` 환경변수 지원
  - 읽기 전용 루트 파일시스템 환경에서도 동작하도록 `PODDUMP_RUNTIME_DIR`, `/run`, `/var/run`, `/var/tmp` 순차 폴백 (소유자 전용 `0o700` 권한 엄격 검증)
- **아키텍처 및 하이퍼바이저 독립성**:
  - x86_64 런타임 AVX2 감지 + 4-unrolled u64 스칼라/aarch64 폴백
  - 중첩 가상화나 제한된 클라우드 vCPU 인스턴스에서 하이퍼바이저가 XSAVE(`NT_X86_XSTATE`)를 마스킹할 경우 표준 FXSAVE(`NT_PRFPREG`)로 안전한 폴백
  - 클라우드 네트워크 스토리지(EFS/NFS/CephFS/overlayfs)에서 `posix_fallocate`가 미지원될 경우 제로 블록 직접 기록 폴백

---

## 실측 성능 및 운영 벤치마크

단일 노드 쿠버네티스 환경(Ubuntu 24.04.5 LTS, Linux 6.8, 4 vCPU, 9 GiB RAM)에서 12개 파드, 60개 프로세스를 대상으로 실측한 결과입니다 (상세 데이터는 [성능 및 운영 검증 기록](docs/benchmarks.md) 참조).

### 1. 수집 처리 시간 및 속도 비교 (Throughput & Scalability)

| 대상 워크로드 | 파드 수 | 프로세스 수 | 메모리 프로파일 | PodDump 순수실행 (`cmd_span`) | PodDump 전체하네스 (`wall`) | ProcDump (Expanded) | GDB (readnever) | 성능 향상률 (Speedup) | CPU 시간 절감 |
|:---|:---:|:---:|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** | ~15 MiB RSS / PID | **0.218초** | 0.230초 | 0.520초 | 0.550초 | **2.39x ~ 2.52x 빠름** | 60% ~ 71% 절감 |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** | ~15 MiB RSS / PID | **0.325초** | 0.338초 | 0.913초 | 1.022초 | **2.81x ~ 3.14x 빠름** | 67% ~ 72% 절감 |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** | ~15 MiB RSS / PID | **0.568초** | 0.581초 | 1.743초 | 1.944초 | **3.07x ~ 3.42x 빠름** | 65% ~ 69% 절감 |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** | 1.2 GiB VMA (Go 예약) | **0.269초** | 0.280초 | 0.520초 | 0.892초 | **1.93x ~ 3.32x 빠름** | 42% ~ 70% 절감 |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** | 1.2 GiB VMA (Go 예약) | **0.428초** | 0.438초 | 0.932초 | 0.952초 | **2.18x ~ 2.22x 빠름** | 31% ~ 32% 절감 |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** | 1.2 GiB VMA (Go 예약) | **0.747초** | 0.776초 | 1.791초 | 1.945초 | **2.40x ~ 2.60x 빠름** | 42% ~ 43% 절감 |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** | ~128 MiB RSS / PID | **0.720초** | 0.727초 | 0.618초 | 0.638초 | 동등 (비동기 내구성) | - |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** | ~128 MiB RSS / PID | **0.926초** | 0.933초 | 0.653초 | 0.870초 | 동등 (노드 예산 보호) | - |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** | ~128 MiB RSS / PID | **1.969초** | 1.984초 | 1.163초 | 1.279초 | 안전 2파동 직렬화 | - |

---

### 2. 서비스 정지 지연시간 및 가용성 보장 (Stop Freeze Latency & Zero-Downtime QoS)

| 대상 워크로드 | 파드 수 | 프로세스 수 | PodDump 최대 멈춤 (Max Freeze) | ProcDump 최대 멈춤 | GDB 최대 멈춤 | 멈춤 지연 단축율 | 트래픽 P99 응답지연 | PodDump 서비스 실패 | ProcDump 서비스 실패 |
|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** | **14.4 ms** | 77.2 ms | 78.9 ms | **5.4x 단축** | 4.1 ms | **0건 (100% 정상)** | 0건 |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** | **15.6 ms** | 96.4 ms | 85.4 ms | **5.5x ~ 6.2x 단축** | 11.8 ms | **0건 (100% 정상)** | 0건 |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** | **26.9 ms** | 94.2 ms | 88.1 ms | **3.3x ~ 3.5x 단축** | 39.6 ms | **0건 (100% 정상)** | 0건 |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** | **46.9 ms** | 142.8 ms | 250.9 ms | **3.0x ~ 5.3x 단축** | 5.4 ms | **0건 (100% 정상)** | 0건 |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** | **90.5 ms** | 153.8 ms | 116.6 ms | **1.3x ~ 1.7x 단축** | 21.1 ms | **0건 (100% 정상)** | 0건 |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** | **83.6 ms** | 153.6 ms | 245.5 ms | **1.8x ~ 2.9x 단축** | 37.5 ms | **0건 (100% 정상)** | 0건 |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** | **272.9 ms** | 331.6 ms | 115.4 ms | ProcDump 대비 1.2x 단축 | 253.3 ms | **0건 (100% 정상)** | 0건 |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** | **319.2 ms** | 418.8 ms | 110.1 ms | ProcDump 대비 1.3x 단축 | 295.7 ms | **0건 (100% 정상)** | 0건 |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** | **365.8 ms** | **531.0 ms** | 136.8 ms | **1.45x 단축** | 339.8 ms | **0건 (100% 정상)** | **3건 타임아웃 유실** |

---

### 3. 디스크 스토리지 및 희소 파일 절감 (Storage & Sparse Optimization)

| 대상 워크로드 | 파드 수 | 프로세스 수 | 논리 코어 크기 (CoreBytes) | 실제 디스크 할당 (AllocatedBytes) | 절감된 디스크 블록 | 디스크 절감율 |
|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** | 79.9 MiB | 56.3 MiB | **23.6 MiB 절감** | 29.5% |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** | 159.8 MiB | 112.7 MiB | **47.1 MiB 절감** | 29.5% |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** | 319.6 MiB | 225.3 MiB | **94.3 MiB 절감** | 29.5% |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** | 147.2 MiB | 76.1 MiB | **71.1 MiB 절감** | 48.3% |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** | 294.4 MiB | 152.5 MiB | **141.9 MiB 절감** | 48.2% |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** | 588.7 MiB | 306.5 MiB | **282.2 MiB 절감** | 47.9% |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** | 231.4 MiB | 179.4 MiB | **52.0 MiB 절감** | 22.5% |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** | 456.7 MiB | 359.2 MiB | **97.5 MiB 절감** | 21.3% |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** | 907.4 MiB | 718.4 MiB | **189.0 MiB 절감** | 20.8% |

---

## 실행 환경과 설치

Linux x86-64 또는 aarch64, cgroup v1/v2 및 containerd/CRI-O/Docker 기반 호스트에서 사용합니다. 대상 프로세스에 대한 ptrace 권한과 호스트 `/proc` 접근을 위해 `sudo` 권한이 필요합니다. 대상 Pod가 실행 중인 워커 노드에서 직접 호출하거나 권한 있는 포렌식 DaemonSet을 통해 실행합니다.

### 빌드 및 설치
Rust edition 2024를 사용하며 vendored 의존성을 포함하고 있어 오프라인 환경에서도 즉시 빌드할 수 있습니다.

```bash
# 저장소 루트에서 빌드 및 단위 테스트 수행 (55개 전수 통과)
cargo build --release --locked --offline
cargo test --locked --offline
cargo test --locked --offline --features development-options

# 실행 파일 설치
install -d -m 700 "$HOME/.local/bin"
install -m 755 target/release/poddump "$HOME/.local/bin/poddump"
```

---

## 사용법

```text
poddump --pod FULL_POD_UID --output NEW_DIR [--request-id ID]
```

| 인자 | 설명 |
|:---|:---|
| `--pod` | 대상 파드의 전체 UID (Full Kubernetes Pod UID). 이름이나 축약형 접두사는 허용되지 않습니다. |
| `--output` | 덤프 산출물을 저장할 새 디렉터리 경로. 부모 디렉터리는 존재해야 하며, 대상 경로는 사전에 존재하면 안 됩니다. |
| `--request-id` | (선택) 호출 추적 및 SIEM/SOAR 연계를 위한 식별자 ID. |
| `--pod-uid` | `--pod`의 호환 별칭. |
| `--help` | 상세 도움말 출력. |

### 실행 예시

```bash
NAMESPACE="production"
POD="web-api-7b89f"

# 1. 대상 파드의 전체 UID 획득
POD_UID=$(kubectl get pod -n "$NAMESPACE" "$POD" -o jsonpath='{.metadata.uid}')

# 2. 전용 세션 디렉터리에서 안전하게 수집 실행
SESSION=$(mktemp -d /var/tmp/poddump-session.XXXXXXXX)
sudo "$HOME/.local/bin/poddump" \
  --pod "$POD_UID" \
  --output "$SESSION/capture" \
  --request-id "incident-alert-1042" \
  >"$SESSION/stdout.log" 2>"$SESSION/stderr.log"

RC=$?
printf 'Exit Code: %s, Capture Dir: %s\n' "$RC" "$SESSION/capture"

# 3. 수집 결과 및 메타데이터 감사 원장 확인
sudo python3 -m json.tool "$SESSION/capture/manifest.json"
sudo ls -lh "$SESSION/capture"
```

---

## 산출물 구조 및 종료 코드

출력 디렉터리는 `0700`, 생성된 Core 및 Manifest 파일은 `0600` 보안 권한으로 엄격히 보호됩니다.

```text
capture/
  ├── manifest.json              # 파드 메타데이터, 프로세스 신원, 세부 수집 커버리지 감사 원장
  ├── <host_pid>_<starttime>.core # 수집 완료된 프로세스별 ELF Core 이미지
  └── ...
```

### 종료 코드 규격
- **`0` (Complete)**: 파드 내 모든 대상 프로세스의 메모리 수집이 성공적으로 완료되었으며, Core 이미지 및 Manifest의 디스크 fsync 내구성이 완전히 검증됨.
- **`2` (Partial)**: 프로세스 메모리 수집은 정상 수행되었으나, 커널 특수 매핑(`[vvar]` 등)의 읽기 불가 영역이나 프로세스 종료 등으로 일부 바이트가 누락되어 감사 원장에 기록됨 (완료된 바이트는 `.core` 또는 `.core.partial`로 보존).
- **`1` (Error)**: 필수 인자 누락, 권한 부족, 출력 경로 충돌, 대상 프로세스 탐색 실패 등으로 수집이 중단됨.

---

## 내부 아키텍처 및 자원 예산

PodDump는 단일 노드 내 여러 인스턴스가 동시에 실행될 때도 시스템 리소스를 독점하지 않도록 `/run/poddump-<euid>/credits-v3` 파일을 통해 노드 수준의 동시성을 조율합니다.

```
[ Ingestion Pipeline ]
  Target Pod UID
       │
       ▼
 [ Process Census ] ──► cgroup v1/v2 기반 파드 내 모든 컨테이너 PID 자동 탐색
       │
       ▼
 [ FIFO Admission ] ──► Node-wide credits-v3 자원 할당 대기
       │
       ├──► Concurrent Stops : 최대 4개 PID 동시 ptrace 정지
       ├──► Memory Buffer    : 최대 512 MiB 스냅샷 메모리 풀
       └──► I/O Backlog      : 최대 1 GiB 디스크 쓰기 큐
       │
       ▼
 [ Parallel Reader ] ──► 4개 워커가 /proc/{pid}/mem 병렬 스트림 복사
       │
       ▼ (메모리 복사 완료 즉시 Target Resume -> 다운타임 해제)
 [ Async Writer ]   ──► 2개 디스크 라이터가 ELF64 ET_CORE 포맷으로 비동기 스트림 기록
       │
       ▼
 [ Durability Fsync ]──► Core & Manifest 원장 디스크 fsync 보장 후 완료 보고
```

상세한 내부 설계와 복구 메커니즘은 [아키텍처 문서](docs/architecture.md) 및 [성능 및 운영 검증 기록](docs/benchmarks.md)을 참조하십시오.

---

## 라이선스

[MIT License](LICENSE)를 따릅니다. Microsoft의 ProcDump for Linux core 엔진을 기반으로 쿠버네티스 컨테이너 환경에 맞추어 Rust로 재설계 및 고도화되었습니다. 상세 고지는 [NOTICE](NOTICE.txt) 및 [상위 라이선스](licenses/procdump_mit.txt)에 있습니다.

</details>

---

<details id="-english">
<summary><b>🌐 English</b></summary>

<br>

**PodDump** is a **high-performance, non-disruptive memory forensics acquisition engine** engineered specifically for Kubernetes container and pod architectures.

Given a **single Pod UID** on a host node, PodDump autonomously resolves and captures all processes across all containers within the Pod (applications, sidecars, and sandbox infrastructure) into standard ELF core files using precision ptrace control and an asynchronous multi-worker streaming pipeline.

---

## Why PodDump? (Key Merits over Legacy Tools)

Legacy single-process tools (`ProcDump`, `gcore`, `GDB`) wrapped in shell scripts suffer from severe service downtime, node-wide OOM risks, and multi-gigabyte virtual address space latency in containerized environments. PodDump eliminates these fundamental bottlenecks by design.

### 1. Multi-Process Scalability
- **2.4x ~ 3.4x Faster Capture Completion**: Instead of sequential per-PID dump scripts, PodDump utilizes an internal pipeline of 4 parallel reader workers and 2 async disk writers, capturing 6 to 28 container processes in just **0.22s to 0.75s**.
- **Up to 55% Lower CPU Overhead**: Eliminates redundant procfs rescanning and child process spawns, using less than half the CPU cycles consumed by GDB on multi-process pods.

### 2. Virtual Reservation Optimization & Kernel Agnosticism
- **5.4x Acceleration & 12x Freeze Reduction**: Cloud-native sidecars (Prometheus Exporters, Fluentbit, Envoy) allocate over 1.2 GiB of uncommitted anonymous `PROT_NONE` guard pages via Go/JVM runtimes. Legacy tools dump these blindly, freezing targets for seconds.
- PodDump inspects `/proc/{pid}/smaps` to verify `Rss: 0 kB` and `Swap: 0 kB`, safely bypassing uncommitted reservations without fabricating zero data. For 4 Pods (28 PIDs), capture time drops to **0.75s** with target freeze suppressed to **83.6ms**.
- Works universally across Linux 4.x, 5.x, and 6.x kernels, supporting Go, JVM, V8/Node.js, .NET, and jemalloc allocators without relying on kernel-specific VMA naming.

### 3. Zero-Downtime Service QoS
- **Zero Request Drops (100% Availability)**: `ProcDump` holds processes frozen during disk writes and fsync, causing stop latencies up to **531ms** and dropping live TCP connections under load (3 query timeouts recorded on Redis).
- PodDump captures memory into a 512 MiB in-memory snapshot buffer and **immediately resumes the target** (freeze limited to 14ms ~ 90ms), persisting data asynchronously to ensure **zero request failures** on live production traffic.

### 4. Node Safety via Shared Kernel Budget (`credits-v3`)
- Uses Linux Open File Description (OFD) range locks on `/run/poddump-<uid>/credits-v3` to enforce node-wide resource quotas across concurrent PodDump instances:
  - Max 4 concurrently stopped processes node-wide
  - Max 512 MiB total snapshot buffer
  - Max 1 GiB pending write backlog
- Enforces strict FIFO admission, preventing memory acquisition from triggering node OOM Killer cascades or disk I/O starvation.

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

Evaluated on a Kubernetes node (Ubuntu 24.04.5 LTS, Linux 6.8, 4 vCPU, 9 GiB RAM) hosting 12 service Pods and 60 target processes under live traffic load (see [Detailed Benchmark Audit](docs/benchmarks.md)).

### 1. Acquisition Throughput & Speedup Matrix

| Target Workload | Pod Count | Target PIDs | Memory Profile | PodDump Command Span | PodDump Total Wall | ProcDump (Expanded) | GDB (readnever) | Speedup Advantage | CPU Time Savings |
|:---|:---:|:---:|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** | ~15 MiB RSS / PID | **0.218s** | 0.230s | 0.520s | 0.550s | **2.39x ~ 2.52x faster** | 60% ~ 71% less CPU |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** | ~15 MiB RSS / PID | **0.325s** | 0.338s | 0.913s | 1.022s | **2.81x ~ 3.14x faster** | 67% ~ 72% less CPU |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** | ~15 MiB RSS / PID | **0.568초** | 0.581s | 1.743s | 1.944s | **3.07x ~ 3.42x faster** | 65% ~ 69% less CPU |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** | 1.2 GiB VMA (Go heap) | **0.269s** | 0.280s | 0.520s | 0.892s | **1.93x ~ 3.32x faster** | 42% ~ 70% less CPU |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** | 1.2 GiB VMA (Go heap) | **0.428s** | 0.438s | 0.932s | 0.952s | **2.18x ~ 2.22x faster** | 31% ~ 32% less CPU |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** | 1.2 GiB VMA (Go heap) | **0.747s** | 0.776s | 1.791s | 1.945s | **2.40x ~ 2.60x faster** | 42% ~ 43% less CPU |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** | ~128 MiB RSS / PID | **0.720s** | 0.727s | 0.618s | 0.638s | Comparable (async durability) | - |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** | ~128 MiB RSS / PID | **0.926s** | 0.933s | 0.653s | 0.870s | Comparable (node safety) | - |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** | ~128 MiB RSS / PID | **1.969s** | 1.984s | 1.163s | 1.279s | Safe 2-wave FIFO execution | - |

---

### 2. Stop Freeze Latency & Zero-Downtime QoS

| Target Workload | Pod Count | Target PIDs | PodDump Max Stop Freeze | ProcDump Max Freeze | GDB Max Freeze | Freeze Reduction | P99 Request Latency | PodDump Errors | ProcDump Errors |
|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** | **14.4 ms** | 77.2 ms | 78.9 ms | **5.4x shorter** | 4.1 ms | **0 (100% QoS)** | 0 |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** | **15.6 ms** | 96.4 ms | 85.4 ms | **5.5x ~ 6.2x shorter** | 11.8 ms | **0 (100% QoS)** | 0 |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** | **26.9 ms** | 94.2 ms | 88.1 ms | **3.3x ~ 3.5x shorter** | 39.6 ms | **0 (100% QoS)** | 0 |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** | **46.9 ms** | 142.8 ms | 250.9 ms | **3.0x ~ 5.3x shorter** | 5.4 ms | **0 (100% QoS)** | 0 |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** | **90.5 ms** | 153.8 ms | 116.6 ms | **1.3x ~ 1.7x shorter** | 21.1 ms | **0 (100% QoS)** | 0 |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** | **83.6 ms** | 153.6 ms | 245.5 ms | **1.8x ~ 2.9x shorter** | 37.5 ms | **0 (100% QoS)** | 0 |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** | **272.9 ms** | 331.6 ms | 115.4 ms | 1.2x shorter than ProcDump | 253.3 ms | **0 (100% QoS)** | 0 |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** | **319.2 ms** | 418.8 ms | 110.1 ms | 1.3x shorter than ProcDump | 295.7 ms | **0 (100% QoS)** | 0 |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** | **365.8 ms** | **531.0 ms** | 136.8 ms | **1.45x shorter** | 339.8 ms | **0 (100% QoS)** | **3 Dropped Queries** |

---

### 3. Storage Allocation & Sparse Optimization

| Target Workload | Pod Count | Target PIDs | Logical Size (CoreBytes) | Physical Allocation (AllocatedBytes) | Disk Blocks Saved | Disk Reduction |
|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **6 PIDs** | 79.9 MiB | 56.3 MiB | **23.6 MiB saved** | 29.5% |
| **Nginx Web Server** | **2 Pods** | **12 PIDs** | 159.8 MiB | 112.7 MiB | **47.1 MiB saved** | 29.5% |
| **Nginx Web Server** | **4 Pods** | **24 PIDs** | 319.6 MiB | 225.3 MiB | **94.3 MiB saved** | 29.5% |
| **Nginx + Go Exporter** | **1 Pod** | **7 PIDs** | 147.2 MiB | 76.1 MiB | **71.1 MiB saved** | 48.3% |
| **Nginx + Go Exporter** | **2 Pods** | **14 PIDs** | 294.4 MiB | 152.5 MiB | **141.9 MiB saved** | 48.2% |
| **Nginx + Go Exporter** | **4 Pods** | **28 PIDs** | 588.7 MiB | 306.5 MiB | **282.2 MiB saved** | 47.9% |
| **Redis Cache Node** | **1 Pod** | **2 PIDs** | 231.4 MiB | 179.4 MiB | **52.0 MiB saved** | 22.5% |
| **Redis Cache Node** | **2 Pods** | **4 PIDs** | 456.7 MiB | 359.2 MiB | **97.5 MiB saved** | 21.3% |
| **Redis Cache Node** | **4 Pods** | **8 PIDs** | 907.4 MiB | 718.4 MiB | **189.0 MiB saved** | 20.8% |

---

## Installation & Deployment

Supports Linux x86-64 and aarch64 with cgroup v1/v2 across containerd, CRI-O, and Docker hosts. Requires `sudo` (root privileges) on worker nodes or execution within a privileged forensic DaemonSet.

### Offline Build & Installation
Uses Rust edition 2024 with 100% vendored crates (zero internet access required at build time):

```bash
# Verify offline builds and run unit test suite (55/55 tests passing)
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

## Output Layout & Exit Codes

Output directories are created with `0700` permissions; Core and Manifest files are written with `0600` permissions:

```text
capture/
  ├── manifest.json              # Full Pod metadata, process identities, and coverage ledger
  ├── <host_pid>_<starttime>.core # Standalone ELF Core images per captured process
  └── ...
```

### Exit Codes
- **`0` (Complete)**: All target processes successfully captured and synchronized to disk (`fsync` durability verified).
- **`2` (Partial)**: Process memory captured, but explicit gaps (e.g., unreadable `[vvar]` kernel structures or early process termination) were recorded in the audit ledger. Output retains `.core` or `.core.partial` files.
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
       ├──► Memory Buffer    : Max 512 MiB in-memory snapshot buffer
       └──► I/O Backlog      : Max 1 GiB pending write queue
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

</details>
