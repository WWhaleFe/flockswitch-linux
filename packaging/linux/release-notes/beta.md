> [!CAUTION]
> **베타 버전입니다.** 2.0.0은 실제 Linux PC(Ubuntu 26.04, GNOME 50, Wayland)에서 Mac과 함께 네트워크 공유까지 확인했습니다. 2.1.0의 새 기능(새 설정 창, 서브끼리 넘기기와 클립보드, 키보드 'OS 전환 시' 규칙, 잠금 해제 뒤 자동 재연결)은 아직 실기기에서 확인하지 않았습니다. X11(Xorg) 세션과 KDE Plasma·sway·Hyprland 등도 아직 시험하지 않았습니다. 문제가 있으면 [이슈](https://github.com/WWhaleFe/flockswitch-linux/issues)로 알려 주세요.
>
> **Beta.** 2.0.0 was checked on a real Linux PC (Ubuntu 26.04, GNOME 50, Wayland), network sharing with a Mac included. The new 2.1.0 features (new settings window, sub-to-sub crossing and clipboard, keyboard rules per OS pair, reconnect after unlock) have not been tried on real hardware yet, nor have X11 (Xorg) sessions and KDE Plasma, sway, Hyprland and others; please report problems as an [issue](https://github.com/WWhaleFe/flockswitch-linux/issues).

2.1은 로컬 네트워크의 내 컴퓨터끼리 마우스·키보드·클립보드를 공유하고, 서브끼리도 바로 넘어갑니다. GNOME Wayland는 입력 캡처와 원격 데스크톱 두 권한을 요청합니다.

2.1 shares mouse, keyboard and clipboard between your own computers on the local network, and subs now cross to each other directly. GNOME Wayland asks for two permissions: input capture and remote desktop.
