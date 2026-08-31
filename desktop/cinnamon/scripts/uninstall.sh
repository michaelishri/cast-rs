#!/usr/bin/env bash
set -euo pipefail

install_root=${DESTDIR:-}
if [[ -z "$install_root" && ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "Run this uninstaller with sudo, or set DESTDIR for a staged uninstall." >&2
  exit 1
fi

settings_dir="$install_root/usr/share/cinnamon/cinnamon-settings/modules"
schema_dir="$install_root/usr/share/glib-2.0/schemas"
icon_root="$install_root/usr/share/icons/hicolor"

rm -f "$settings_dir/cs_cast.py"
rm -f "$settings_dir/cast_common.py"
rm -f "$schema_dir/io.github.michaelishri.cast.gschema.xml"
rm -f "$icon_root/scalable/apps/cast-symbolic.svg"
rm -rf "$install_root/usr/share/cinnamon/applets/cast@cast-rs"

if [[ -z "$install_root" ]]; then
  glib-compile-schemas "$schema_dir"
  gtk-update-icon-cache -f -t "$icon_root" >/dev/null 2>&1 || true
  echo "Removed Cinnamon Cast integration. Log out and back in if the applet is still visible."
else
  echo "Removed staged Cinnamon Cast integration from $install_root"
fi
