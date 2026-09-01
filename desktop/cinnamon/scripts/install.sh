#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source_dir=$(cd -- "$script_dir/.." && pwd)
install_root=${DESTDIR:-}

if [[ -z "$install_root" && ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "Run this installer with sudo, or set DESTDIR for a staged install." >&2
  exit 1
fi

settings_dir="$install_root/usr/share/cinnamon/cinnamon-settings/modules"
applet_dir="$install_root/usr/share/cinnamon/applets/cast@cast-rs"
schema_dir="$install_root/usr/share/glib-2.0/schemas"
icon_dir="$install_root/usr/share/icons/hicolor/scalable/apps"

install -d "$settings_dir" "$applet_dir" "$schema_dir" "$icon_dir"
install -m 0644 "$source_dir/settings/cs_cast.py" "$settings_dir/cs_cast.py"
install -m 0644 "$source_dir/settings/cast_common.py" "$settings_dir/cast_common.py"
install -m 0644 "$source_dir/schemas/io.github.michaelishri.cast.gschema.xml" "$schema_dir/io.github.michaelishri.cast.gschema.xml"
install -m 0644 "$source_dir/icons/cast-symbolic.svg" "$icon_dir/cast-symbolic.svg"
cp -R "$source_dir/applet/cast@cast-rs/." "$applet_dir/"
chmod -R a+rX "$applet_dir"

if [[ -z "$install_root" ]]; then
  glib-compile-schemas "$schema_dir"
  gtk-update-icon-cache -f -t "$install_root/usr/share/icons/hicolor" >/dev/null 2>&1 || true
  echo "Installed Cinnamon Cast integration. Open it with: cinnamon-settings cast"
else
  echo "Staged Cinnamon Cast integration under $install_root"
fi
