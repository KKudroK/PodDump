# PodDump

PodDump는 **Pod UID 하나를 받아, 해당 Pod의 프로세스 메모리를 파일로 저장하는 도구**입니다. 저장한 메모리 파일을 덤프라고 합니다. 각 프로세스의 ELF Core와 수집 결과를 적은 `manifest.json`을 만듭니다. 버전은 **0.5.0**입니다.

[다운로드](https://github.com/KKudroK/PodDump/releases/latest) · [빠른 시작](#빠른-시작) · [옵션](#옵션) · [결과 확인](#결과-확인) · [코드와 수집 원리](docs/CODEBASE_GUIDE.md)

## 실행 조건

- Linux x86-64 노드에서 실행합니다. 공개 바이너리는 ARM64용이 아닙니다.
- 대상 Pod가 실행 중인 **노드에 접속**해야 합니다. 다른 노드의 Pod는 수집하지 못합니다.
- 호스트의 `/proc`와 cgroup을 볼 수 있고 `ptrace`를 사용할 권한이 필요합니다. `ptrace`는 다른 프로세스를 멈추고 상태를 읽는 Linux 기능입니다. 일반적으로 `sudo`로 실행하지만, 노드의 보안 정책이 접근을 막으면 실패할 수 있습니다.
- 출력에는 Linux 파일 권한과 `fsync`를 지원하는 저장소를 사용합니다. `fsync`는 파일을 저장 장치에 반영하도록 요청하는 기능입니다. 부모 디렉터리는 미리 만들고, 수집 결과 디렉터리는 매번 새 이름을 사용합니다.

실행 파일 하나로 동작합니다. 수집 노드에 Rust, Python, GDB를 설치할 필요는 없습니다.

## 이 도구를 쓰는 이유

Pod UID로 수집을 시작하면 같은 Pod에 속한 프로세스를 찾고, 컨테이너 ID·PID·프로세스 시작값을 Core와 함께 기록합니다. 분석가가 PID를 하나씩 골라 덤프를 만들고 Pod 소속을 따로 정리하는 작업을 줄일 수 있습니다. PID가 재사용되거나 읽기에 실패한 경우도 수집 기록에서 구분합니다.

`MADV_DONTDUMP` 영역도 보존하고, 큰 0 구간은 읽었을 때 같은 값이 나오는 희소 파일로 저장합니다. GDB도 [설정을 바꾸면 제외 표시 영역을 수집할 수 있습니다](https://sourceware.org/gdb/current/onlinedocs/gdb.html/Core-File-Generation.html). PodDump의 장점은 이 기능 하나가 아니라 Pod 식별·프로세스별 수집·무결성·누락 기록을 한 명령으로 묶는 데 있습니다. Pod 전체가 같은 순간에 멈춘 수집본은 아닙니다.

## 빠른 시작

### 1. 다운로드하고 확인하기

수집할 Linux 노드에서 실행합니다. `manifest.json`은 배포 파일의 빌드 정보입니다. 뒤에서 만드는 수집 결과의 `manifest.json`과 저장 위치가 다릅니다.

```sh
mkdir -p poddump_download
cd poddump_download
curl -fLO https://github.com/KKudroK/PodDump/releases/download/v0.5.0/poddump
curl -fLO https://github.com/KKudroK/PodDump/releases/download/v0.5.0/manifest.json
curl -fLO https://github.com/KKudroK/PodDump/releases/download/v0.5.0/SHA256SUMS.txt
sha256sum -c SHA256SUMS.txt
chmod 755 poddump
./poddump --version
```

`sha256sum`의 두 결과가 모두 `OK`인지 확인합니다. SHA-256은 파일 내용이 바뀌었는지 비교하는 데 쓰는 값입니다.

### 2. Pod UID와 노드 확인하기

`kubectl`을 사용할 수 있는 곳에서 실행합니다. `default`와 `my-pod`는 실제 Namespace와 Pod 이름으로 바꾸세요.

```sh
kubectl get pod -n default my-pod   -o jsonpath='{.metadata.uid}{"\n"}{.spec.nodeName}{"\n"}'
```

첫 줄은 Pod UID, 둘째 줄은 노드 이름입니다. Pod UID는 Pod를 구별하는 전체 값입니다. Pod 이름이나 UID의 일부를 수집 명령에 넣으면 안 됩니다. 이 확인 단계에만 `kubectl`이 필요합니다.

### 3. 수집하기

확인한 노드에서 실행합니다. `POD_UID`에 앞서 확인한 전체 값을 넣으세요. `/evidence` 대신 준비한 저장소 경로를 쓸 수 있습니다.

```sh
POD_UID='여기에-전체-Pod-UID'
sudo install -d -m 700 /evidence
sudo ./poddump --pod "$POD_UID" --output /evidence/pod_001
result=$?
printf '종료 코드: %s\n' "$result"
```

다음 수집에는 `pod_002`처럼 새 이름을 사용합니다. 이미 있는 결과 디렉터리를 덮어쓰지 않습니다.

### 4. 결과 확인하기

```sh
sudo ls -lh /evidence/pod_001
sudo cat /evidence/pod_001/manifest.json
```

`manifest.json`의 `status`와 각 `processes` 항목을 확인합니다. 화면의 완료 표시와 함께 **종료 코드가 0인지**도 확인해야 저장 완료를 판단할 수 있습니다.

## 결과 확인

| 파일 | 내용 |
|---|---|
| `PID_시작시각.core` | 해당 프로세스의 ELF Core. 메모리와 수집한 CPU 상태를 담습니다. |
| `manifest.json` | Pod UID, 컨테이너 ID, 프로세스 식별 정보, 수집 범위, 실패 이유, Core의 SHA-256 값 |
| `.partial` 파일 | 작업이 끝나지 않았거나 저장 중 문제가 생겨 남은 파일. 완료본으로 취급하지 않습니다. |

| 종료 코드 | 뜻 | 다음 행동 |
|---:|---|---|
| `0` | 정해진 수집 정책에 따라 완료하고 저장을 마침 | `manifest.json`과 Core를 함께 보관합니다. |
| `2` | 일부만 수집함 | 실패·제외·미수집 범위를 확인합니다. 남은 파일을 버리지 않습니다. |
| `1` | 인자, 준비, 저장 등의 오류 | 터미널 오류와 남은 파일을 확인합니다. |
| `3` | 같은 Pod의 같은 작업이 이미 실행 중 | 출력된 기존 경로에서 원래 작업의 완료를 확인합니다. |

## 옵션

```sh
./poddump --help
```

| 옵션 | 사용 방법 |
|---|---|
| `--pod FULL_POD_UID` | 필수. 전체 Pod UID |
| `--output NEW_DIR` | 필수. 새 결과 디렉터리 |
| `-h`, `--help` | 사용법 확인 |
| `-V`, `--version` | 버전 확인 |

작업자 수, 대기 시간, 메모리 예산, 0 페이지 생략 같은 내부 동작은 프로그램이 자동으로 정합니다.

```sh
sudo ./poddump --pod "$POD_UID" --output /evidence/pod_002
```

## 수집 범위와 주의할 점

- 프로세스를 각각 잠깐 멈추고 읽은 뒤 다시 실행합니다. Pod 전체와 커널을 같은 순간에 수집한 결과는 아닙니다. 다른 프로세스가 공유 메모리를 바꿀 수도 있습니다.
- 일반 메모리, 실행 코드, 스택 등을 수집합니다. `MADV_DONTDUMP`가 붙은 영역도 수집 대상에 남깁니다. 읽지 못한 바이트를 성공으로 표시하지 않습니다.
- 안전 조건이 확인된 익명 메모리에서 아직 할당되지 않은 0 페이지의 읽기를 생략합니다. `pagemap`의 guard·userfaultfd 표시나 `smaps` 정보가 불확실하면 실제 읽기를 시도합니다. 장치 메모리 등 제외한 범위는 수집 기록에 남깁니다.
- 0으로 채워진 구간은 `64 KiB` 이상이거나 구간 끝부분일 때 Sparse 방식(빈 구간 정보만 기록해 디스크 블록을 아끼는 저장 방식)으로 저장합니다. `64 KiB`보다 짧은 내부 0 구간은 앞뒤 데이터와 이어서 한 번에 써 파일 조각화와 디스크 지연 시간을 줄입니다. 파일을 읽을 때는 같은 0바이트가 나옵니다.
- 같은 Pod의 잠금 파일은 실행 중 자동 삭제하지 않습니다. 프로세스가 끝나면 Linux가 잠금을 해제합니다. 수집기가 실행 중일 때 잠금 파일을 수동으로 지우지 마세요.
- Core에는 환경변수와 프로그램이 사용한 값이 원문으로 포함될 수 있습니다. 결과 디렉터리는 소유자만 접근하도록 만듭니다.

프로세스 메모리와 커널을 함께 분석하려면 [KernelDump](https://github.com/KKudroK/KernelDump)의 결과와 해당 커널의 ISF도 준비합니다. ISF는 분석기가 커널 자료의 위치와 형식을 해석할 때 쓰는 파일입니다. PodDump 수집에는 필요하지 않습니다.

## 빌드와 상세 문서

소스에서 빌드할 때만 Rust와 Python 3가 필요합니다. 의존성 소스는 `vendor/`에 포함되어 있습니다.

```sh
rustup target add x86_64-unknown-linux-musl
cargo test --offline --locked
python3 scripts/build_release.py
```

결과는 `release/poddump`, `release/manifest.json`, `release/SHA256SUMS.txt`입니다. 공개 빌드에는 추가 테스트 기능이 들어가지 않습니다.

- [코드와 수집 원리](docs/CODEBASE_GUIDE.md)
- [배포 조건](docs/deployment.md)
- [배포판별 과거 검증](docs/distro_validation.md)
- [과거 성능 측정](docs/benchmark_summary.md)

MIT 라이선스입니다. 포함된 외부 의존성에는 각 프로젝트의 라이선스가 적용됩니다.
