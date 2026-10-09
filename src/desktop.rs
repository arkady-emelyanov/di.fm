//! Desktop integration: an application entry (so docks and menus show the app with its
//! own, sharp icon) and starting at login. On Linux these are XDG desktop entries;
//! other platforms aren't supported yet.

use anyhow::Result;

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};

    /// File name of both desktop entries.
    const ENTRY: &str = "difm-tray.desktop";
    /// WM_CLASS of the app's windows, which docks use to match them to the entry.
    const WM_CLASS: &str = "Difm";

    fn base() -> Result<directories::BaseDirs> {
        directories::BaseDirs::new().context("cannot determine home directories")
    }

    fn autostart_path() -> Result<PathBuf> {
        Ok(base()?.config_dir().join("autostart").join(ENTRY))
    }

    /// Writes the app icon and the application entry. Rewritten on every start so they
    /// follow the binary if it moves.
    pub fn install() -> Result<()> {
        let base = base()?;
        let icon = base.data_dir().join("difm-tray").join("icon.png");
        write(&icon, crate::ui::APP_ICON_PNG)?;
        let apps = base.data_dir().join("applications").join(ENTRY);
        write(&apps, entry(&icon, false)?.as_bytes())
    }

    /// Writes or removes the autostart entry.
    pub fn set_autostart(on: bool) -> Result<()> {
        let path = autostart_path()?;
        if !on {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        let icon = base()?.data_dir().join("difm-tray").join("icon.png");
        write(&path, entry(&icon, true)?.as_bytes())
    }

    fn entry(icon: &Path, autostart: bool) -> Result<String> {
        let exe = std::env::current_exe().context("locating the app binary")?;
        let mut text = format!(
            "[Desktop Entry]\nType=Application\nName={name}\nComment=DI.FM radio in the system tray\nExec={exec}\nIcon={icon}\nTerminal=false\nCategories=AudioVideo;Audio;Player;\nStartupWMClass={WM_CLASS}\n",
            name = crate::APP_NAME,
            exec = quote(&exe.to_string_lossy()),
            icon = icon.display(),
        );
        if autostart {
            text.push_str("X-GNOME-Autostart-enabled=true\n");
        }
        Ok(text)
    }

    /// Desktop entry quoting: wrap in double quotes, escape `"`, `` ` ``, `$` and `\`.
    fn quote(arg: &str) -> String {
        let mut quoted = String::from('"');
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    }

    fn write(path: &Path, data: &[u8]) -> Result<()> {
        // Leave the file alone when unchanged, so desktops watching it don't reload.
        if fs::read(path).is_ok_and(|old| old == data) {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, data).with_context(|| format!("writing {}", path.display()))
    }
}

pub fn autostart_supported() -> bool {
    cfg!(target_os = "linux")
}

pub fn install() -> Result<()> {
    #[cfg(target_os = "linux")]
    return imp::install();
    #[cfg(not(target_os = "linux"))]
    Ok(())
}

pub fn set_autostart(on: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    return imp::set_autostart(on);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = on;
        Ok(())
    }
}
