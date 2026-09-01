"""Pure helpers shared by the Cinnamon Cast settings module and its tests."""

APPLET_UUID = "cast@cast-rs"
CAST_SCHEMA = "io.github.michaelishri.cast"
CINNAMON_SCHEMA = "org.cinnamon"


def applet_uuid(entry):
    """Return the UUID from a Cinnamon enabled-applets entry."""
    parts = entry.split(":")
    return parts[3] if len(parts) >= 4 else None


def is_applet_enabled(entries):
    return any(applet_uuid(entry) == APPLET_UUID for entry in entries)


def disable_applet(entries):
    return [entry for entry in entries if applet_uuid(entry) != APPLET_UUID]


def preferred_panel(panels_enabled):
    """Choose the first configured panel, falling back to Cinnamon's panel 1."""
    if not panels_enabled:
        return "panel1"

    panel_id = panels_enabled[0].split(":", 1)[0]
    if panel_id.startswith("panel"):
        return panel_id
    if panel_id.isdigit():
        return "panel" + panel_id
    return "panel1"


def enable_applet(entries, panels_enabled, next_applet_id):
    """Add the Cast applet to the right side of the preferred panel."""
    if is_applet_enabled(entries):
        return list(entries), next_applet_id

    panel = preferred_panel(panels_enabled)
    positions = []
    for entry in entries:
        parts = entry.split(":")
        if len(parts) >= 3 and parts[0] == panel and parts[1] == "right":
            try:
                positions.append(int(parts[2]))
            except ValueError:
                pass

    position = max(positions, default=-1) + 1
    new_entry = f"{panel}:right:{position}:{APPLET_UUID}:{next_applet_id}"
    return [*entries, new_entry], next_applet_id + 1
