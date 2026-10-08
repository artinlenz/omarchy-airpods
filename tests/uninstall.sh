#!/bin/bash
# SPDX-License-Identifier: MIT
set -euo pipefail

if (( EUID == 0 )); then
  echo 'Run removal tests as a non-root user, like the real uninstaller.' >&2
  exit 1
fi

uninstaller=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/uninstall.sh
sandbox=$(mktemp -d)
trap 'rm -rf -- "$sandbox"' EXIT
original_path=$PATH

fixture() {
  local name=$1
  export HOME="$sandbox/$name/home"
  export XDG_CONFIG_HOME="$sandbox/$name/config"
  export CALLS="$sandbox/$name/calls"
  export PATH="$sandbox/$name/bin:$original_path"
  binary="$HOME/.local/bin/airpodsd"
  config="$XDG_CONFIG_HOME/omarchy-airpods/config.json"
  unit="$XDG_CONFIG_HOME/systemd/user/omarchy-airpods.service"
  dropin="$XDG_CONFIG_HOME/systemd/user/omarchy-airpods.service.d/standalone.conf"
  plugin="$HOME/.config/omarchy/plugins/artinlenz.airpods/manifest.json"
  mkdir -p "${binary%/*}" "${config%/*}" "${dropin%/*}" "${plugin%/*}" "$sandbox/$name/bin"
  printf 'service fixture\n' >"$unit"
  printf 'dropin fixture\n' >"$dropin"
  printf '{}\n' >"$plugin"
  : >"$CALLS"
  for command in systemctl omarchy; do
    cat >"$sandbox/$name/bin/$command" <<'STUB'
#!/bin/bash
printf '%s %s\n' "${0##*/}" "$*" >>"$CALLS"
STUB
    chmod +x "$sandbox/$name/bin/$command"
  done
}

refuses_teardown() {
  cp "$config" "$sandbox/config-before"
  if bash "$uninstaller" >"$sandbox/stdout" 2>"$sandbox/stderr"; then
    echo 'Removal unexpectedly succeeded before guard release.' >&2
    exit 1
  fi
  [[ -f $unit && -f $dropin && -f $plugin && ! -s $CALLS ]]
  cmp "$config" "$sandbox/config-before"
}

allows_teardown() {
  bash "$uninstaller" >"$sandbox/stdout" 2>"$sandbox/stderr"
  [[ ! -e $unit && ! -e ${dropin%/*} && -f $plugin ]]
}

for original_blocked in false true; do
  fixture "managed-$original_blocked"
  printf '{"managed":true,"original_blocked":%s}\n' "$original_blocked" >"$config"
  refuses_teardown
  printf 'PASS: missing daemon retains managed guard state (original_blocked=%s)\n' "$original_blocked"
done

state_index=0
for state in '{"managed":null}' '{}' 'null' '{"managed":false' $'{"managed":true}\n{"managed":false}'; do
  state_index=$((state_index + 1))
  fixture "unknown-$state_index"
  printf '%s\n' "$state" >"$config"
  refuses_teardown
done
printf 'PASS: unknown, malformed and multiple configuration values prevent teardown\n'

fixture dangling-config
ln -s "$sandbox/missing-config" "$config"
if bash "$uninstaller" >"$sandbox/stdout" 2>"$sandbox/stderr"; then
  echo 'Removal unexpectedly succeeded with unreadable ownership state.' >&2
  exit 1
fi
[[ -f $unit && -f $dropin && -f $plugin && -L $config && ! -s $CALLS ]]
printf 'PASS: dangling configuration retains recovery artifacts\n'

fixture failed-release
printf '{"managed":true,"original_blocked":false}\n' >"$config"
printf '#!/bin/bash\nexit 7\n' >"$binary"
chmod +x "$binary"
refuses_teardown
[[ -x $binary ]]
printf 'PASS: failed release retains daemon, service and private state\n'

fixture released
printf '{"managed":false,"original_blocked":false,"keys":{"retained":true}}\n' >"$config"
cp "$config" "$sandbox/config-before"
allows_teardown
cmp "$config" "$sandbox/config-before"
printf 'PASS: confirmed released state allows cleanup without removing private data\n'

fixture never-enrolled
allows_teardown
[[ ! -e $config ]]
printf 'PASS: absent daemon and configuration allow never-enrolled cleanup\n'
