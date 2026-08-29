#!/usr/bin/env sh
set -eu

destination="${KERNARY_INSTALL_DIR:-${HARNESS_INSTALL_DIR:-$HOME/.local/bin}}"
mode="${1:-install}"
case "$destination" in
  /|"") echo "拒绝把 Kernary 安装到文件系统根目录。" >&2; exit 1 ;;
esac
primary_target="$destination/kernary"
compatibility_target="$destination/harness"
rollback_directory="$destination/rollback"
mkdir -p "$destination" "$rollback_directory"

verify_set() {
  directory=$1
  found=0
  if [ -f "$directory/kernary" ]; then chmod 0755 "$directory/kernary"; "$directory/kernary" --version >/dev/null; found=1; fi
  if [ -f "$directory/harness" ]; then chmod 0755 "$directory/harness"; "$directory/harness" --version >/dev/null; found=1; fi
  [ "$found" -eq 1 ]
}

move_current_set() {
  target_directory=$1
  mkdir -p "$target_directory"
  [ ! -f "$primary_target" ] || mv "$primary_target" "$target_directory/kernary"
  [ ! -f "$compatibility_target" ] || mv "$compatibility_target" "$target_directory/harness"
}

move_set_to_destination() {
  source_directory=$1
  [ ! -f "$source_directory/kernary" ] || mv "$source_directory/kernary" "$primary_target"
  [ ! -f "$source_directory/harness" ] || mv "$source_directory/harness" "$compatibility_target"
}

if [ "$mode" = "--rollback" ]; then
  previous=$(find "$rollback_directory" -mindepth 1 -maxdepth 1 -type d -name 'set-*' -print | sort -r | head -n 1)
  if [ -z "$previous" ]; then echo "没有可回滚的 Kernary binary set。" >&2; exit 1; fi
  swap="$destination/rollback-swap-$$"
  move_current_set "$swap"
  if move_set_to_destination "$previous" && verify_set "$destination"; then
    stamp=$(date +%s)-$$
    if [ -f "$swap/kernary" ] || [ -f "$swap/harness" ]; then
      mv "$swap" "$rollback_directory/set-$stamp-current"
    else
      rmdir "$swap"
    fi
    rmdir "$previous" 2>/dev/null || true
    echo "Kernary rollback complete: $destination"
    exit 0
  fi
  rm -f "$primary_target" "$compatibility_target"
  move_set_to_destination "$swap"
  exit 1
fi

package_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
primary_source="$package_root/bin/kernary"
compatibility_source="$package_root/bin/harness"
if [ ! -f "$primary_source" ] || [ ! -f "$compatibility_source" ]; then
  echo "发布包缺少 kernary/harness binary set" >&2
  exit 1
fi
staging="$destination/install-staging-$$"
mkdir -p "$staging"
cp "$primary_source" "$staging/kernary"
cp "$compatibility_source" "$staging/harness"
verify_set "$staging"

stamp=$(date +%s)-$$
previous_set="$rollback_directory/set-$stamp"
move_current_set "$previous_set"
if move_set_to_destination "$staging" && verify_set "$destination"; then
  rmdir "$staging" 2>/dev/null || true
  rmdir "$previous_set" 2>/dev/null || true
  echo "Kernary installed: $primary_target"
  echo "Harness compatibility alias: $compatibility_target"
  echo "Add to PATH if needed: $destination"
  exit 0
fi
rm -f "$primary_target" "$compatibility_target"
move_set_to_destination "$previous_set"
rm -f "$staging/kernary" "$staging/harness"
rmdir "$staging" 2>/dev/null || true
exit 1
