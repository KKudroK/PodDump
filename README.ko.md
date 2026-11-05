# PodDump

2026-11-05 소스 수정으로 수집 누락과 소속 판정 문제를 보완했습니다. [수정 내용과 검증 범위](docs/capture_review.md)를 참고하세요. 아래의 기존 릴리즈 수집 결과와 성능 수치는 이전 빌드의 기록입니다.

[English](README.md) · [배포 조건](docs/deployment.md) · [측정 상세](docs/benchmark_summary.md)

Pod UID 하나로 해당 파드의 프로세스 메모리를 수집합니다. 노드에서 컨테이너·샌드박스 프로세스를 찾아 ELF core를 만들고, 실행 식별 정보·읽지 못한 범위·수집 상태를 `manifest.json`에 남깁니다.

## 실행

Linux x86-64 정적 바이너리 `poddump` 하나로 실행합니다. GDB, glibc 설치, 컨테이너 런타임 CLI나 별도 설정 파일이 필요하지 않습니다.

```sh
chmod 755 poddump
sudo ./poddump --pod FULL_POD_UID --output /evidence/new_collection
```

출력 부모 디렉터리는 미리 만들고, 수집 디렉터리는 새 경로를 지정합니다. 대상 노드의 호스트 proc/cgroup과 ptrace 권한이 필요합니다. 출력 저장소는 사적 권한과 내구 저장을 지원해야 합니다. [지원 환경과 한계](docs/deployment.md).

Ubuntu 24.04.5에서 v0.3.1로 프로세스 2개인 시험 Pod를 수집한 실제 출력입니다. 출력 경로를 표시하는 뒷부분만 생략했습니다.

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

[실제 stdout 전문](docs/examples/capture.stdout).

## 검증한 환경

아래 환경에 동일한 x86-64 릴리즈 바이너리를 사용했습니다.

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

## 측정 결과

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
# 빌드 호스트에만 Rust 1.90.0 + x86_64-unknown-linux-musl 타깃 필요
python3 scripts/build_release.py
```

버전 **0.3.1**. 빌드 스크립트가 시험 후 `release/poddump`와 해시 증적을 만듭니다. 해시 파일은 실행에 필요하지 않습니다. [릴리즈 검증](docs/release_validation.md) · [이전 문서](docs/history_readme_20261017_ko.md).

MIT 라이선스입니다. Core 파일 코드는 Microsoft ProcDump for Linux에 기반합니다. [NOTICE.txt](NOTICE.txt).
