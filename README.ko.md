# PodDump

**버전 0.5.0**은 운영 환경의 노드 부하와 프로세스 정지 시간을 줄이면서 포렌식 커버리지와 SHA-256 무결성을 유지하도록 개선했습니다. 코드 구조·기능·검증 근거는 [코드 · 기능 기준 문서](docs/CODEBASE_GUIDE.md) 한 문서에서 관리합니다. 아래의 v0.3.1 출력 예시, 배포판 표, 2026-10-16 Nginx 수치는 이전 빌드의 기록입니다.

[English](README.md) · [코드 · 기능 기준](docs/CODEBASE_GUIDE.md) · [배포 조건](docs/deployment.md) · [측정 상세](docs/benchmark_summary.md)

Pod UID 하나로 해당 파드의 프로세스 메모리를 수집합니다. 노드에서 컨테이너·샌드박스 프로세스를 찾아 ELF 코어를 만들고, 실행 식별 정보·제외 및 실패 범위·수집 상태를 `manifest.json`에 남깁니다.

## v0.5.0 주요 동작

- **2단계 0-페이지 고속 처리**: `smaps` `VmFlags`로 안전 조건(`io`/`pf`/`ht`/`mm`/`sh`/`ui`/`uw`/`up` 미포함)을 확인한 일반 익명 구간에서 `pagemap`으로 미할당 0-페이지를 사전 판별해 `/proc/<pid>/mem` 읽기 호출 자체를 건너뛰고, 읽은 페이지도 AVX2 SIMD 검사 후 `lseek` 희소 기록으로 디스크 쓰기를 줄입니다.
- **포렌식 커버리지 보호**: `MADV_DONTDUMP`(`dd`) 구역은 계속 `PT_LOAD`에 유지해 회피 시도를 막고, 커밋된 RAM·스왑이 모두 `0`인 `PROT_NONE` 가드만 제외해 제외 바이트를 기록합니다.
- **동적 ELF 헤더와 단일 패스 해시**: 필요한 프로그램 헤더 수에 맞춘 동적 헤더 예약(`MIN_PHDR_SLACK = 64`~`MAX_PHDR_SLACK = 1024`)으로 소형 컨테이너 덤프 크기를 줄였고, 버퍼 경로에서는 쓰기 중 인라인 SHA-256(`inline_write`, 스트리밍·미생략 경로는 `post_sync_fd_read`)을 계산합니다.
- **운영 자원 보호**: `--max-write-mbps`(1..10000)로 프로세스당 디스크 쓰기 속도를 제한하고, `POSIX_FADV_DONTNEED`로 커널 페이지 캐시 점유를 비우며, `posix_fallocate`로 `manifest.json` 공간을 먼저 확보합니다. `pod_lock`은 부트 ID 변경 또는 3600초 경과 만료 잠금을 안전하게 정리합니다.

## 실행

Linux x86-64 정적 바이너리 `poddump` 하나로 실행합니다. GDB, glibc 설치, 컨테이너 런타임 CLI나 별도 설정 파일이 필요하지 않습니다.

```sh
chmod 755 poddump
sudo ./poddump --pod FULL_POD_UID --output /evidence/new_collection
# 운영 디스크 대역폭 제한 예시
sudo ./poddump --pod FULL_POD_UID --output /evidence/new_collection --max-write-mbps 200
```

기본(프로덕션) 빌드 옵션은 다음과 같습니다.
- `--pod FULL_POD_UID` (`--pod-uid` 별칭 지원): 대상 노드의 전체 Pod UID
- `--output NEW_DIR`: 새로 만들 수집 출력 디렉터리 (부모 디렉터리는 미리 생성)
- `--request-id ID`: 동일 활성 요청 중복 실행 방지 식별자
- `--wait-timeout-ms 0..3600000`: 동일 Pod 선행 작업 대기 시간 (기본 `60000`, `0`이면 즉시 거부)
- `--max-write-mbps 1..10000`: 프로세스당 디스크 쓰기 대역폭 상한
- `--format tree|json`, `--full-ids`: 터미널 출력 형식 및 전체 컨테이너 ID 표시

대상 노드의 호스트 proc/cgroup과 `ptrace` 권한이 필요합니다. 출력 저장소는 소유자 전용 사적 권한(`0o700`)과 내구 저장을 지원해야 합니다. [지원 환경과 한계](docs/deployment.md).

이전 빌드(v0.3.1, Ubuntu 24.04.5)에서 프로세스 2개인 시험 Pod를 수집한 출력 예시입니다(출력 경로 줄 생략).

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

[이전 실행 stdout 전문](docs/examples/capture.stdout).

## 검증한 환경 (이전 릴리즈 검증 기록)

아래 환경 표는 이전 릴리즈 x86-64 정적 바이너리 기준 기록이며, v0.5.0 개선은 WSL2(`6.18.33.2`) A/B 실측과 `rrudrok` 호환 시험 및 오프라인 테스트(`81 passed`)로 검증했습니다. 상세 수치는 [코드 · 기능 기준 문서](docs/CODEBASE_GUIDE.md)를 참고하세요.

| 환경 | 커널 | 결과 |
|---|---|---|
| Ubuntu 24.04.5, Kubernetes/containerd | 6.8.0-147 | ext4에서 실제 Pod 수집 성공 |
| Ubuntu 26.04, WSL2 | 6.18.33.2 | ext4에서 합성 Pod 수집 성공 |
| Debian 12 | 6.1.0-53-cloud-amd64 | 수집·패턴 검증 성공; ext4, cgroup v2 |
| Debian 13 | 6.12.111+deb13-cloud-amd64 | 수집·패턴 검증 성공; ext4, cgroup v2 |
| Rocky Linux 8.10 | 4.18.0-553.el8_10.x86_64 | 수집·패턴 검증 성공; xfs, cgroup v1 |
| Rocky Linux 9.8 | 5.14.0-687.10.1.el9_8.0.1.x86_64 | 수집·패턴 검증 성공; xfs, cgroup v2 |
| Kali Rolling (cloud 2026.2) | 6.18.12+kali-cloud-amd64 | 수집·패턴 검증 성공; ext4, cgroup v2 |
| Alpine 3.16.9, cgroup v1 | 5.15.148 | 합성 Pod 수집 성공 |
| Alpine 3.22.6, cgroup v2 | 6.12.110 | 합성 Pod 수집 성공 |

Ubuntu Kubernetes 외의 Pod 시험은 합성 cgroup을 사용했습니다. Debian·Rocky·Kali에서는 containerd/CRI-O/Docker 경로 형식별 수집을 확인했으며, 세 런타임 모두의 실제 설치·통합 검증을 뜻하지는 않습니다. [검증 조건](docs/distro_validation.md).

Kali cloud 이미지의 배포명은 2026.2이며, 내부 `VERSION_ID`는 2026.1입니다.

## 이전 측정 결과 (2026-10-16 비교 기록)

2026-10-16 Nginx 비교 결과입니다. 조건당 1회이며 PodDump v7, ProcDump 3.5.3의 gcore 백엔드, GDB `--readnever`를 사용했습니다. 표는 저장된 결과의 wall time을 사용합니다.

| 파드 | 프로세스 | PodDump | ProcDump | GDB |
|---:|---:|---:|---:|---:|
| 1 | 6 | **0.228 s** | 0.520 s | 0.550 s |
| 2 | 12 | **0.387 s** | 0.913 s | 1.022 s |
| 4 | 24 | **0.691 s** | 1.743 s | 1.944 s |

이 다중 프로세스 Nginx 조건에서는 수집 시간이 짧았습니다. 모든 부하에서 빠른 것은 아니며 Redis 조건에서는 더 느렸습니다. 도구별 수집 정책과 출력 크기가 달라 동일 바이트 수집을 비교한 표는 아닙니다. [결과와 원본 출처](docs/benchmark_summary.md).

## 요청과 결과

- 같은 Pod의 다른 요청은 대상 조회 전에 대기합니다. `--wait-timeout-ms` 기본값은 60000이며 0이면 대기 없이 거부합니다.
- 진행 중인 `--request-id`가 같으면 기존 작업 위치와 종료 코드 **3**을 반환합니다. 완료 뒤 다시 호출하면 새로 수집합니다.
- 종료 **0**: 기록된 정책에 따른 수집·저장 완료. **2**: 부분 수집. **1**: 입력·준비·저장 오류. stderr와 미완성 파일을 보존하세요.
- 프로세스별로 정지하므로 파드 전체의 원자적 스냅샷은 아닙니다. SIGKILL 뒤에는 최종 manifest 없이 일부 파일만 남을 수 있습니다.

동시 실행은 같은 실행 UID와 공유 런타임 디렉터리를 사용해야 합니다. 업그레이드 전에는 이전 수집기를 종료합니다. [중복 요청과 복구](docs/deployment.md#duplicate-requests-and-recapture).

## 빌드와 확인

```sh
./poddump --version
./poddump --build-info
cargo test --offline --locked
python3 scripts/build_release.py --check-vendor
python3 scripts/build_release.py
```

버전 **0.5.0**. 빌드 스크립트가 시험 후 `release/poddump`와 해시 증적을 만듭니다. 해시 파일은 실행에 필요하지 않습니다. [코드 · 기능 기준](docs/CODEBASE_GUIDE.md) · [개발 옵션](docs/development_options.md) · [이전 0.4.0 변경](docs/release_v0_4_0.md) · [릴리즈 검증](docs/release_validation.md) · [이전 문서](docs/history_readme_20261017_ko.md).

MIT 라이선스입니다. Core 파일 코드는 Microsoft ProcDump for Linux에 기반합니다. [NOTICE.txt](NOTICE.txt).
