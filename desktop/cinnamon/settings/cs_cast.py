#!/usr/bin/python3

import os
import shlex
import shutil
import subprocess

import gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gio, GLib, Gtk
from SettingsWidgets import SidePage
from xapp.GSettingsWidgets import *

from cast_common import (
    CAST_SCHEMA,
    CINNAMON_SCHEMA,
    disable_applet,
    enable_applet,
    is_applet_enabled,
)


class DiagnosticRow(SettingsWidget):
    def __init__(self, cast_settings):
        super().__init__()
        self.cast_settings = cast_settings
        self.label = SettingsLabel("")
        self.label.set_hexpand(True)
        self.button = Gtk.Button.new_with_label(_("Refresh"))
        self.button.set_valign(Gtk.Align.CENTER)
        self.button.connect("clicked", self.refresh)
        self.pack_start(self.label, True, True, 0)
        self.pack_end(self.button, False, False, 0)
        self.refresh()

    def refresh(self, *_args):
        configured = self.cast_settings.get_string("cast-command").strip() or "cast"
        try:
            command = shlex.split(configured)
        except ValueError:
            command = []

        executable = shutil.which(command[0]) if command else None
        if executable is None and command and os.path.isabs(command[0]):
            executable = command[0] if os.access(command[0], os.X_OK) else None

        if executable is None:
            self.label.set_markup(_("<b>Cast CLI:</b> not found"))
            self.set_tooltip_text(_("Install cast or set the full executable path above."))
            return

        try:
            result = subprocess.run(
                [executable, "--version"],
                capture_output=True,
                check=False,
                text=True,
                timeout=2,
            )
            version = (result.stdout or result.stderr).strip().splitlines()[0]
        except (OSError, subprocess.SubprocessError, IndexError):
            version = _("available")

        self.label.set_markup(
            _("<b>Cast CLI:</b> {} ({})").format(
                GLib.markup_escape_text(version), GLib.markup_escape_text(executable)
            )
        )
        self.set_tooltip_text(executable)


class Module:
    name = "cast"
    comment = _("Configure Google Cast desktop streaming")
    category = "prefs"

    def __init__(self, content_box):
        keywords = _("cast, chromecast, google tv, screen, mirror, desktop")
        self.sidePage = SidePage(_("Cast"), "cast-symbolic", keywords, content_box, module=self)
        self._syncing_applet = False
        self._signal_ids = []

    def on_module_selected(self):
        if self.loaded:
            return

        self.cast_settings = Gio.Settings.new(CAST_SCHEMA)
        self.cinnamon_settings = Gio.Settings.new(CINNAMON_SCHEMA)
        self._sync_applet_setting_from_cinnamon()
        self._signal_ids.append(
            self.cast_settings.connect("changed::applet-enabled", self._on_applet_setting_changed)
        )
        self._signal_ids.append(
            self.cinnamon_settings.connect("changed::enabled-applets", self._on_enabled_applets_changed)
        )

        page = SettingsPage()
        self.sidePage.add_widget(page)

        integration = page.add_section(_("Desktop integration"))
        integration.add_row(
            GSettingsSwitch(
                _("Show the Cast applet in the panel"), CAST_SCHEMA, "applet-enabled"
            )
        )
        integration.add_row(
            GSettingsEntry(
                _("Cast executable"),
                CAST_SCHEMA,
                "cast-command",
                expand_width=True,
                tooltip=_("Command name or absolute path for the cast CLI."),
            )
        )
        integration.add_row(DiagnosticRow(self.cast_settings))

        defaults = page.add_section(_("Casting defaults"))
        defaults.add_row(
            GSettingsComboBox(
                _("Desktop mode"),
                CAST_SCHEMA,
                "default-mode",
                [("mirror", _("Mirror desktop")), ("extend", _("Extended desktop"))],
                valtype=str,
            )
        )
        defaults.add_row(GSettingsSwitch(_("Include audio"), CAST_SCHEMA, "audio"))
        defaults.add_row(
            GSettingsComboBox(
                _("Streaming transport"),
                CAST_SCHEMA,
                "transport",
                [("mirror", _("Low-latency mirror")), ("hls", _("HLS"))],
                valtype=str,
            )
        )
        defaults.add_row(
            GSettingsComboBox(
                _("Capture backend"),
                CAST_SCHEMA,
                "backend",
                [("auto", _("Automatic")), ("x11", _("X11"))],
                valtype=str,
            )
        )
        defaults.add_row(
            GSettingsComboBox(
                _("Video encoder"),
                CAST_SCHEMA,
                "encoder",
                [
                    ("auto", _("Automatic")),
                    ("nvenc", _("NVIDIA NVENC")),
                    ("vaapi", _("VA-API")),
                    ("openh264", _("OpenH264")),
                ],
                valtype=str,
            )
        )

        video = page.add_section(_("Video quality"))
        video.add_row(GSettingsSpinButton(_("Width"), CAST_SCHEMA, "width", _("pixels")))
        video.add_row(GSettingsSpinButton(_("Height"), CAST_SCHEMA, "height", _("pixels")))
        video.add_row(GSettingsSpinButton(_("Frame rate"), CAST_SCHEMA, "fps", _("fps")))
        video.add_row(GSettingsSpinButton(_("Bitrate"), CAST_SCHEMA, "bitrate", _("bits/s"), step=100000, page=1000000))
        video.add_row(GSettingsSpinButton(_("Target delay"), CAST_SCHEMA, "target-delay-ms", _("ms"), step=10, page=100))
        video.add_row(GSettingsSpinButton(_("Discovery timeout"), CAST_SCHEMA, "discovery-timeout", _("seconds")))

        self.loaded = True

    def _on_applet_setting_changed(self, *_args):
        if self._syncing_applet:
            return

        entries = self.cinnamon_settings.get_strv("enabled-applets")
        should_enable = self.cast_settings.get_boolean("applet-enabled")
        if should_enable:
            panels = self.cinnamon_settings.get_strv("panels-enabled")
            next_id = self.cinnamon_settings.get_int("next-applet-id")
            updated, new_next_id = enable_applet(entries, panels, next_id)
            if updated != entries:
                self.cinnamon_settings.set_strv("enabled-applets", updated)
                self.cinnamon_settings.set_int("next-applet-id", new_next_id)
        else:
            updated = disable_applet(entries)
            if updated != entries:
                self.cinnamon_settings.set_strv("enabled-applets", updated)

    def _on_enabled_applets_changed(self, *_args):
        self._sync_applet_setting_from_cinnamon()

    def _sync_applet_setting_from_cinnamon(self):
        enabled = is_applet_enabled(self.cinnamon_settings.get_strv("enabled-applets"))
        if self.cast_settings.get_boolean("applet-enabled") == enabled:
            return
        self._syncing_applet = True
        try:
            self.cast_settings.set_boolean("applet-enabled", enabled)
        finally:
            self._syncing_applet = False
