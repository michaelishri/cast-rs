use std::env;

use anyhow::{Context, Result, anyhow, bail};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as _, xproto::ConnectionExt as _},
    rust_connection::RustConnection,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum BackendPreference {
    #[default]
    Auto,
    X11,
    Portal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    X11,
    Portal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Monitor {
    pub(crate) name: String,
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) primary: bool,
}

impl Monitor {
    pub(crate) fn geometry(&self) -> String {
        format!("{}x{}{:+}{:+}", self.width, self.height, self.x, self.y)
    }
}

pub(crate) struct DisplayConnection {
    pub(crate) connection: RustConnection,
    pub(crate) screen_number: usize,
}

impl DisplayConnection {
    pub(crate) fn connect() -> Result<Self> {
        let display = env::var("DISPLAY").map_err(|_| {
            anyhow!("DISPLAY is not set; run cast inside the logged-in X11 desktop session or pass --backend portal")
        })?;
        let (connection, screen_number) = RustConnection::connect(Some(&display)).with_context(|| {
            format!(
                "could not connect to X11 display {display}; check DISPLAY and Xauthority access"
            )
        })?;
        Ok(Self {
            connection,
            screen_number,
        })
    }

    pub(crate) fn root(&self) -> u32 {
        self.connection.setup().roots[self.screen_number].root
    }

    pub(crate) fn monitors(&self) -> Result<Vec<Monitor>> {
        self.connection
            .randr_query_version(1, 5)
            .context("could not query the X11 RandR extension")?
            .reply()
            .context("the X11 server does not provide RandR monitor discovery")?;
        let reply = self
            .connection
            .randr_get_monitors(self.root(), true)
            .context("could not request active X11 RandR monitors")?
            .reply()
            .context("could not read active X11 RandR monitors")?;
        let mut monitors = Vec::with_capacity(reply.monitors.len());
        for monitor in reply.monitors {
            let name = self
                .connection
                .get_atom_name(monitor.name)
                .context("could not request an X11 monitor name")?
                .reply()
                .context("could not read an X11 monitor name")?;
            let name = String::from_utf8(name.name)
                .context("an X11 RandR monitor name was not valid UTF-8")?;
            monitors.push(Monitor {
                name,
                x: monitor.x,
                y: monitor.y,
                width: monitor.width,
                height: monitor.height,
                primary: monitor.primary,
            });
        }
        if monitors.is_empty() {
            bail!("the X11 server reported no active RandR monitors");
        }
        monitors
            .sort_by_key(|monitor| (!monitor.primary, monitor.y, monitor.x, monitor.name.clone()));
        Ok(monitors)
    }

    pub(crate) fn select_monitor(&self, requested: Option<&str>) -> Result<Monitor> {
        let monitors = self.monitors()?;
        match requested {
            Some(name) => monitors
                .into_iter()
                .find(|monitor| monitor.name == name)
                .ok_or_else(|| anyhow!(
                    "X11 monitor {name:?} was not found; run `cast displays --backend x11` to list active monitors"
                )),
            None => monitors
                .iter()
                .find(|monitor| monitor.primary)
                .or_else(|| monitors.first())
                .cloned()
                .ok_or_else(|| anyhow!("the X11 server reported no active monitors")),
        }
    }
}

pub(crate) fn resolve_backend(preference: BackendPreference) -> Result<Backend> {
    match preference {
        BackendPreference::Portal => Ok(Backend::Portal),
        BackendPreference::X11 => {
            DisplayConnection::connect()?;
            Ok(Backend::X11)
        }
        BackendPreference::Auto => {
            let session_type = env::var("XDG_SESSION_TYPE").unwrap_or_default();
            let has_display = env::var_os("DISPLAY").is_some();
            if session_type.eq_ignore_ascii_case("x11") || has_display {
                DisplayConnection::connect().context(
                    "this looks like an X11 session, so automatic capture will not silently fall back to the portal",
                )?;
                Ok(Backend::X11)
            } else {
                Ok(Backend::Portal)
            }
        }
    }
}

pub(crate) fn list_monitors() -> Result<()> {
    let display = env::var("DISPLAY").unwrap_or_else(|_| "<unset>".to_owned());
    println!("X11 display {display} monitors:");
    for monitor in DisplayConnection::connect()?.monitors()? {
        let primary = if monitor.primary { " (primary)" } else { "" };
        println!("  {}: {}{}", monitor.name, monitor.geometry(), primary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_geometry_is_xrandr_compatible() {
        let monitor = Monitor {
            name: "DP-1".to_owned(),
            x: -1920,
            y: 40,
            width: 1920,
            height: 1080,
            primary: false,
        };
        assert_eq!(monitor.geometry(), "1920x1080-1920+40");
    }
}
