#!/bin/bash
# SPDX-License-Identifier: MIT
set -euo pipefail

if (( EUID == 0 )); then
  echo 'Run this as your desktop user, not root.' >&2
  exit 1
fi

config_home=${XDG_CONFIG_HOME:-$HOME/.config}
# Release first: do not strand a device blocked if BlueZ is unavailable.
"$HOME/.local/bin/airpodsd" release
systemctl --user disable --now omarchy-airpods.service
omarchy plugin disable artinlenz.airpods
rm -f "$config_home/systemd/user/omarchy-airpods.service" "$HOME/.local/bin/airpodsd"
systemctl --user daemon-reload
printf 'Service removed and its device guard released. Pairing was not removed.\n'
printf 'Remove the plugin checkout with: omarchy plugin remove artinlenz.airpods\n'
printf 'Private setup data, if retained by release, lives under %s/omarchy-airpods/.\n' "$config_home"
