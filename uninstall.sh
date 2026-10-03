#!/bin/bash
# SPDX-License-Identifier: MIT
set -euo pipefail

if (( EUID == 0 )); then
  echo 'Run this as your desktop user, not root.' >&2
  exit 1
fi

config_home=${XDG_CONFIG_HOME:-$HOME/.config}
binary="$HOME/.local/bin/airpodsd"
unit_dir="$config_home/systemd/user"
config_file="$config_home/omarchy-airpods/config.json"

# Release first: do not strand a device blocked if BlueZ is unavailable.
# A failed release stops here on purpose; only a missing binary continues.
guard_note='Service removed and its device guard released. Pairing was not removed.'
if [[ -e $binary ]]; then
  "$binary" release
elif [[ -f $config_file ]] && jq -e '.managed == true and .original_blocked == false' "$config_file" >/dev/null; then
  address=$(jq -r '.address' "$config_file")
  printf '%s is missing, so the device guard cannot be released automatically.\n' "$binary" >&2
  printf 'Unblock your AirPods with: bluetoothctl unblock %s\n' "$address" >&2
  guard_note='Service removed. Your AirPods stay blocked until you run the bluetoothctl command above.'
fi
systemctl --user disable --now omarchy-airpods.service
omarchy plugin disable artinlenz.airpods
rm -f "$unit_dir/omarchy-airpods.service" "$binary"
rm -rf -- "$unit_dir/omarchy-airpods.service.d"
systemctl --user daemon-reload
printf '%s\n' "$guard_note"
printf 'Remove the plugin checkout with: omarchy plugin remove artinlenz.airpods\n'
printf 'Private setup data, if retained by release, lives under %s/omarchy-airpods/.\n' "$config_home"
