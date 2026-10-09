//! OS media session: lets media keys, headphone buttons and the desktop's media widget
//! control playback, and shows what's playing there.
//!
//! `souvlaki` speaks MPRIS on Linux, System Media Transport Controls on Windows and the
//! Now Playing center on macOS.

use souvlaki::{MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use tao::event_loop::EventLoopProxy;

use crate::AppEvent;
use crate::player::Status;

/// What the OS is shown; updates are skipped when nothing changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NowPlaying {
    pub status: Option<Status>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub cover_url: Option<String>,
}

pub struct MediaKeys {
    controls: MediaControls,
    shown: NowPlaying,
}

impl MediaKeys {
    /// `hwnd` is a window of ours; Windows ties the media session to one.
    pub fn new(proxy: EventLoopProxy<AppEvent>, hwnd: Option<*mut std::ffi::c_void>) -> anyhow::Result<Self> {
        let config = PlatformConfig { display_name: crate::APP_NAME, dbus_name: "difm", hwnd };
        let mut controls = MediaControls::new(config).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        controls
            .attach(move |event| {
                let _ = proxy.send_event(AppEvent::MediaKey(event));
            })
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(Self { controls, shown: NowPlaying::default() })
    }

    pub fn update(&mut self, now: NowPlaying) {
        if now == self.shown {
            return;
        }
        if (&now.title, &now.artist, &now.cover_url) != (&self.shown.title, &self.shown.artist, &self.shown.cover_url) {
            let metadata = MediaMetadata {
                title: now.title.as_deref(),
                artist: now.artist.as_deref(),
                cover_url: now.cover_url.as_deref(),
                ..Default::default()
            };
            if let Err(e) = self.controls.set_metadata(metadata) {
                log::debug!("updating media metadata: {e:?}");
            }
        }
        if now.status != self.shown.status {
            let playback = match now.status {
                Some(Status::Playing | Status::Loading) => MediaPlayback::Playing { progress: None },
                Some(Status::Paused) => MediaPlayback::Paused { progress: None },
                Some(Status::Stopped) | None => MediaPlayback::Stopped,
            };
            if let Err(e) = self.controls.set_playback(playback) {
                log::debug!("updating media playback state: {e:?}");
            }
        }
        self.shown = now;
    }
}

