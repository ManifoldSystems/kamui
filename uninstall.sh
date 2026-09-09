#!/bin/sh
# Removes the kamui binary installed by install.sh.
# Usage: uninstall.sh [--purge]
#   --purge   Also remove configuration (kamui.toml, themes) and the
#             SQLite database. Without it, settings survive a reinstall.
set -eu

purge=0
for arg in "$@"; do
  case "$arg" in
    --purge) purge=1 ;;
    -h|--help)
      echo "Usage: uninstall.sh [--purge]"
      exit 0
      ;;
    *) echo "Unknown argument: $arg (see --help)" >&2; exit 1 ;;
  esac
done

install_dir="${KAMUI_INSTALL_DIR:-$HOME/.local/bin}"

printf '\n  KAMUI uninstall\n\n'

if [ -e "$install_dir/kamui" ]; then
  rm -f "$install_dir/kamui"
  printf '  Removed %s/kamui\n' "$install_dir"
else
  printf '  No binary at %s/kamui (nothing to remove)\n' "$install_dir"
fi

if [ "$purge" -eq 1 ]; then
  os=$(uname -s)
  case "$os" in
    Darwin)
      config_dir="$HOME/Library/Application Support/kamui"
      data_dir="$HOME/Library/Application Support/kamui"
      ;;
    *)
      config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/kamui"
      if [ -n "${KAMUI_DATA_DIR:-}" ]; then
        data_dir="$KAMUI_DATA_DIR"
      else
        data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/kamui"
      fi
      ;;
  esac
  for dir in "$config_dir" "$data_dir"; do
    if [ -d "$dir" ]; then
      rm -rf "$dir"
      printf '  Purged %s\n' "$dir"
    fi
  done
else
  printf '  Kept configuration and database (use --purge to remove them)\n'
fi

printf '\n  Done.\n\n'
