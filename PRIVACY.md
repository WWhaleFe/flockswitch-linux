# Privacy - flockswitch for Linux

Effective November 26, 2026, for version 2.0.0 and later.

flockswitch collects no personal data, analytics or crash reports. Logitech communication
stays between this computer and its connected devices. When you accept the notice and
enable network sharing, captured mouse/keyboard input and enabled clipboard content
travel only between your own paired computers on a trusted local network. Discovery
uses mDNS; paired traffic uses authenticated TLS 1.3 (default TCP 47160). There is no
relay server or cloud storage. Keystrokes and clipboard contents are not logged.

Settings, device lists, button state, wheel modes, logs and backups stay in
`~/.config/flockswitch/`. Paired IDs and certificate fingerprints stay in `config.toml`;
this computer's certificate and private key stay in `net/` (the key has mode 0600).
Received files stay in Downloads/flockswitch, or a received folder beside network
settings if Downloads is unavailable. GNOME portal restore tokens also stay locally.

Internet access is used only for update checks and downloads from GitHub. Requests
include normal HTTP information (IP address and a version user agent), with no settings,
input or device list. Automatic update checks can be disabled in General. Homebrew
updates and optional setup package installation run only after approval and contact
their package repositories. Opening help/report links is an explicit user action;
no report is sent automatically.

General → Uninstall restores diverted buttons, disables autostart and removes settings.
Downloaded/received files outside the settings folder remain yours to delete separately.
You can also delete the settings folder yourself. Nothing is sold or shared with the
developer, Logitech or any other third party.

Questions: feedback.wwhale@gmail.com or [GitHub issues](https://github.com/WWhaleFe/flockswitch-linux/issues).

# 개인정보 안내 - flockswitch Linux

2026년 11월 26일 시행, 2.0.0 이상에 적용합니다.

개인정보·분석·충돌 보고를 수집하지 않습니다. Logitech 통신은 이 컴퓨터와 연결 장치
사이에만 이뤄집니다. 주의 사항에 동의하고 공유를 켜면 키보드·마우스 입력과 허용한
클립보드만 신뢰하는 로컬 네트워크의 짝 맺은 내 컴퓨터 사이에 전송합니다. mDNS로
탐색하고 TLS 1.3으로 통신합니다(기본 TCP 47160). 중계 서버나 클라우드 저장소는 없으며
키 입력과 클립보드 내용은 로그에 기록하지 않습니다.

설정·장치 목록·버튼 상태·휠 모드·로그·백업과 포털 복원 토큰은
`~/.config/flockswitch/`에 보관합니다. 짝의 ID와 인증서 지문은 config.toml에,
이 컴퓨터의 인증서와 개인 키는 net/에 보관합니다(개인 키 권한 0600).
받은 파일은 다운로드/flockswitch, 다운로드 폴더가 없으면 네트워크 설정 옆 received에
보관합니다.

인터넷은 GitHub 업데이트 확인·다운로드에만 사용합니다. 일반 HTTP 정보(IP와 버전 사용자
에이전트) 외 설정·입력·장치 목록은 보내지 않습니다. 일반 탭에서 자동 확인을 끌 수 있습니다.
Homebrew 업데이트와 설치 점검의 패키지 설치는 승인 후 해당 저장소에 접속합니다.
도움말·신고 링크는 사용자가 열며 자동으로 신고를 전송하지 않습니다.

일반 → 제거는 버튼을 돌려놓고 자동 실행과 설정을 제거합니다. 설정 폴더 밖의 다운로드·
받은 파일은 따로 삭제해야 합니다. 설정 폴더를 직접 삭제할 수도 있습니다.
개발자·Logitech·제3자에게 정보를 판매하거나 제공하지 않습니다.

문의: feedback.wwhale@gmail.com 또는 [GitHub 문제 신고](https://github.com/WWhaleFe/flockswitch-linux/issues).
