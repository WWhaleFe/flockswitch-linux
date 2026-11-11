#!/usr/bin/env bash
# Installs what the build needs (libudev-dev, pkg-config): nothing when the runner image
# has them, else the .deb copies in .github/debs, else apt with retries and a second
# mirror, so an outage of the Ubuntu archive doesn't fail the build.
set -u

need=""
for p in libudev-dev pkg-config; do
  dpkg -s "$p" >/dev/null 2>&1 || need="$need $p"
done
if [ -z "$need" ]; then
  echo "build dependencies already installed"
  exit 0
fi

# First the copies kept in this repository (.github/debs, from the Ubuntu 22.04 archive),
# so a build needs no package server. --force-depends: libudev-dev pins the exact libudev1
# version, and any libudev1 of the same soname is enough to build against.
arch=$(dpkg --print-architecture)
debs=()
for p in $need; do
  for f in .github/debs/${p}_*_"$arch".deb; do [ -e "$f" ] && debs+=("$f"); done
done
if [ ${#debs[@]} -gt 0 ] && sudo dpkg -i --force-depends "${debs[@]}" && pkg-config --exists libudev; then
  echo "installed from .github/debs:${need}"
  exit 0
fi
echo "repository copies did not work; trying apt"

opts=(-o Acquire::Retries=5 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30)
codename=$(. /etc/os-release && echo "$VERSION_CODENAME")
if [ "$(dpkg --print-architecture)" = amd64 ]; then
  mirror=http://mirrors.edge.kernel.org/ubuntu
else
  mirror=http://ports.ubuntu.com/ubuntu-ports
fi

for try in 1 2; do
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
