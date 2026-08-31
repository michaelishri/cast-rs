const Applet = imports.ui.applet;
const Gio = imports.gi.Gio;
const GLib = imports.gi.GLib;
const Main = imports.ui.main;
const PopupMenu = imports.ui.popupMenu;
const Util = imports.misc.util;

const CAST_SCHEMA = "io.github.michaelishri.cast";
const SIGINT = 2;


class CastApplet extends Applet.IconApplet {
    constructor(metadata, orientation, panelHeight, instanceId) {
        super(orientation, panelHeight, instanceId);

        this.metadata = metadata;
        imports.searchPath.unshift(metadata.path);
        this.Command = imports.command;
        this._settings = new Gio.Settings({schema_id: CAST_SCHEMA});
        this._activeProcess = null;
        this._activeDevice = null;
        this._discoveryProcess = null;
        this._discoveryGeneration = 0;
        this._lastDiscoveryTime = 0;
        this._sessionGeneration = 0;
        this._stopping = false;
        this._removed = false;

        this.set_applet_icon_symbolic_name("cast-symbolic");
        this.set_applet_tooltip(_("Cast desktop"));

        this.menuManager = new PopupMenu.PopupMenuManager(this);
        this.menu = new Applet.AppletPopupMenu(this, orientation);
        this.menuManager.addMenu(this.menu);
        this.menu.connect("open-state-changed", (_menu, isOpen) => {
            if (isOpen)
                this._maybeDiscoverDevices();
        });

        this._statusItem = new PopupMenu.PopupMenuItem(_("Open to search for devices"), {
            reactive: false,
            style_class: "cast-status-item",
        });
        this.menu.addMenuItem(this._statusItem);
        this._deviceSection = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._deviceSection);

        this._refreshItem = new PopupMenu.PopupMenuItem(_("Refresh devices"));
        this._refreshItem.connect("activate", () => this._discoverDevices());
        this.menu.addMenuItem(this._refreshItem);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._audioItem = new PopupMenu.PopupSwitchMenuItem(
            _("Include audio"), this._settings.get_boolean("audio")
        );
        this._audioItem.connect("toggled", () => {
            this._settings.set_boolean("audio", this._audioItem.state);
        });
        this._audioSignal = this._settings.connect("changed::audio", () => {
            this._audioItem.setToggleState(this._settings.get_boolean("audio"));
        });
        this.menu.addMenuItem(this._audioItem);

        this._sessionSection = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._sessionSection);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        let settingsItem = new PopupMenu.PopupMenuItem(_("Cast settings"));
        settingsItem.connect("activate", () => Util.spawnCommandLine("cinnamon-settings cast"));
        this.menu.addMenuItem(settingsItem);
        this._renderSession();
    }

    on_applet_clicked() {
        this.menu.toggle();
    }

    on_applet_removed_from_panel() {
        this._removed = true;
        this._cancelDiscovery();
        this._stopSession(false);
        if (this._audioSignal)
            this._settings.disconnect(this._audioSignal);
    }

    _castExecutable() {
        return this._settings.get_string("cast-command").trim() || "cast";
    }

    _maybeDiscoverDevices() {
        let age = GLib.get_monotonic_time() - this._lastDiscoveryTime;
        if (this._lastDiscoveryTime === 0 || age > 10000000)
            this._discoverDevices();
    }

    _discoverDevices() {
        this._cancelDiscovery();
        let generation = ++this._discoveryGeneration;
        this._statusItem.label.set_text(_("Searching for Cast devices…"));
        this._refreshItem.setSensitive(false);
        this._deviceSection.removeAll();

        let argv = this.Command.buildDiscoveryCommand(
            this._castExecutable(), this._settings.get_int("discovery-timeout")
        );
        try {
            this._discoveryProcess = Util.spawnCommandLineAsyncIO("", (stdout, stderr, exitCode) => {
                if (generation !== this._discoveryGeneration || this._removed)
                    return;
                this._discoveryProcess = null;
                this._refreshItem.setSensitive(true);
                if (exitCode !== 0) {
                    this._showDiscoveryError((stderr || _("Device discovery failed")).trim());
                    return;
                }

                let devices;
                try {
                    devices = JSON.parse(stdout || "[]");
                } catch (error) {
                    this._showDiscoveryError(_("Cast returned invalid device data"));
                    global.logError(`Cast applet could not parse discovery output: ${error}`);
                    return;
                }
                this._lastDiscoveryTime = GLib.get_monotonic_time();
                this._renderDevices(devices);
            }, {argv});
        } catch (error) {
            this._discoveryProcess = null;
            this._refreshItem.setSensitive(true);
            this._showDiscoveryError(error.message);
        }
    }

    _cancelDiscovery() {
        ++this._discoveryGeneration;
        if (this._discoveryProcess !== null) {
            try {
                if (this._discoveryProcess.cancellable)
                    this._discoveryProcess.cancellable.cancel();
                this._discoveryProcess.force_exit();
            } catch (_error) {
                // The process may have exited between the null check and cancellation.
            }
            this._discoveryProcess = null;
        }
    }

    _showDiscoveryError(message) {
        this._statusItem.label.set_text(_("Could not discover devices"));
        let item = new PopupMenu.PopupMenuItem(message || _("Unknown error"), {reactive: false});
        this._deviceSection.addMenuItem(item);
    }

    _renderDevices(devices) {
        this._deviceSection.removeAll();
        if (!Array.isArray(devices) || devices.length === 0) {
            this._statusItem.label.set_text(_("No Cast devices found"));
            return;
        }

        devices.sort((left, right) => left.name.localeCompare(right.name));
        this._statusItem.label.set_text(
            devices.length === 1 ? _("1 Cast device") : _("%d Cast devices").format(devices.length)
        );
        for (let device of devices) {
            let title = device.name || device.address;
            if (device.model)
                title += ` — ${device.model}`;
            let target = new PopupMenu.PopupSubMenuMenuItem(title);

            if (device.capability === "audio_only") {
                target.setSensitive(false);
                target.label.set_text(`${title} (${_("audio only")})`);
            } else {
                let mirror = new PopupMenu.PopupMenuItem(_("Mirror desktop"));
                mirror.connect("activate", () => this._startSession(device, "mirror"));
                mirror.setSensitive(this._activeProcess === null);
                target.menu.addMenuItem(mirror);

                let extend = new PopupMenu.PopupMenuItem(_("Extended desktop"));
                extend.connect("activate", () => this._startSession(device, "extend"));
                extend.setSensitive(this._activeProcess === null);
                target.menu.addMenuItem(extend);
            }
            this._deviceSection.addMenuItem(target);
        }
    }

    _desktopOptions(mode) {
        return {
            mode,
            audio: this._settings.get_boolean("audio"),
            backend: this._settings.get_string("backend"),
            transport: this._settings.get_string("transport"),
            encoder: this._settings.get_string("encoder"),
            width: this._settings.get_int("width"),
            height: this._settings.get_int("height"),
            fps: this._settings.get_int("fps"),
            bitrate: this._settings.get_int("bitrate"),
            targetDelayMs: this._settings.get_int("target-delay-ms"),
        };
    }

    _startSession(device, mode) {
        if (this._activeProcess !== null)
            return;

        let argv = this.Command.buildDesktopCommand(
            this._castExecutable(), device, this._desktopOptions(mode), GLib.getpid()
        );
        let generation = ++this._sessionGeneration;
        this._stopping = false;
        try {
            this._activeProcess = Gio.Subprocess.new(
                argv, Gio.SubprocessFlags.STDOUT_SILENCE | Gio.SubprocessFlags.STDERR_PIPE
            );
            this._activeDevice = device;
            this._renderSession();
            this._activeProcess.communicate_utf8_async(null, null, (process, result) => {
                let stderr = "";
                try {
                    let output = process.communicate_utf8_finish(result);
                    stderr = output[2] || "";
                } catch (error) {
                    stderr = error.message;
                }
                if (generation !== this._sessionGeneration)
                    return;

                let stopped = this._stopping;
                let successful = false;
                try {
                    successful = process.get_successful();
                } catch (_error) {
                    successful = false;
                }
                this._activeProcess = null;
                this._activeDevice = null;
                this._stopping = false;
                this._renderSession();
                if (!stopped && !successful && !this._removed) {
                    let message = (stderr || _("Desktop casting stopped unexpectedly")).trim();
                    Main.notifyError(_("Cast failed"), message.slice(-500));
                }
            });
        } catch (error) {
            this._activeProcess = null;
            this._activeDevice = null;
            this._renderSession();
            Main.notifyError(_("Could not start Cast"), error.message);
        }
    }

    _stopSession(render = true) {
        if (this._activeProcess === null)
            return;
        this._stopping = true;
        try {
            this._activeProcess.send_signal(SIGINT);
        } catch (error) {
            global.logError(`Cast applet could not stop desktop session: ${error}`);
            this._activeProcess.force_exit();
        }
        if (render)
            this._renderSession();
    }

    _renderSession() {
        this._sessionSection.removeAll();
        if (this._activeProcess === null) {
            this.set_applet_tooltip(_("Cast desktop"));
            return;
        }

        let name = this._activeDevice.name || this._activeDevice.address;
        let text = this._stopping ? _("Stopping cast…") : _("Casting to %s").format(name);
        this.set_applet_tooltip(text);
        this._sessionSection.addMenuItem(
            new PopupMenu.PopupMenuItem(text, {reactive: false, style_class: "cast-status-item"})
        );
        let stop = new PopupMenu.PopupMenuItem(_("Stop casting"));
        stop.connect("activate", () => this._stopSession());
        stop.setSensitive(!this._stopping);
        this._sessionSection.addMenuItem(stop);
    }
}


function main(metadata, orientation, panelHeight, instanceId) {
    return new CastApplet(metadata, orientation, panelHeight, instanceId);
}
