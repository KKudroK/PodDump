# 릴리스 검증

`poddump 0.5.0` 후보판은 오프라인 코드·빌드 검사, 정적 `musl` 바이너리 무결성, 단순 CLI 18건 검사, 지역 WSL2 환경의 특수 매핑·분할 헤더·대용량 익명 메모리 수집 검증, `rrudrok 0.4.0` 분석 호환 검증을 모두 통과했습니다. 실제 AWS EC2 노드 수집 검증은 아직 남아 있으므로, 이 확인이 끝나기 전에는 `main` 병합과 공개 릴리스를 완료하지 않습니다.

## 1. 정적 빌드와 코드 검사

오프라인 환경에서 아래 검사를 모두 통과했습니다.

- `cargo fmt --check`, `cargo clippy --offline --locked --all-targets -- -D warnings`, `python3 scripts/build_release.py --check-vendor` 통과
- `cargo test --offline --locked`, `cargo test --offline --locked --release` 각각 **92개 통과, 실패 0개**
- 배포 바이너리(`1,439,560`바이트, SHA-256 `3591065e8c35994a570d2b2767c9f1043d3b6fe6d583eafe2d4a760ed37284ba`) 확인: 외부 공유 라이브러리 없이 정적 연결(`statically linked`), 기능 플래그 비어 있음(`features: []`), 배포 `manifest.json`의 소스 19개 SHA-256 전부 현재 소스와 일치(자세한 해시는 배포 `manifest.json`과 `SHA256SUMS.txt` 참조)

## 2. 단순 CLI 검증

정적 배포 바이너리로 CLI 18건을 확인했습니다. 정상 경로 5건(인자 없는 도움말, `--help`, `-h`, `--version`, `-V`)과 오류 거부 13건(제거된 옵션 `--pod-uid`·`--request-id`·`--max-write-mbps`·`--build-info`·`--wait-timeout-ms`·`development-options`, 중복·누락 인자 등) 모두 기대한 종료 코드와 오류 안내(`poddump --pod <전체_Pod_UID> --output <새_폴더>`)를 확인했습니다.

## 3. 특수 메모리 안전성·희소 파일 무결성·`rrudrok` 호환 검증

지역 WSL2 커널(`6.18.33.2-microsoft-standard-WSL2`)에서 정적 배포 바이너리로 세 가지 실동작 조건을 검증했습니다.

- **읽기 실패 `userfaultfd` 영역 (`poddump_uffd_missing`)**: 사용자 공간에서 페이지 폴트를 처리하는 `userfaultfd` 구간이 읽히지 않을 때 0바이트로 속이지 않고 **종료 코드 2(`partial`)**, 실패 바이트 `262,144`, 읽기 오류 `1`건을 기록했습니다(`84.797 ms`).
- **512개 분할 읽기 실패와 `PT_LOAD` 헤더 보존 (`poddump_guard512`)**: `PT_LOAD`(ELF Core 안에 각 메모리 구간의 위치와 크기를 적는 프로그램 헤더)가 여러 읽기 실패로 512조각 이상으로 나뉘는 상황에서 헤더 공간이 모자라 중단되지 않고 **534개 `PT_LOAD`**, 실패 바이트 `2,097,152`, 읽기 오류 `512`건을 기록하며 유효한 partial Core를 저장했습니다(`115.349 ms`, 종료 코드 2).
- **256 MiB 아레나 수집·희소 파일 무결성·특수 표시 유지 (`poddump_arena256m`)**:
  - `pagemap`(프로세스의 각 4 KiB 메모리 칸이 실제 RAM에 올라와 있는지 알려 주는 커널 표)으로 미할당 익명 페이지 **235,171,840바이트**는 `read` 호출 없이 건너뛰고, 접근 권한과 실제 할당이 없는 미커밋 `PROT_NONE` 구간 **8,388,608바이트**만 제외해 **252.534 ms**(`rc=0`, `complete`)에 완료했습니다.
  - 희소 파일(0바이트만 연속된 구간은 실제 디스크 블록을 쓰지 않고 빈 구간 정보만 남기는 파일)로 논리 크기 `273,182,720`바이트 중 디스크에 `19,374,080`바이트만 할당하면서도, 인라인 SHA-256(`inline_write`) `94e1083b6d102a69c511097eefdb87f06a3338c5b47e21d7dd7afcb23688722c`과 디스크 파일 전체 읽기·ELF 검증(`elf_reads_verified: true`)이 일치했습니다.
  - `MADV_DONTDUMP`(보통 코어 덤프에서 빼 달라고 표시한 구역)는 숨겨진 증거 보존을 위해 빼지 않고 남겼으며, `rrudrok 0.4.0`의 `analyze --verify-hashes`(오류 0건, 경고 0건)와 `read-memory`로 `Pod C` Core 안의 증인 문자열(`PODDUMP_POD_C_DONTDUMP_20261115_VERIFIED`)을 그대로 읽었습니다.

검증에 사용한 임시 덤프와 작업 폴더는 확인 직후 삭제했습니다. 이 결과는 단일 WSL2 커널에서 모의 부하로 확인한 것이며, 실제 쿠버네티스 노드의 컨테이너 런타임과 디스크·네트워크 환경을 대신하지 않습니다.

## 4. 남은 확인

AWS 로그인 후 실제 EC2 쿠버네티스 노드에서 `poddump --pod <전체_Pod_UID> --output <새_폴더>` 실수집, 부분 수집과 중복 잠금 해제, 전체 SHA-256과 `rrudrok` 분석 호환을 확인하고 임시 덤프를 정리한 뒤 `main` 병합과 릴리스를 진행합니다.

