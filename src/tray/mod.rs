//! Tray icon. A left click opens the popup (see `ui.rs`); a right click shows a small
//! native menu with the essentials.
//!
//! On Linux we speak StatusNotifierItem directly (via `ksni`): libappindicator never
//! reports left clicks, it always opens its own menu, so it can't anchor a custom popup.
//! Elsewhere `tray-icon` reports clicks with their screen position.

use anyhow::Result;

use crate::api::Network;
use crate::player::Status;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Tray;

#[cfg(not(target_os = "linux"))]
mod other;
#[cfg(not(target_os = "linux"))]
pub use other::Tray;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    TogglePause,
    Stop,
    OpenChannels,
    Login,
    Logout,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrayState {
    pub network: Network,
    pub status: Status,
    pub tooltip: String,
    pub logged_in: bool,
    pub can_play: bool,
}

impl Default for TrayState {
    fn default() -> Self {
        Self { network: Network::Di, status: Status::Stopped, tooltip: crate::APP_NAME.into(), logged_in: false, can_play: false }
    }
}

impl TrayState {
    fn active(&self) -> bool {
        matches!(self.status, Status::Playing | Status::Loading)
    }

    fn toggle_label(&self) -> &'static str {
        match self.status {
            Status::Playing | Status::Loading => "Pause",
            Status::Paused => "Resume",
            Status::Stopped => "Play",
        }
    }
}

fn icon_png(network: Network) -> &'static [u8] {
    match network {
        Network::Di => include_bytes!("../../assets/action_icon48.png"),
        Network::Jazzradio => include_bytes!("../../assets/jazzradio_action_icon48.png"),
    }
}

/// The artwork is full-bleed, which looks oversized next to other tray icons (they
/// leave a transparent margin); shrink it within the same canvas.
const ICON_SCALE: f32 = 0.8;

/// RGBA pixels of the tray icon: `(active, idle, width, height)`. The idle variant
/// is desaturated so it's obvious at a glance whether something is playing.
fn icon_pixels(network: Network) -> Result<(Vec<u8>, Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(icon_png(network))?.into_rgba8();
    let (w, h) = img.dimensions();
    let (sw, sh) = ((w as f32 * ICON_SCALE).round() as u32, (h as f32 * ICON_SCALE).round() as u32);
    let small = image::imageops::resize(&img, sw, sh, image::imageops::FilterType::Lanczos3);
    let mut canvas = image::RgbaImage::new(w, h);
    image::imageops::overlay(&mut canvas, &small, ((w - sw) / 2).into(), ((h - sh) / 2).into());
    let active = canvas.into_raw();
    let mut idle = active.clone();
    for px in idle.chunks_exact_mut(4) {
        let l = (0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32) as u8;
        px[0] = l;
        px[1] = l;
        px[2] = l;
        px[3] = (px[3] as f32 * 0.8) as u8;
    }
    Ok((active, idle, w, h))
}
