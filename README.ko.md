# PodDump: Kubernetes-Native Memory Acquisition Engine

<div align="center">

[English](README.md) &nbsp;|&nbsp; [한국어](README.ko.md)

</div>

---

**PodDump**는 쿠버네티스 파드 단위 프로세스 메모리 수집 도구입니다.

호스트 노드에서 **단일 Pod UID**를 입력받아 파드 내 모든 컨테이너(애플리케이션, 사이드카, 인프라 샌드박스)의 프로세스를 식별하고, ptrace 제어와 비동기 스트리밍 파이프라인을 통해 프로세스 메모리를 표준 ELF Core 파일로 수집합니다.

---

## 주요 특징

단일 프로세스 단위 덤프 스크립트 대비 대상 프로세스 정지 시간을 줄이고, 파드 내 다중 프로세스를 일괄 수집하도록 설계되었습니다.

### 1. 다중 프로세스 확장성
- **수집 시간 단축:** 비동기 스트리밍 파이프라인을 통해 파드 내 6~28개 프로세스를 0.22초 ~ 0.81초 만에 수집합니다.
- **CPU 오버헤드 절감:** 중복된 procfs 파싱 및 프로세스 생성 비용을 줄여 GDB 대비 CPU 소모를 절감합니다.

### 2. 가상 메모리 예약 최적화
- **미할당 익명 예약 건너뛰기:** Prometheus Exporter, Fluentbit 등 클라우드 네이티브 사이드카는 Go나 JVM 런타임의 주소 공간 예약으로 인해 1.2 GiB 이상의 미커밋 익명 영역을 갖습니다.
- PodDump는 `/proc/{pid}/smaps`를 확인하여 실제 물리 메모리가 할당되지 않은(`Rss: 0 kB` 및 `Swap: 0 kB`) 영역을 선별적으로 건너뛰어 수집 시간과 정지 시간을 단축합니다.
- Linux 5.3 이상 커널의 `pidfd_open` 및 Linux 4.19 이상 커널의 `kill(0)` 상태 검증 경로를 지원합니다.

### 3. 짧은 프로세스 정지 시간
- **정지 시간 단축:** 대상 프로세스의 메모리를 인메모리 버퍼로 빠르게 복사한 뒤 즉시 재개하고, 디스크 저장은 비동기로 처리하여 서비스 요청 지연을 줄입니다.

### 4. 공유 예산 프로토콜 (`credits-v3`) 기반 노드 안전성
- 여러 파드를 동시에 수집할 때 노드 공유 파일 락을 통해 동시 정지 프로세스 수(최대 4개), 메모리 버퍼, I/O 부하를 제어하여 노드 과부하 및 OOM 발생 위험을 완화합니다.

### 5. 포렌식 무결성 및 희소 파일 최적화
- 읽기 실패한 커널 특수 영역에 임의의 0 채움을 하지 않고, 실제 성공적으로 읽은 바이트만 ELF `PT_LOAD`에 매핑합니다. 미수집 구간은 `manifest.json`에 주소 범위와 errno를 기록합니다.
- 제로 블록 감지를 통해 희소 파일(sparse file) 형태로 기록하여 디스크 공간을 절약합니다.

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

단일 노드 쿠버네티스 환경(Ubuntu 24.04.5 LTS, Linux 6.8, 4 vCPU, 9 GiB RAM)에서 12개 파드, 60개 활성 프로세스를 대상으로 총 9개 조건(누적 105개 코어 덤프 생성)을 실측한 결과입니다 (상세 데이터는 [성능 및 운영 검증 기록](docs/benchmarks.md) 참조).

### 1. 수집 처리 시간 및 속도 비교

> [!NOTE]
> **비교 도구 측정 조건 안내**  
> * **ProcDump**: Linux용 ProcDump v3.5.3에서 커널 특수 영역 에러로 인한 조기 종료를 방지하고 가상 메모리 전체를 누락 없이 덤프하도록 `-usegcore` 확장 백엔드 옵션을 적용한 구성입니다.
> * **GDB**: GNU Debugger의 `gcore` 호출 시 수 기가바이트에 달하는 디버그 심볼(DWARF) 로딩 지연을 배제하기 위해 `--readnever` 플래그를 적용한 순수 메모리 덤프 최적화 구성입니다.

| 대상 워크로드 | 파드 수 | 총 프로세스 수 (파드당) | 메모리 프로파일 | PodDump 소요 시간 | ProcDump | GDB | 수집 속도 향상 | CPU 시간 절감 |
|:---|:---:|:---:|:---|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **총 6개** (파드당 6개) | ~15 MiB RSS / PID | **0.22초** | 0.52초 | 0.55초 | **2.4x ~ 2.5x 빠름** | 58% ~ 70% 절감 |
| **Nginx Web Server** | **2 Pods** | **총 12개** (파드당 6개) | ~15 MiB RSS / PID | **0.38초** | 0.91초 | 1.02초 | **2.4x ~ 2.7x 빠름** | 63% ~ 68% 절감 |
| **Nginx Web Server** | **4 Pods** | **총 24개** (파드당 6개) | ~15 MiB RSS / PID | **0.67초** | 1.74초 | 1.94초 | **2.6x ~ 2.9x 빠름** | 60% ~ 64% 절감 |
| **Nginx + Go Exporter** | **1 Pod** | **총 7개** (파드당 7개) | 1.2 GiB VMA (Go 예약) | **0.33초** | 0.52초 | 0.89초 | **1.6x ~ 2.7x 빠름** | 35% ~ 66% 절감 |
| **Nginx + Go Exporter** | **2 Pods** | **총 14개** (파드당 7개) | 1.2 GiB VMA (Go 예약) | **0.40초** | 0.93초 | 0.95초 | **2.3x ~ 2.4x 빠름** | 37% ~ 38% 절감 |
| **Nginx + Go Exporter** | **4 Pods** | **총 28개** (파드당 7개) | 1.2 GiB VMA (Go heap) | **0.81초** | 1.79초 | 1.95초 | **2.2x ~ 2.4x 빠름** | 37% ~ 38% 절감 |
| **Redis Cache Node** | **1 Pod** | **총 2개** (파드당 2개) | ~128 MiB RSS / PID | **0.73초** | 0.62초 | 0.64초 | 동등 (비동기 내구성) | - |
| **Redis Cache Node** | **2 Pods** | **총 4개** (파드당 2개) | ~128 MiB RSS / PID | **0.99초** | 0.65초 | 0.87초 | 동등 (노드 예산 보호) | - |
| **Redis Cache Node** | **4 Pods** | **총 8개** (파드당 2개) | ~128 MiB RSS / PID | **2.34초** | 1.16초 | 1.28초 | 안전 2파동 직렬화 | - |

---

### 2. 서비스 정지 지연시간 및 가용성 보장

| 대상 워크로드 | 파드 수 | 총 프로세스 수 (파드당) | PodDump 최대 정지 시간 | ProcDump 최대 정지 시간 | GDB 최대 정지 시간 | 정지 시간 단축률 | 실시간 트래픽 P99 응답 지연 | PodDump 서비스 실패 | ProcDump 서비스 실패 |
|:---|:---:|:---:|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **총 6개** (파드당 6개) | **15.5 ms** | 77.2 ms | 78.9 ms | **5.0x ~ 5.1x 단축** | 7.0 ms | **0건 (100% 정상)** | 0건 |
| **Nginx Web Server** | **2 Pods** | **총 12개** (파드당 6개) | **17.9 ms** | 96.4 ms | 85.4 ms | **4.8x ~ 5.4x 단축** | 14.9 ms | **0건 (100% 정상)** | 0건 |
| **Nginx Web Server** | **4 Pods** | **총 24개** (파드당 6개) | **28.1 ms** | 94.2 ms | 88.1 ms | **3.1x ~ 3.4x 단축** | 35.9 ms | **0건 (100% 정상)** | 0건 |
| **Nginx + Go Exporter** | **1 Pod** | **총 7개** (파드당 7개) | **90.7 ms** | 142.8 ms | 250.9 ms | **1.6x ~ 2.8x 단축** | 8.3 ms | **0건 (100% 정상)** | 0건 |
| **Nginx + Go Exporter** | **2 Pods** | **총 14개** (파드당 7개) | **50.3 ms** | 153.8 ms | 116.6 ms | **2.3x ~ 3.1x 단축** | 21.6 ms | **0건 (100% 정상)** | 0건 |
| **Nginx + Go Exporter** | **4 Pods** | **총 28개** (파드당 7개) | **81.8 ms** | 153.6 ms | 245.5 ms | **1.9x ~ 3.0x 단축** | 32.7 ms | **0건 (100% 정상)** | 0건 |
| **Redis Cache Node** | **1 Pod** | **총 2개** (파드당 2개) | **274.3 ms** | 331.6 ms | 115.4 ms | ProcDump 대비 1.2x 단축 | 251.3 ms | **0건 (100% 정상)** | 0건 |
| **Redis Cache Node** | **2 Pods** | **총 4개** (파드당 2개) | **280.9 ms** | 418.8 ms | 110.1 ms | ProcDump 대비 1.5x 단축 | 262.6 ms | **0건 (100% 정상)** | 0건 |
| **Redis Cache Node** | **4 Pods** | **총 8개** (파드당 2개) | 1175.0 ms | **531.0 ms** | 136.8 ms | 4 vCPU 부하 환경 병렬 수집 | 413.8 ms | 34건 (CPU 포화) | **3건 타임아웃 유실** |

---

### 3. 디스크 스토리지 및 희소 파일 절감 효과

| 대상 워크로드 | 파드 수 | 총 프로세스 수 (파드당) | 논리 코어 크기 | 실제 디스크 할당량 | 절감된 블록 크기 | 디스크 절감율 |
|:---|:---:|:---:|:---|:---:|:---:|:---:|
| **Nginx Web Server** | **1 Pod** | **총 6개** (파드당 6개) | 79.8 MiB | 56.2 MiB | **23.6 MiB 절감** | 29.5% |
| **Nginx Web Server** | **2 Pods** | **총 12개** (파드당 6개) | 159.5 MiB | 112.4 MiB | **47.1 MiB 절감** | 29.5% |
| **Nginx Web Server** | **4 Pods** | **총 24개** (파드당 6개) | 319.1 MiB | 224.9 MiB | **94.2 MiB 절감** | 29.5% |
| **Nginx + Go Exporter** | **1 Pod** | **총 7개** (파드당 7개) | 147.0 MiB | 76.0 MiB | **71.0 MiB 절감** | 48.3% |
| **Nginx + Go Exporter** | **2 Pods** | **총 14개** (파드당 7개) | 294.1 MiB | 152.2 MiB | **141.9 MiB 절감** | 48.2% |
| **Nginx + Go Exporter** | **4 Pods** | **총 28개** (파드당 7개) | 588.2 MiB | 305.6 MiB | **282.6 MiB 절감** | 48.0% |
| **Redis Cache Node** | **1 Pod** | **총 2개** (파드당 2개) | 231.3 MiB | 179.4 MiB | **51.9 MiB 절감** | 22.4% |
| **Redis Cache Node** | **2 Pods** | **총 4개** (파드당 2개) | 456.6 MiB | 359.1 MiB | **97.5 MiB 절감** | 21.4% |
| **Redis Cache Node** | **4 Pods** | **총 8개** (파드당 2개) | 907.3 MiB | 718.2 MiB | **189.1 MiB 절감** | 20.8% |

---

## 실행 환경과 설치

Linux x86-64 또는 aarch64, cgroup v1/v2 및 containerd/CRI-O/Docker 기반 호스트에서 사용합니다. 대상 프로세스에 대한 ptrace 권한과 호스트 `/proc` 접근을 위해 `sudo` 권한이 필요합니다. 대상 Pod가 실행 중인 워커 노드에서 직접 호출하거나 권한 있는 포렌식 DaemonSet을 통해 실행합니다.

### 빌드 및 설치
Rust edition 2024를 사용하며 vendored 의존성을 포함하고 있어 오프라인 환경에서도 즉시 빌드할 수 있습니다.

```bash
# 저장소 루트에서 빌드 및 단위 테스트 수행 (56개 전수 통과)
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
| `--pod` | 대상 파드의 전체 UID. 이름이나 축약형 접두사는 허용되지 않습니다. |
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

## 실제 실행 화면 및 산출물 검증

실제 단일 Pod(`nginx-01`, 2개 컨테이너, 6개 프로세스)에 대해 PodDump를 실행했을 때 터미널에 출력되는 대화형 트리 및 생성된 산출물 결과입니다:

### 1. 터미널 실행 및 대화형 프로세스 트리 출력
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
> **커널 특수 매핑(`[vvar]`, `[vsyscall]`) 자동 선별 제외 안내**  
> 최신 리눅스 커널의 `[vvar]`(약 8 KiB) 및 `[vsyscall]`은 커널이 관리하는 특수 영역으로 `/proc/{pid}/mem`을 통한 직접 읽기가 커널 레벨에서 차단(`EFAULT`)됩니다.  
> PodDump는 유저 애플리케이션의 메모리가 아닌 이들 영역을 수집 대상에서 안전하게 선별 제외(`skipped_kernel_special`)하고 감사 원장에 투명하게 기록합니다.  
> 그 결과 실제 애플리케이션 메모리(코드, 힙, 스택, 공유 라이브러리)가 온전히 수집되면 깔끔하게 **`[OK]` / `COMPLETE` (종료 코드 0)**로 완료되며, 실제 메모리 누락이나 프로세스 강제 종료가 발생한 경우에만 **`[PARTIAL]` (종료 코드 2)**로 명확히 구분됩니다.

### 2. 저장 디렉터리 및 보안 권한 확인 (`ls -lh`)
모든 Core 덤프 파일과 `manifest.json` 원장은 타 사용자의 접근을 차단하기 위해 엄격한 `0600`(`-rw-------`) 보안 권한으로 생성됩니다:

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

### 3. 메타데이터 감사 원장 (`manifest.json` 발췌)
커널 특수 매핑은 날조된 0바이트 대신 감사 원장에 `skipped_kernel_special` 사유로 정확히 기록되며, 유저 메모리는 100% 정상 수집(`complete: true`)되었음을 입증합니다:

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

## 산출물 구조 및 종료 코드

출력 디렉터리는 `0700`, 생성된 Core 및 Manifest 파일은 `0600` 보안 권한으로 엄격히 보호됩니다.

```text
capture/
  ├── manifest.json              # 파드 메타데이터, 프로세스 신원, 세부 수집 커버리지 감사 원장
  ├── <host_pid>_<starttime>.core # 수집 완료된 프로세스별 ELF Core 이미지
  └── ...
```

### 종료 코드 규격
- **0 (정상 완료)**: 대상 파드의 모든 프로세스 메모리가 성공적으로 획득 및 디스크 동기화(`fsync`)되었습니다. 정상 파드 수집 시 종료 코드 0으로 완료되며 온전한 `.core` 파일이 생성됩니다.
- **2 (부분 완료)**: 대상 프로세스가 수집 중 조기 종료되었거나 비정상 메모리 읽기 오류가 발생하여 감사 원장에 기록되었습니다. (수집된 파일은 `.core` 또는 `.core.partial`로 보존)
- **1 (실행 실패)**: 권한 부족, 유효하지 않은 Pod UID, 출력 경로 충돌 등으로 수집이 시작되지 못하고 중단됨.

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
       ├──► Memory Buffer    : 512 MiB 목표 스냅샷 메모리 풀
       └──► I/O Backlog      : 소프트 예약 기준 1 GiB 디스크 쓰기 큐
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
