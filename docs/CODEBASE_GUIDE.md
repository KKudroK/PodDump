# PodDump 코드와 수집 원리

PodDump는 Pod UID로 프로세스를 찾고, 각 프로세스의 메모리와 CPU 상태를 ELF Core에 저장합니다. 사용 명령과 결과 확인 방법은 [README](../README.md)에 있습니다.

## 수집 순서

1. `main`은 `--pod`와 `--output` 두 필수 인자와 `-h`/`--help`, `-V`/`--version`만 받습니다. 작업자 수, 대기 시간(60,000ms), 버퍼 정책은 내부 기본값을 사용합니다.
2. `pod_lock`이 같은 Pod의 동시 수집을 제한합니다. 다른 요청은 잠금이 풀릴 때까지 기다립니다. 이 Pod 잠금은 엄격한 FIFO 순서를 보장하지 않습니다.
3. `identity`가 cgroup에서 대상 PID, 시작 시각, 컨테이너 ID를 확인합니다. PID는 재사용될 수 있으므로 번호만으로 대상을 정하지 않습니다. CRI-O의 conmon 등 확인된 런타임 보조 프로세스는 분리합니다.
4. `safe_output`이 소유자 전용 디렉터리를 만들고, `main`이 manifest 저장 공간을 먼저 확보합니다.
5. `ptrace`로 프로세스의 스레드를 멈춥니다. 이후 `/proc/PID/mem`, `pagemap`, `maps`, `smaps`와 CPU 상태를 읽습니다. 정지 전에 열어 둔 오래된 주소 공간을 잘못 읽지 않도록 메모리 파일은 정지 후 엽니다.
6. 메모리가 버퍼 한도 안에 들어오면 RAM으로 읽은 뒤 프로세스를 다시 실행하고 파일에 씁니다. 큰 대상은 정지 상태에서 파일로 바로 쓰는 streaming 경로를 사용합니다. 정지 시간 한도에 닿으면 확보한 범위와 빠진 범위를 구분합니다.
7. 파일 동기화, SHA-256 계산, 파일 게시를 진행하고 `manifest.json`을 남깁니다. 파일 게시에는 `RENAME_NOREPLACE`를 사용하므로 기존 결과를 덮어쓰지 않습니다.

노드 공유 자원 대기는 `shared_queue`·`shared_budget`이 별도로 관리합니다. 같은 Pod의 잠금과 노드의 동시 작업 수 제한은 서로 다른 기능입니다.

## 메모리를 0으로 처리하는 조건

`pagemap`은 가상 페이지의 현재 상태를 알려 주는 Linux 파일입니다. `pagemap_eligible`은 파일과 연결되지 않은 private 익명 메모리만 허용합니다. `smaps`의 RSS와 `VmFlags`도 확인해야 합니다.

- `io`, `pf`, `ht`, `mm`, `sh`, `ms`, `um`, `uw`, `ui`가 있거나 `VmFlags`를 읽지 못했으면 `pagemap` 읽기 생략을 하지 않습니다.
- present(63), swapped(62), file/shared(61), guard(58), userfaultfd(57) 비트가 모두 0일 때만 미할당 0 페이지로 처리합니다. guard와 userfaultfd의 뜻은 [Linux pagemap 문서](https://www.kernel.org/doc/html/latest/admin-guide/mm/pagemap.html)를 따릅니다.
- `pagemap`을 열거나 읽지 못하면 `/proc/PID/mem`에서 실제 읽기를 시도합니다. 실패한 범위를 0으로 꾸미지 않습니다.
- `MADV_DONTDUMP`의 `dd`만으로 수집에서 제외하지 않습니다. `PROT_NONE` 제외는 공유 매핑이 아닌 일반 익명 메모리에서 RSS·Swap이 모두 0이고 특수 매핑이 아닐 때만 적용합니다. 장치 I/O·PFN 매핑과 읽을 수 없는 커널 특수 구간의 제외는 별도 이유로 기록합니다.

실제로 읽은 0 구간도 Sparse 파일로 저장합니다. AVX2를 지원하는 CPU에서는 이를 사용하고, 지원하지 않으면 일반 비교 코드로 처리합니다.

## ELF 헤더와 무결성

`PT_LOAD`는 Core에서 읽을 수 있는 메모리 범위와 파일 위치를 나타냅니다. 읽기 실패가 반복되면 한 매핑이 여러 `PT_LOAD`로 나뉩니다. 따라서 헤더는 매핑 개수뿐 아니라 읽기 성공·실패 페이지가 번갈아 나타나는 경우도 포함해 예약합니다. 전체 상한은 `MAX_PHDRS = 524_288`입니다. 예약만 하고 쓰지 않은 부분은 Sparse로 유지하며, 버퍼 경로에서도 미접촉 익명 `mmap` 헤더 빈 구간을 `known_zero_ranges`에 등록해 불필요한 페이지 폴트와 0-스캔 없이 건너뜁니다.

버퍼 경로는 쓰는 바이트와 0 구간을 순서대로 SHA-256에 넣는 `inline_write`를 사용합니다. streaming 경로는 동기화된 파일을 다시 읽는 `post_sync_fd_read`를 사용합니다. 이 이름과 실제 디스크 SHA-256 일치는 별도로 검사합니다. `pagemap` 생략 여부와 무관하게 해시 계산 경로는 동일하게 동작합니다.

## 잠금과 쓰기 속도

잠금 파일을 삭제하면 이미 그 파일을 연 요청과 새 파일을 연 요청이 서로 다른 잠금을 잡을 수 있습니다. `pod_lock`은 오래된 파일도 자동 삭제하지 않습니다. Linux는 프로세스 종료 시 OFD 잠금을 해제합니다. 파일의 이름·inode·소유자·권한·링크 개수도 검사합니다.

`rate_limit::RateLimiter`는 실제로 쓴 바이트에 따라 필요한 시간이 지날 때까지 반복해서 기다립니다. 대기는 20ms씩 나눠 취소를 확인합니다. 버퍼·streaming 양쪽에 적용되며 Sparse로 생략한 바이트는 속도 계산에 넣지 않습니다. 프로세스별 평균 제한이고, 노드 전체의 순간 I/O를 제한하는 기능은 아닙니다.

`posix_fallocate`는 manifest 공간을 확보합니다. `RLIMIT_NOFILE`은 열 수 있는 파일 수를 확인해 보류 작업 수를 정합니다. 페이지 캐시 해제 요청은 운영체제에 보내는 힌트이며 메모리 사용량의 보장은 아닙니다.

## 파일별 역할

| 파일 | 담당 |
|---|---|
| `src/main.rs` | 인자 검사, 작업 조율, 최종 저장과 종료 코드 |
| `src/identity.rs` | Pod·컨테이너·프로세스 식별 |
| `src/pod_lock.rs` | 같은 Pod의 수집 잠금과 중복 요청 안내 |
| `src/shared_budget.rs`, `src/shared_queue.rs`, `src/pipeline.rs` | 노드 공유 예산, 대기, 프로그램 내부 작업 수 |
| `src/corex/procfs.rs` | maps·smaps·프로세스 정보와 제외 조건 |
| `src/corex/ptrace.rs` | 스레드 정지, CPU 상태 읽기, 정지 해제 |
| `src/corex/elf.rs` | ELF 배치, 읽기 성공·실패 범위, pagemap, Sparse 쓰기 |
| `src/corex/notes.rs` | CPU·스레드·파일 매핑 정보를 ELF note로 기록 |
| `src/corex/mod.rs` | 수집 경로 선택, 버퍼 쓰기, SHA-256 |
| `src/rate_limit.rs` | 쓰기 평균 속도와 취소 대기 |
| `src/safe_output.rs` | 경로·권한 검사, 기존 파일을 바꾸지 않는 게시 |
| `src/cli_output.rs` | 결과 표시와 터미널 제어 문자 처리 |
| `build.rs`, `src/build_info.rs` | 소스·의존성·실행 파일 해시 |

## 검증 범위

배포 전 `cargo fmt --check`, clippy와 단위 test, vendor 해시 검사, musl 정적 빌드를 실행합니다. 회귀 검사는 같은 잠금 inode 유지, 0.25초보다 긴 속도 제한, streaming 속도 제한, 특수 메모리 제외 방지, guard 표시, 여러 읽기 실패 뒤의 ELF 헤더 보존을 포함합니다. `corex` 내부의 `allow_pagemap_skip` 토글 단위 검사로 실제 익명 메모리 생략 전후 바이트와 Sparse 파일 해시도 확인합니다.

[이전 후보판 측정](history_codebase_candidate.md)의 시간·크기·81개 테스트 결과는 해당 후보판의 기록입니다. 헤더 예약과 속도 제한을 수정한 배포본의 성능 수치로 그대로 사용하지 않습니다. 최신 배포 검사 결과는 [릴리스 검증](release_review.md)에 기록합니다.
