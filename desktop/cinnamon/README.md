# Cinnamon desktop integration

This directory contains the Cinnamon System Settings module and panel applet for Cast. It targets Cinnamon on X11; Wayland support is intentionally out of scope for this first integration.

## Install

Build and install the `cast` CLI first, then install the desktop integration:

```sh
sudo desktop/cinnamon/scripts/install.sh
cinnamon-settings cast
```

The installer places the settings module, applet, GSettings schema, and icon in their standard system locations. The **Show the Cast applet in the panel** switch updates Cinnamon's `enabled-applets` setting and stays synchronized when the applet is added or removed elsewhere.

If `cast` is not on the graphical session's `PATH`, enter its absolute path in Cast settings.

## Uninstall

Disable the applet in Cast settings first, then run:

```sh
sudo desktop/cinnamon/scripts/uninstall.sh
```

## Development checks

```sh
PYTHONPATH=desktop/cinnamon/settings python3 -m unittest discover -s desktop/cinnamon/tests
python3 -m py_compile desktop/cinnamon/settings/*.py
bash -n desktop/cinnamon/scripts/*.sh
glib-compile-schemas --strict --dry-run desktop/cinnamon/schemas
```

Set `DESTDIR` to exercise the installer without modifying the host system.
