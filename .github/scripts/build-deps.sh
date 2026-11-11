#!/usr/bin/env bash
# Installs what the build needs (libudev-dev, pkg-config). Skips apt when the runner
# image already has them, and otherwise retries with timeouts and a second mirror, so a
# short outage of the Ubuntu archive doesn't fail the build.
set -u

need=""
for p in libudev-dev pkg-config; do
  dpkg -s "$p" >/dev/null 2>&1 || need="$need $p"
done
if [ -z "$need" ]; then
  echo "build dependencies already installed"
  exit 0
fi

opts=(-o Acquire::Retries=5 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30)
codename=$(. /etc/os-release && echo "$VERSION_CODENAME")
if [ "$(dpkg --print-architecture)" = amd64 ]; then
  mirror=http://mirrors.edge.kernel.org/ubuntu
else
  mirror=http://ports.ubuntu.com/ubuntu-ports
fi

for try in 1 2 3 4; do
  sudo apt-get "${opts[@]}" update || true
  # shellcheck disable=SC2086
  if sudo apt-get "${opts[@]}" install -y --no-install-recommends $need; then
    exit 0
  fi
  echo "apt failed (try $try); adding $mirror and trying again"
  printf 'deb %s %s main universe\ndeb %s %s-updates main universe\ndeb %s %s-security main universe\n' \
    "$mirror" "$codename" "$mirror" "$codename" "$mirror" "$codename" |
    sudo tee /etc/apt/sources.list.d/fallback-mirror.list >/dev/null
  sleep $((try * 20))
done
echo "could not install:$need"
exit 1
