# 릴리스 검증

`poddump 0.5.0`(`prod-hardening` 브랜치)은 오프라인 코드·빌드 검사, 정적 `musl` 바이너리 무결성, 단순 CLI 검사, 지역 WSL2(`ext4`) 환경의 `main`·이전 후보 대비 256/512 MiB 3회 교차 성능 측정과 특수 매핑·분할 헤더 검증, `rrudrok 0.5.0` 분석 호환 검증을 모두 통과했습니다. `main` 병합과 공개 릴리스 여부는 사용자 결정에 따라 진행합니다.

## 1. 정적 빌드와 코드 검사

오프라인 환경에서 아래 검사를 모두 통과했습니다.

- `cargo fmt --check`, `cargo clippy --offline --locked --all-targets -- -D warnings`, `python3 scripts/build_release.py --check-vendor` 통과
- `cargo test --offline --locked`, `cargo test --offline --locked --release`, `x86_64-unknown-linux-musl` 정적 릴리스 테스트 각각 **94개 통과, 실패 0개**
- 배포 바이너리(`1,580,304`바이트, SHA-256 `28bf4e76aaf5c4777d93e7b1c82ca9ad13bbf600aca7e9687f9bcf00344806d2`) 확인: 외부 공유 라이브러리 없이 정적 연결(`statically linked`), 기능 플래그 비어 있음(`features: []`), 배포 `manifest.json`의 소스 19개 SHA-256 전부 현재 소스와 일치(자세한 해시는 배포 `manifest.json`과 `SHA256SUMS.txt` 참조)

## 2. 단순 CLI 검증

정적 배포 바이너리는 `--pod <전체_Pod_UID> --output <새_폴더>`와 `-h`/`--help`, `-V`/`--version`만 제공합니다. 도움말·버전 출력과 제거된 레거시 옵션(`--max-write-mbps` 등) 거부를 확인했습니다.

## 3. 성능 개선·특수 메모리 안전성·`rrudrok` 호환 검증

지역 WSL2 커널(`6.18.33.2-microsoft-standard-WSL2`, `ext4`)에서 `main`, 이전 후보(`b93155d`), 현재 개선판(`v0.5.0`)을 매번 새 프로세스로 3회 교차 비교했습니다. 테스트 프로세스는 256 MiB 또는 512 MiB 익명 영역의 앞 32 MiB에 4 KiB 데이터와 4 KiB 0 페이지를 번갈아 채우고, `MADV_DONTDUMP` 구간과 미할당 8 MiB `PROT_NONE` 보호 구간을 함께 둡니다.

| 시험 | 항목 | `main` | 이전 후보(`b93155d`) | 개선판(`v0.5.0`) | `main` 대비 변화 |
|---|---|---:|---:|---:|---|
| 256 MiB (buffered) | 대상 정지 (중앙값) | 147.276 ms | 23.333 ms | 26.055 ms | 82.3% 단축 |
| 256 MiB (buffered) | 전체 수집 (중앙값) | 400.795 ms | 189.904 ms | 159.268 ms | 60.3% 단축 |
| 256 MiB (buffered) | 디스크 할당 (중앙값) | 36,511,744 B | 19,378,176 B | 36,204,544 B | extent 2개 유지 |
| 512 MiB (streaming) | 대상 정지 (중앙값) | 87.444 ms | 27.295 ms | 29.849 ms | 65.9% 단축 |
| 512 MiB (streaming) | 전체 수집 (중앙값) | 486.152 ms | 446.867 ms | 227.542 ms | 53.2% 단축 |
| 512 MiB (streaming) | 디스크 할당 (중앙값) | 36,450,304 B | 19,378,176 B | 36,208,640 B | extent 2개 유지 |

개선판은 `64 KiB` 미만 내부 0 구간을 연속 쓰기로 묶고 `64 KiB` 이상·끝단만 Sparse 구멍으로 처리하며 `SEEK_DATA`/`SEEK_HOLE` 기반 해시 계산을 사용합니다. 앞 32 MiB처럼 4 KiB 0 페이지가 촘촘히 섞인 구간에서는 디스크 구멍을 쪼개지 않아 할당 크기가 `main`과 비슷하지만, 전체 수집 시간과 정지 시간을 모두 크게 줄였습니다. 뒷단 224 MiB/480 MiB 0 구간은 그대로 Sparse 절약을 유지하고, 모든 바이트와 `MADV_DONTDUMP` 증인이 일치했습니다.

- **읽기 실패 `userfaultfd` 영역 (`uffd`)**: 사용자 공간에서 페이지 폴트를 처리하는 `userfaultfd` 구간이 읽히지 않을 때 0바이트로 속이지 않고 **종료 코드 2(`partial`)**, 실패 바이트 `262,144`B를 기록했습니다.
- **512개 분할 읽기 실패와 `PT_LOAD` 헤더 보존 (`guard512`)**: `PT_LOAD`(ELF Core 안에 각 메모리 구간의 위치와 크기를 적는 프로그램 헤더)가 여러 읽기 실패로 나뉘는 상황에서 **534개 `PT_LOAD`**, 실패 바이트 `2,097,152`B를 기록하며 유효한 partial Core를 저장했습니다(종료 코드 2).
- **`rrudrok 0.5.0` 분석 호환**: `analyze --verify-hashes`(오류 0건, 경고 0건, 분석 후 해시 검증 일치)와 `read-memory`로 Core 안의 증인 문자열(`PODDUMP_POD_C_DONTDUMP_20261115_VERIFIED`)을 그대로 읽었습니다.

검증에 사용한 임시 덤프와 작업 폴더는 확인 직후 모두 삭제했습니다. 이 수치는 WSL2 `ext4` 환경에서 모의 부하로 측정한 것이며, 실제 클라우드 스토리지(EBS 등)의 이전 EC2 기록과는 별개입니다.

## 4. 배포 전 확인 상태

소스 검증, 정적 musl 릴리스 빌드, WSL2 기능·성능 비교와 `rrudrok` 호환 검증, 임시 덤프 정리를 마쳤습니다. `main` 병합과 공개 릴리스만 사용자 확인 후 진행합니다.

