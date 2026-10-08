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
# A failed release or unknown ownership state stops before any teardown.
if [[ -e $binary ]]; then
  "$binary" release
elif [[ -e $config_file || -L $config_file ]]; then
  if ! jq -e -s 'length == 1 and (.[0] | type == "object" and .managed == false)' "$config_file" >/dev/null 2>&1; then
    printf '%s is missing, and device-guard release cannot be confirmed.\n' "$binary" >&2
    printf 'Restore the daemon by running install.sh, then rerun this uninstaller.\n' >&2
    printf 'The service, plugin and private setup data have not been removed.\n' >&2
    exit 1
  fi
fi
systemctl --user disable --now omarchy-airpods.service
omarchy plugin disable artinlenz.airpods
rm -f "$unit_dir/omarchy-airpods.service" "$binary"
rm -rf -- "$unit_dir/omarchy-airpods.service.d"
systemctl --user daemon-reload
printf 'Service removed. No device guard remains owned by this plugin. Pairing was not removed.\n'
printf 'Remove the plugin checkout with: omarchy plugin remove artinlenz.airpods\n'
printf 'Private setup data, if retained by release, lives under %s/omarchy-airpods/.\n' "$config_home"
