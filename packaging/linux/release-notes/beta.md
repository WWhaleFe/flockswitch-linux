> [!CAUTION]
> **베타 버전이며, 아직 재확인이 필요합니다.** Linux에서 실제로 동작을 확인한 것은 0.12.0까지(Ubuntu 26.04, GNOME 50, Wayland)입니다. 그 뒤 버전은 실제 Linux PC에서 다시 확인하지 않았고, X11(Xorg) 세션과 KDE Plasma·sway·Hyprland 등에서는 한 번도 시험하지 않았습니다. 예상대로 동작하지 않을 수 있으니, 문제가 있으면 [이슈](https://github.com/WWhaleFe/flockswitch-linux/issues)로 알려 주세요.
>
> **Beta, and still needs re-checking.** The Linux version was last checked on a real PC at 0.12.0 (Ubuntu 26.04, GNOME 50, Wayland). Later versions have not been re-checked on real Linux hardware, and X11 (Xorg) sessions and KDE Plasma, sway, Hyprland and others have never been tried. It may not work as expected; please report problems as an [issue](https://github.com/WWhaleFe/flockswitch-linux/issues).


2.0은 로컬 네트워크의 내 컴퓨터끼리 마우스·키보드·클립보드를 공유합니다. GNOME Wayland는 입력 캡처와 원격 데스크톱 두 권한을 요청합니다. 2.0 기능은 아직 Linux 실기기에서 확인하지 않았습니다.

2.0 shares mouse, keyboard and clipboard between your own computers on the local network. GNOME Wayland asks for two permissions: input capture and remote desktop. 2.0 features have not yet been validated on real Linux hardware.
