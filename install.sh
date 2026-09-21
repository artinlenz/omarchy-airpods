#!/bin/bash
# SPDX-License-Identifier: MIT
set -euo pipefail

backend_only=false
case ${1:-} in
  --backend-only) backend_only=true ;;
  "") ;;
  *) printf 'Usage: %s [--backend-only]\n' "$0" >&2; exit 2 ;;
esac

if (( EUID == 0 )); then
  echo 'Run this installer as your desktop user, not root.' >&2
  exit 1
fi

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
cache_home=${XDG_CACHE_HOME:-$HOME/.cache}

if [[ $backend_only == false && $root != "$HOME/.config/omarchy/plugins/artinlenz.airpods" ]]; then
  echo 'First install this repository with omarchy plugin add, then run install.sh from its installed directory.' >&2
  echo 'Use --backend-only to install just the background service from a development checkout.' >&2
  exit 1
fi

cargo build --release --locked --manifest-path "$root/backend/Cargo.toml" --target-dir "$cache_home/omarchy-airpods/target"
install -Dm755 "$cache_home/omarchy-airpods/target/release/airpodsd" "$HOME/.local/bin/airpodsd"
install -Dm644 "$root/packaging/omarchy-airpods.service" "$config_home/systemd/user/omarchy-airpods.service"
systemctl --user daemon-reload
systemctl --user enable omarchy-airpods.service
systemctl --user restart omarchy-airpods.service

if [[ $backend_only == false ]]; then
  omarchy plugin enable artinlenz.airpods --section right
fi
printf 'AirPods service installed. Use the panel to select your paired AirPods and run one-time setup.\n'
