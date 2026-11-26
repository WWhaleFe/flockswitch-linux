# Linux 2.0 구현 및 검증 기록

2026-11-26, macOS 호스트에서 네트워크 사용 없이 확인했습니다.

| 단계 | 구현 내용 | 교차 검사 |
|---|---|---|
| A | flockswitch 2.0 이름·기존 경로 유지, 1.x 설정 보존, 테스트 빌드 네트워크·인증서 일회성 이전, 기존 업데이트 자산 규칙·버전 비교 확인 | 통과, 경고 0 |
| B | 동의 기반 실행, 마스터 설정과 peer 변경 동기화, 상태·도착 효과, 네트워크 이동의 Logitech 채널 변경 제거, 모서리 단일 담당, X11 지연·카운트다운, GNOME 단일 InputCapture 재설정, 장기 유지 클립보드와 PNG/URI 처리 | 통과, 경고 0 |
| C | 라임·잉크 카드, 상위 탭 5개, 네트워크 하위 탭 4개, 동의·역할·포트·짝 맺기·클립보드 전송 상태, 서브 읽기 전용 설정, 모서리 확인 창, 트레이 상태와 한국어/영어 | 통과, 경고 0 |
| D | main/v2 CI, netcore 검사, Xvfb 스모크 검사, 기존 배포 자산명 유지, 릴리스 안내·README·CHANGELOG·개인정보 문서 | 통과, 경고 0 |
| E | 호스트 검사 경로, 이전 설정 호환 테스트, X11 ignore 테스트 6개, 이름·문자·공백·셸 구문 검사 | 교차 검사 통과, 경고 0. 실행 제약은 아래 참고 |

모든 교차 검사는 `sh target/check-v2.sh --all-targets --offline`로 실행했습니다.
로그는 `target/check-A.log`부터 `target/check-E.log`까지입니다.

## 실행 검사

`python3 scripts/test-host.py`는 실제 소스 모듈을 호스트 테스트용 Cargo 프로젝트에
연결합니다. waveform 상수와 단축키 파서는 실제 소스에서 그대로 추출하며, Linux
HID·GUI 런타임을 링크하지 않습니다. Cargo는 항상 `--offline`으로 실행됩니다.

| 호스트 테스트 | 통과 개수 |
|---|---:|
| config (마이그레이션 5개, 모서리 담당 2개, 0.15/테스트 빌드 호환 2개 포함) | 37 |
| clipboard (PNG 변환·경계, URI 목록) | 3 |
| net_logic (설정 전달·상태·동의·100MB·백엔드 규칙) | 4 |
| edge_logic | 24 |
| setup_logic | 24 |
| locate_logic | 10 |
| window_size | 4 |
| 업데이트·Homebrew 경로 (실제 소스에서 추출) | 4 |
| 합계 | 110 |

`cargo test --offline --manifest-path netcore/Cargo.toml`은 단위 테스트 27개 중
26개 통과, `tests::port_checks` 1개 실패입니다. 실패 지점은 TCP listener 바인딩이며
macOS 샌드박스가 `Operation not permitted`를 반환했습니다.

나머지를 확인하기 위해 같은 명령에 `-- --skip tests::port_checks`를 붙여 실행했습니다.
단위 테스트 26개와 config_compat 통합 테스트 1개가 통과했습니다. service 통합 테스트는
11개 중 1개(동의 전 listener가 없음) 통과, 10개가 같은 소켓 권한 제한으로 실패했습니다.
중복 실행을 제외하면 netcore 39개 중 28개 통과, 11개는 환경 제한으로 실패했습니다.
netcore는 수정하지 않았으며 이 결과를 전체 통과로 간주하지 않습니다.

X11 스모크 테스트 6개(화면, 커서/warp, 포인터 주입, 텍스트, PNG, 파일 목록)는
`#[ignore]`로 추가했고 `--all-targets`에서 컴파일했습니다. 이 호스트에는 Linux/Xvfb가
없어 실행하지 못했습니다. CI는 다음 명령으로 순차 실행합니다.

```sh
xvfb-run -a cargo test --locked -- --ignored --test-threads=1
```

`git diff --check`, 설치 스크립트와 build-deps.sh의 셸 구문 검사를 통과했습니다.
옛 테스트 이름은 마이그레이션 코드·테스트와 기존 명세에만 남았고, 변경 파일에는
em dash 문자가 없습니다. 참조 프로젝트와 netcore의 변경은 없습니다.

## 남은 확인

- 소켓 바인딩이 허용되는 호스트에서 netcore 전체 39개 검사를 다시 실행해야 합니다.
- Linux에서 앱 전체 단위 테스트, Xvfb 스모크 6개와 release 빌드를 실행해야 합니다.
- 이 호스트는 Linux ELF를 링크하지 못하므로 `readelf -d` 결과를 확인하지 못했습니다.
  arboard의 Linux X11 경로는 Rust x11rb, Wayland 경로는 기존 wayland-client를 사용하며
  새 빌드용 -dev 패키지를 추가하지 않았습니다. CI와 release의 기존 readelf 단계가
  실제 NEEDED 라이브러리를 출력합니다. 배포 전 이 출력을 확인해야 합니다.
- X11: 실기기 다중 화면 넘김·복귀·Control 조건·연결 끊김·키 해제, 카운트다운, 텍스트·
  이미지·파일 붙여넣기와 클립보드 소유 수명을 확인해야 합니다.
- GNOME Wayland: 입력 캡처/원격 데스크톱 두 권한, 취소와 재허용, 담당 전환 시 장벽
  해제 순서, 지연 중 안쪽 이동 취소, 긴급 복귀와 도착 효과를 확인해야 합니다.
  XWayland 화면 좌표가 맞지 않으면 카운트다운 없이 지연만 적용합니다.
- KDE/기타 Wayland: ConnectToEIS 받기, data-control 또는 XWayland 클립보드,
  기존 layer-shell Logitech 모서리·GlobalShortcuts·화면 배율을 확인해야 합니다.
- 0.15.0 설치 위 업데이트와 기존 장치·버튼·휠 상태·백업, Homebrew 업데이트,
  실제 macOS/Windows 2.0과의 짝 맺기·마스터/서브 설정 전달, 한글 키를 확인해야 합니다.
- 한국어 글꼴·ASCII 창 제목·트레이·모든 설정 탭과 보조 창의 실제 배치를 확인해야 합니다.

Linux 2.0은 베타이며 이 작업에서 실제 Linux 하드웨어 검증은 수행하지 못했습니다.

## 변경 파일과 이유

| 파일 | 이유 |
|---|---|
| Cargo.toml, src/config.rs, src/update.rs | 2.0 설명, 기존 설정·인증서 이전과 모서리 담당, 버전·자산 호환 검사 |
| src/net.rs, src/net_logic.rs, src/net_x11.rs, src/net_wayland.rs, src/clipboard.rs | 네트워크 실행·동기화·상태·클립보드와 X11 스모크 검사 |
| src/edge.rs, src/edge_wayland.rs, src/edge_portal.rs, src/shared.rs | 단일 모서리 담당, 지연·카운트다운과 포털 장벽 전환 |
| src/settings_ui.rs, src/tray.rs, src/i18n.rs, src/main.rs | 2.0 설정 창·트레이·번역과 새 모듈 연결 |
| .github/workflows/ci.yml, .github/workflows/release.yml | v2 CI, netcore·Xvfb 검사와 개인정보 문서 동봉 |
| README.md, README.ko.md, CHANGELOG.md, CHANGELOG.ko.md, PRIVACY.md, packaging/linux/release-notes/beta.md | 네트워크 사용·지원 범위·베타·권한·개인정보 안내 |
| scripts/test-host.py, docs/V2_VALIDATION.md | 오프라인 호스트 검사와 단계별 결과·남은 확인 기록 |
