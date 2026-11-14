# PodDump 저장소 작업 지침

## 기본 문체 및 단일 기준 문서 유지 규칙

- 사용자용 설명 문서([docs/CODEBASE_GUIDE.md](docs/CODEBASE_GUIDE.md), [README.md](README.md))는 핵심 결론과 구체적 동작부터 쉬운 한국어로 쓰고 비유는 쓰지 않는다. 새 용어는 쉬운 말로 뜻을 먼저 풀어서 설명한다.
- 독자용 문서 본문에 검토 기준일, 조사 과정, 작성자 설명, 작업 시각, 예약 상태 같은 관리 기록을 넣지 않는다. 중요한 조건과 한계는 해당 기능을 설명하는 자리에 한 번만 적는다.
- 코드 구조·기능·검증 근거는 [docs/CODEBASE_GUIDE.md](docs/CODEBASE_GUIDE.md) 한 문서에서 유지관리한다.
- `src/` 모듈, CLI 옵션(`--pod`, `--max-write-mbps`, `development-options` 포함), `manifest.json` Schema 2 필드, 안전 가드 또는 수집 정책을 변경할 때는 **같은 작업 안에서 `docs/CODEBASE_GUIDE.md`도 함께 갱신**한다.

## 브랜치 및 변경 범위 제한

- 정식 배포 기준 브랜치인 `main`은 사용자 명시 요청 없이 직접 수정·덮어쓰기·force push 하지 않는다. 새 개선·검증 작업은 별도 작업 브랜치(예: `prod-hardening`)에서 진행한다.
- 사용자 명시 요청 없이 `git push`, GitHub 릴리스 발행, 원격 저장소 공개 작업을 수행하지 않는다.

## 오프라인 검증 게이트 (코드 변경 시 필수)

모든 Rust 코드 변경 뒤에는 오프라인·잠금 모드로 아래 5개 검사를 모두 통과해야 완료로 간주한다.

1. `cargo fmt --check`
2. `cargo clippy --offline --locked --all-targets -- -D warnings`
3. `cargo clippy --offline --locked --all-targets --features development-options -- -D warnings`
4. `cargo test --offline --locked`
5. `python3 scripts/build_release.py --check-vendor`

수집기 성능을 변경한 경우에는 `--no-pagemap-skip` 대조를 포함해 전체 코어 파일 SHA-256 일치, `PT_LOAD` 배치 유지, `MADV_DONTDUMP` 영역 보존, 미커밋 `PROT_NONE` 제외 여부를 함께 검증하고, 검증에 만든 임시 덤프 파일은 확인 직후 삭제한다.

## 핵심 설계 및 안전 불변식

- **대상 식별로 속단하지 않는다**: Pod UID·컨테이너 ID·PID 재사용 검증(`SYS_pidfd_open` + 시작 시각 `start_ticks`)을 통과한 대상만 수집한다. 모호한 귀속은 미확인으로 남긴다.
- **프리즈 없는 발견, 프로세스별 짧은 정지**: cgroup 탐색과 OFD 대기는 비정지로 수행하고, 메모리 수집 단계만 프로세스 단위로 `PTRACE_SEIZE` + `PTRACE_INTERRUPT` 정지 후 `/proc/<pid>/mem`·`pagemap` 핸들을 고정하고 즉시 분리한다. Pod 전체의 동시 스냅샷(atomic snapshot)으로 표기하지 않는다.
- **커버리지 손실 금지 원칙**:
  - `MADV_DONTDUMP`(`dd`) 표시 영역도 포렌식 증거 유실을 막기 위해 `PT_LOAD`에 유지한다.
  - Stage-1 `pagemap` 미할당 페이지 생략은 `smaps` `VmFlags`에서 장치 I/O(`io`/`pf`), hugetlb(`ht`), 특수 매핑(`mm`/`sh`/`ui`/`uw`/`up`)이 없을 때만 허용하며, 커널 비트 `(1<<63)|(1<<62)|(1<<61)|(1<<58)|(1<<57)`가 모두 0인 경우에만 0-채움으로 처리한다.
  - `PROT_NONE` 영역은 커밋된 RSS와 스왑이 모두 `0`일 때만(`uncommitted_anonymous_prot_none`) 제외하며 제외 바이트를 `manifest.json`에 명시한다.
- **희소 파일(Sparse) 논리 무결성**: 디스크 블록을 아끼기 위해 0-구간을 `seek`로 건너뛰더라도 ELF 논리 오프셋과 `PT_LOAD` 범위 내 바이트 스트림(0x00 포함)은 온전해야 하며, 인라인 SHA-256과 디스크 파일 SHA-256이 일치해야 한다.
- **출력 안전성 및 제어 문자 무해화**: 출력 디렉터리는 `O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC`, `0o700` 권한으로 열고, 최종 파일 게시는 `SYS_renameat2(..., RENAME_NOREPLACE)`를 사용한다. 환경변수·인자·복구 바이트는 마스킹하지 않되 터미널 출력 시 제어 문자는 공백 등으로 안전하게 치환한다.
