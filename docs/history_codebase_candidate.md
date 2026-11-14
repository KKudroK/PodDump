# PodDump 코드 · 기능 · 검증 근거 기준 문서

**핵심 결론:** PodDump(버전 **0.5.0**)는 쿠버네티스 노드에서 Pod UID 하나로 컨테이너와 샌드박스 프로세스의 ELF 코어(ELF Core: 프로세스 메모리와 CPU 레지스터 상태를 담은 표준 덤프 파일)를 수집하는 단일 정적 실행 파일입니다. 리눅스 파일 잠금(OFD 잠금)으로 여러 요청을 안전하게 조율하고, `pagemap` 사전 판별과 CPU 벡터 연산(AVX2 SIMD)을 함께 써서 프로세스를 멈추는 시간과 디스크 쓰기를 크게 줄이면서도 전체 바이트 SHA-256 해시와 포렌식 증거 범위를 그대로 지킵니다.

---

## 1. 핵심 용어 쉽게 이해하기

- **정적 바이너리(static binary)**: 실행할 때 외부 라이브러리(`.so`)나 GDB, 컨테이너 런타임 CLI에 의존하지 않고 파일 하나로 바로 실행되는 프로그램입니다.
- **ELF 코어(ELF Core)**: 리눅스에서 실행 중인 프로그램의 메모리 구간(`PT_LOAD` 헤더로 위치를 기록)과 스레드 레지스터·파일 매핑 정보(`NT_*` 노트 섹션)를 한 파일에 담은 형식입니다.
- **희소 파일(sparse file)**: 파일 안에 0바이트만 연속으로 이어진 구간을 디스크에 실제로 쓰지 않고, 파일 크기와 0 위치 정보만 파일시스템에 남기는 저장 방식입니다. 나중에 파일 전체를 읽으면 0으로 돌려받습니다.
- **OFD 잠금(Open File Description lock)**: 프로세스 전체가 아니라 열어 둔 파일 핸들(서술자) 단위로 파일 일부 범위를 잠그는 리눅스 잠금 기능입니다. 같은 프로그램 안 여러 스레드나 여러 수집 프로세스 사이에서도 서로 부딪히지 않게 자원을 나눕니다.
- **`pagemap` (`/proc/<pid>/pagemap`)**: 가상 메모리 4 KiB 칸마다 실제로 물리 RAM에 올라왔는지, 스왑에 있는지 커널이 비트 단위로 알려 주는 커널 표입니다.
- **SIMD(AVX2)**: CPU가 한 번에 256비트(32바이트)씩 묶어 데이터가 모두 0인지 한꺼번에 비교하는 고속 연산 명령입니다.

---

## 2. 전체 동작 흐름

1. **요청 조율과 중복 확인 (`pod_lock`, `shared_queue`)**: 같은 Pod UID에 대해 이미 진행 중인 `--request-id`가 있으면 새 수집을 시작하지 않고 기존 작업 경로와 종료 코드 `3`을 반환합니다. 같은 Pod의 다른 요청은 FIFO(먼저 온 순서) 대기열에서 기다립니다.
2. **프리즈 없는 대상 탐색 (`identity`)**: `/sys/fs/cgroup` 또는 `/host/sys/fs/cgroup`을 읽어 Pod에 속한 컨테이너·샌드박스 PID를 찾습니다. 이때 `--pod` 대상 프로세스는 멈추지 않습니다.
3. **안전한 출력 디렉터리와 공간 점검 (`safe_output`, `main`)**: 출력 폴더를 심볼릭 링크를 따라가지 않는 방식(`O_NOFOLLOW`)과 소유자 전용 권한(`0o700`)으로 만들고, `manifest.json` 저장 여유를 위해 `posix_fallocate`로 공간을 먼저 확보합니다.
4. **프로세스별 짧은 정지와 핸들 고정 (`corex/ptrace`, `corex/mod`, `corex/procfs`)**:
   - 대상 PID를 `SYS_pidfd_open`으로 열고 시작 시각(`start_ticks`)을 확인해 PID 재사용으로 다른 프로그램을 잘못 잡지 않도록 막습니다.
   - `PTRACE_SEIZE`·`PTRACE_INTERRUPT`로 한 프로세스만 잠깐 멈춘 뒤 스레드 레지스터 노트(`NT_*`), `maps`/`smaps` 정보, `/proc/<pid>/mem`·`/proc/<pid>/pagemap` 파일 핸들을 엽니다.
   - **버퍼 경로(`Snapshot`)**: 대상 메모리 중 실제 읽기가 필요한 크기가 허용 버퍼 안에 들어오면 RAM 버퍼로 빠르게 복사한 뒤 **곧바로 `PTRACE_DETACH`로 프로세스를 다시 실행**하고, 이후 백그라운드에서 디스크에 씁니다.
   - **스트리밍 경로(`SparseWriter`)**: 버퍼 한도를 넘는 큰 프로세스는 정지 상태에서 곧바로 희소 파일로 씁니다.
5. **2단계 0-페이지 고속 처리와 인라인 SHA-256 (`corex/elf`, `corex/mod`)**:
   - **1단계 (`pagemap`)**: `smaps` `VmFlags`로 안전 조건이 확인된 일반 익명 구간에서 커널 비트가 미할당 0-페이지로 표시한 칸은 `/proc/<pid>/mem` `read` 시스템 콜 자체를 건너뛰고 0-위치로 기록합니다.
   - **2단계 (AVX2 SIMD + `lseek`)**: 실제로 읽어 온 페이지도 256비트 AVX2 연산으로 0인지 검사해, 0인 구간은 디스크에 쓰지 않고 파일 오프셋만 앞으로 이동(`lseek`)합니다.
   - 쓰기 완료 시 버퍼가 잡혀 있으면 메모리에 남아 있는 데이터와 `HASH_ZERO_SLICE`(64 KiB 0-조각)를 조합해 **별도 디스크 재읽기 없이** 한 번에 SHA-256을 계산(`hash_mode: "inline_write"`)하고, 스트리밍이나 skip 비활성 경로 등 필요한 경우 동기화된 파일 핸들에서 다시 읽어 SHA-256을 계산(`hash_mode: "post_sync_fd_read"`)합니다.
6. **페이지 캐시 비우기, 내구성 확보 및 무교체 게시 (`main`, `safe_output`)**:
   - `--max-write-mbps`(1..10000)가 지정되면 초당 쓰기 속도를 제한해 운영 디스크 I/O 포화를 막습니다.
   - `sync_file_range`와 `fsync`로 디스크에 내구 저장한 뒤 `POSIX_FADV_DONTNEED`를 호출해 수집기 파일 데이터가 운영체제 페이지 캐시를 점유해 다른 앱 메모리를 밀어내지 않게 합니다.
   - `SYS_renameat2(..., RENAME_NOREPLACE)`로 임시 파일을 최종 `.core`·`manifest.json`으로 원자적 게시해 기존 파일 덮어쓰기를 원천 차단합니다.

---

## 3. 모듈별 코드 · 기능 · 근거 표

| 파일 | 역할 | 주요 심볼 · 상수 · 근거 |
|---|---|---|
| `src/main.rs` | CLI 파싱, 파이프라인 조율, 매니페스트 및 종료 처리 | `--pod`(`--pod-uid`), `--output`, `--request-id`, `--wait-timeout-ms`, `--max-write-mbps 1..10000`, `--format tree\|json`, `--full-ids`, `CancellationToken`, `atomic_json`, `finalize_core`, `posix_fallocate` 매니페스트 예약, `RLIMIT_NOFILE` 보류 FD 예산, `POSIX_FADV_DONTNEED`, 종료 코드 `0/1/2/3` |
| `src/identity.rs` | Pod UID 검증 및 cgroup v1/v2 컨테이너 식별 | `valid_uid`, `parse_container_segment`, `/sys/fs/cgroup`, `/host/sys/fs/cgroup`, `PODDUMP_CGROUP_ROOT` |
| `src/pod_lock.rs` | Pod 단위 직렬화 잠금·만료 잠금 청소·중복 요청 연결 | `STALE_LOCK_MAX_AGE_SECS = 3600`, `gc_stale_locks` (부트 ID·경과 시간 기반 정리), 바이트 0 Pod 잠금, 바이트 1 메타데이터 잠금 |
| `src/shared_queue.rs` | 동일 Pod 동시 요청 FIFO 대기열 | `SLOTS = 256`, OFD 슬롯 잠금 `LOCKS = 1 << 47` |
| `src/shared_budget.rs` | 노드 내 다수 `poddump` 실행 간 자원 예산 공유 | `poddump-shared-v3`, `PODDUMP_RUNTIME_DIR`, `/run\|/var/run\|/var/tmp/poddump-<uid>/credits-v3`, `MEMORY`·`BACKLOG`·`FILES`·`SYNC` 바이트 범위 잠금, `acquire_sync` |
| `src/pipeline.rs` | 프로세스 내부 동시 작업 예산 및 기아 방지 | 바이트 범위 `Budget`, `skips >= 16` 우선권 보장 |
| `src/safe_output.rs` | 심볼릭 링크 차단·사적 권한·무교체 파일 게시 | `O_NOFOLLOW \| O_DIRECTORY \| O_CLOEXEC`, `mkdirat(0o700)`, `SYS_renameat2` + `RENAME_NOREPLACE` (`publish_file`), `check_file`, `remove_owned` |
| `src/cli_output.rs` | 트리·JSON 요약 출력 및 터미널 제어 문자 차단 | 컨테이너별 상태 트리 렌더링, `c.is_control() -> ' '` 치환으로 터미널 조작 방지 |
| `src/corex/mod.rs` | 대상 고정·수집 전략 선택·인라인 해시·대역폭 제어 | `SYS_pidfd_open`, `Snapshot` vs `SparseWriter`, `HASH_ZERO_SLICE` (64 KiB), `known_zero_ranges`, `max_write_mbps * 1_000_000.0` (대기 상한 `0.25s`), `sync_file_range(SYNC_FILE_RANGE_WRITE)` |
| `src/corex/procfs.rs` | 메모리 맵 수집 및 안전 필터링 판정 | `read_maps`, `disambiguate_map_files`, `apply_smaps_inspection` |
| `src/corex/ptrace.rs` | 프로세스 정지 및 즉시 해제 | `PTRACE_SEIZE`, `PTRACE_INTERRUPT`, `wait_stopped_or_exited`, `PTRACE_DETACH` |
| `src/corex/elf.rs` | 동적 ELF 헤더 예약·`pagemap` 생략·AVX2 희소 기록 | `MIN_PHDR_SLACK = 64`, `MAX_PHDR_SLACK = 1024`, `MAX_PHDRS = 524_288`, `header_reserve_bytes_for`, `PM_NONZERO_BITS = (1<<63)\|(1<<62)\|(1<<61)`, `next_pagemap_step` (2048 페이지), `is_all_zero_avx2` (`_mm256_loadu_si256` + `_mm256_testz_si256`) |
| `src/corex/notes.rs` | 스레드/프로세스 상태 ELF 노트 직렬화 | `NT_PRSTATUS`, `NT_FPREGSET`, `NT_PRPSINFO`, `NT_AUXV`, `NT_FILE`, `NT_ARM_PAC_MASK` |
| `build.rs` / `src/build_info.rs` | 소스·벤더 해시 및 실행 파일 자기 해시 생성 | 컴파일 시 `source_sha256`·`vendor_tree_sha256` 내장, 실행 시 `/proc/self/exe` SHA-256 계산(`runtime_executable_sha256`) |

---

## 4. 커버리지 보존과 무결성 안전 규칙

성능 최적화가 포렌식 증거를 빠뜨리거나 변형하지 않도록 코드에 다음 규칙이 고정되어 있습니다.

1. **`MADV_DONTDUMP` 영역 유지**: 대상 프로그램이 크래시 덤프에서 빼 달라고 표시한(`VmFlags: dd`) 구역이라도 `procfs.rs`는 `should_dump = true`로 유지해 `PT_LOAD`에 포함합니다. 공격자 코드나 비밀 키를 숨기려 이 플래그를 악용해도 놓치지 않기 위함입니다.
2. **`PROT_NONE` 선별 제외**: 접근 권한이 전혀 없는(`PROT_NONE`) 익명 매핑 중 실제 커밋된 메모리(`rss == 0 && swap == 0`)가 없는 가드 페이지(`uncommitted_anonymous_prot_none`)만 `PT_LOAD`에서 빼고, 제외된 바이트를 `manifest.json`의 `excluded_uncommitted_prot_none_bytes`에 기록합니다.
3. **`pagemap` 건너뛰기 제한 조건 (`procfs.rs`, `elf.rs`)**:
   - `smaps` `VmFlags`에 장치/특수 매핑(`io`, `pf`), 대용량 페이지(`ht`), 특수 커널 플래그(`mm`, `sh`, `ui`, `uw`, `up`)가 있으면 장치 부작용이나 커널 제로 페이지 의미를 훼손하지 않기 위해 `pagemap` 건너뛰기를 끕니다.
   - `pagemap` 항목에서 present(`1<<63`), swapped(`1<<62`), file/shared(`1<<61`) 비트가 모두 `0`인 경우만 0-채움으로 간주하며, 한 번이라도 불명확하거나 파일 기반 매핑이면 직접 `/proc/<pid>/mem`에서 읽습니다.
4. **동적 `PHDR` 슬랙 (`elf.rs`)**: 소형 컨테이너가 과거 고정 4 MiB 헤더 패딩 때문에 실제 메모리보다 큰 파일을 만들지 않도록 필요한 프로그램 헤더 수에 맞춰 `MIN_PHDR_SLACK = 64`~`MAX_PHDR_SLACK = 1024` 범위(상한 `MAX_PHDRS = 524_288`)로 헤더 공간을 잡습니다.

---

## 5. 운영 한계와 검증 근거

- **한계**: 각 프로세스를 개별적으로 정지하므로 Pod 전체의 단일 시점 원자적 스냅샷이 아닙니다. `SIGKILL`로 강제 종료되면 최종 `manifest.json` 없이 일부 임시 파일만 남을 수 있습니다. 협력 실행되는 모든 `poddump` 인스턴스는 같은 유효 UID와 공유 런타임 디렉터리를 써야 합니다.
- **검증 근거 (WSL2 `6.18.33.2-microsoft-standard-WSL2`, 기준판 `daf33fd` 대 `v0.5.0`)**:
  - **256 MiB buffered (3회 중앙값)**: 경과 시간 `601.624 ms -> 264.65 ms`, `PTRACE` 정지 시간 `231.486 ms -> 35.512 ms`(약 6.52배 단축), `/proc/<pid>/mem` `read` 호출 `288 -> 58..59`회, 디스크 할당 크기 `36,507,648 -> 19,369,984..19,374,080 B`.
  - **512 MiB streaming (3회 중앙값)**: 경과 시간 `594.973 ms -> 535.815 ms`, 정지 시간 `77.229 ms -> 37.613 ms`, `pagemap_skipped_bytes` `503,668,736..503,672,832 B`.
  - **소형 `pause` 프로세스**: 논리 코어 크기 `6,488,064 -> 2,826,240 B`(-56.4%).
  - **동일 바이트 SHA-256 교차 검증**: `pagemap` 생략을 켠 기본 실행(`inline_write`)과 `--no-pagemap-skip`(`post_sync_fd_read`)의 전체 코어 SHA-256과 디스크 파일 SHA-256이 모두 `2b69184f8d6914cbbb9f4a081fcd344a7017f53d57e9dff3815c3931ab68e544`로 일치했고, `MADV_DONTDUMP` witness 보존 및 `8 MiB` `PROT_NONE` 가드 제외, `rrudrok` 분석 및 `read-memory` 호환까지 통과했습니다.
  - **오프라인 단위/정적 검사**: `cargo fmt --check`, `cargo clippy --offline --locked --all-targets -- -D warnings`(기본 및 `--features development-options`), `cargo test --offline --locked`(81 passed), `python3 scripts/build_release.py --check-vendor`를 모두 통과했습니다.
