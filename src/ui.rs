//! Webview-based windows: the tray popup and the catalogue browser (all channels,
//! playlists or shows).
//!
//! Both pages are bundled HTML. They talk to the app through wry's IPC bridge
//! (`window.ipc.postMessage(json)`) and are updated by calling `window.app.*`.

use std::cell::Cell;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

use crate::AppEvent;
use crate::api::{MediaKind, Network};

const POPUP_HTML: &str = include_str!("ui/popup.html");
const CHANNELS_HTML: &str = include_str!("ui/channels.html");
const COMMON_CSS: &str = include_str!("ui/common.css");
const COMMON_JS: &str = include_str!("ui/common.js");
/// Station logos, shown when nothing with artwork is selected and in the network switch.
const LOGOS: [(Network, &[u8]); 2] = [
    (Network::Di, include_bytes!("../assets/app_icon152.png")),
    (Network::Jazzradio, include_bytes!("../assets/jazzradio_app_icon152.png")),
];
pub const APP_ICON_PNG: &[u8] = include_bytes!("../assets/app_icon240.png");

pub const POPUP_WIDTH: f64 = 340.0;
/// Gap between the tray icon and the popup.
const POPUP_MARGIN: i32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Popup,
    Channels,
}

/// Messages sent by the pages.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UiMsg {
    Ready,
    Play { id: u64 },
    PlayPlaylist { id: u64 },
    PlayShow { id: u64 },
    Toggle,
    Skip,
    Quality { id: u64 },
    Autostart { on: bool },
    Network { network: Network },
    Stop,
    Volume { value: f32, commit: bool },
    Favorite { id: u64, on: bool },
    OpenBrowser { mode: MediaKind },
    /// Follow or unfollow a playlist or show.
    Follow { media: MediaKind, id: u64, on: bool },
    /// Next page of the catalogue browser. `genre` is a playlist tag or a show's
    /// channel filter id; `seq` identifies the request so stale pages can be dropped.
    LoadPage { seq: u64, mode: MediaKind, page: u32, query: String, genre: Option<String> },
    Login,
    Logout,
    Quit,
    Size { height: f64 },
    Hide,
}

/// Static data: the channel catalogue. Pushed once per load.
#[derive(Debug, Clone, Serialize, Default)]
pub struct LibraryView {
    pub channels: Vec<ChannelView>,
    pub filters: Vec<FilterView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelView {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub image: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FilterView {
    pub id: u64,
    pub name: String,
    pub ids: Vec<u64>,
}

/// A playlist or show in the catalogue browser.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogItem {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub image: Option<String>,
}

/// A followed playlist or show.
#[derive(Debug, Clone, Serialize)]
pub struct TileView {
    pub id: u64,
    pub name: String,
    pub image: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QualityView {
    pub id: u64,
    pub label: String,
}

/// Dynamic state, pushed on every change.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub logged_in: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub status: &'static str,
    pub media_kind: Option<MediaKind>,
    pub media_id: Option<u64>,
    pub media_name: Option<String>,
    pub media_image: Option<String>,
    pub track: Option<String>,
    pub volume: f32,
    pub favorites: Vec<u64>,
    pub playlists: Vec<TileView>,
    pub shows: Vec<TileView>,
    /// `None` until known.
    pub skips_remaining: Option<u32>,
    pub qualities: Vec<QualityView>,
    pub quality: Option<u64>,
    /// `None` where starting at login isn't supported.
    pub autostart: Option<bool>,
    pub network: Network,
    pub networks: Vec<NetworkView>,
    pub has_playlists: bool,
    pub has_shows: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkView {
    pub id: Network,
    pub name: &'static str,
}

pub struct View {
    // The webview must be dropped before its window.
    webview: WebView,
    window: Window,
    pub kind: ViewKind,
    pub ready: bool,
    /// Requested logical height; the window's reported size lags behind resizes.
    height: Cell<f64>,
}

impl View {
    pub fn popup(target: &EventLoopWindowTarget<AppEvent>, proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
        let builder = WindowBuilder::new()
            .with_title(crate::APP_NAME)
            .with_inner_size(LogicalSize::new(POPUP_WIDTH, 360.0))
            .with_decorations(false)
            .with_always_on_top(true)
            .with_visible(false);
        #[cfg(target_os = "linux")]
        let builder = {
            use tao::platform::unix::WindowBuilderExtUnix;
            builder.with_skip_taskbar(true)
        };
        #[cfg(windows)]
        let builder = {
            use tao::platform::windows::WindowBuilderExtWindows;
            builder.with_skip_taskbar(true)
        };
        let window = builder.build(target).context("creating popup window")?;
        #[cfg(target_os = "linux")]
        menu_like::setup(&window, proxy.clone());
        Self::new(window, ViewKind::Popup, POPUP_HTML, proxy)
    }

    pub fn channels(target: &EventLoopWindowTarget<AppEvent>, proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
        let window = WindowBuilder::new()
            .with_title(browser_title(MediaKind::Channel))
            .with_window_icon(app_icon())
            .with_inner_size(LogicalSize::new(900.0, 680.0))
            .with_min_inner_size(LogicalSize::new(420.0, 360.0))
            .build(target)
            .context("creating channels window")?;
        Self::new(window, ViewKind::Channels, CHANNELS_HTML, proxy)
    }

    fn new(window: Window, kind: ViewKind, html: &str, proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
        use base64::Engine;
        let logos: serde_json::Map<String, serde_json::Value> = LOGOS
            .iter()
            .map(|(n, png)| {
                let uri = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png));
                (n.key().to_owned(), uri.into())
            })
            .collect();
        let html = html
            .replace("/*COMMON_CSS*/", COMMON_CSS)
            .replace("/*COMMON_JS*/", COMMON_JS)
            .replace("__LOGOS__", &serde_json::Value::Object(logos).to_string());
        let builder = WebViewBuilder::new()
            .with_html(html)
            .with_ipc_handler(move |req| match serde_json::from_str::<UiMsg>(req.body()) {
                Ok(msg) => {
                    let _ = proxy.send_event(AppEvent::Ui(kind, msg));
                }
                Err(e) => log::warn!("bad message from {kind:?} view: {e}: {}", req.body()),
            });
        let webview = build_webview(builder, &window)?;
        let height = Cell::new(window.inner_size().to_logical(window.scale_factor()).height);
        Ok(Self { webview, window, kind, ready: false, height })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// Whether this window (or one of its children, like the web view) is the window the
    /// user is working in.
    #[cfg(windows)]
    pub fn is_foreground(&self) -> bool {
        use tao::platform::windows::WindowExtWindows;
        use windows_sys::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, GetForegroundWindow};

        // SAFETY: plain Win32 queries on window handles; no memory is shared.
        let root = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) };
        root as isize == self.window.hwnd()
    }

    /// Window class of the foreground window, for diagnostics.
    #[cfg(windows)]
    pub fn foreground_class(&self) -> String {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, GetClassNameW, GetForegroundWindow};

        let mut buf = [0u16; 128];
        // SAFETY: plain Win32 queries; the buffer outlives the call and its length is passed.
        let len = unsafe {
            let fg = GetForegroundWindow();
            let n = GetClassNameW(GetAncestor(fg, GA_ROOT), buf.as_mut_ptr(), buf.len() as i32);
            n.max(0) as usize
        };
        let own = if self.is_foreground() { " (the popup)" } else { "" };
        format!("{}{own}", String::from_utf16_lossy(&buf[..len]))
    }

    pub fn show(&self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
        let _ = self.webview.focus();
        #[cfg(target_os = "linux")]
        if self.kind == ViewKind::Popup {
            menu_like::grab(&self.window);
        }
    }

    pub fn hide(&self) {
        #[cfg(target_os = "linux")]
        if self.kind == ViewKind::Popup {
            menu_like::ungrab(&self.window);
        }
        self.window.set_visible(false);
    }

    pub fn call(&self, function: &str, arg: &impl Serialize) {
        if !self.ready {
            return;
        }
        let arg = serde_json::to_string(arg).unwrap_or_else(|_| "null".into());
        let js = format!("window.app && window.app.{function}({arg});");
        if let Err(e) = self.webview.evaluate_script(&js) {
            log::warn!("updating {:?} view: {e}", self.kind);
        }
    }

    pub fn set_height(&self, height: f64) {
        self.height.set(height.ceil());
        self.window.set_inner_size(LogicalSize::new(POPUP_WIDTH, height.ceil()));
    }

    /// Places the popup next to a tray click at `anchor` (physical screen coordinates),
    /// opening towards the centre of the screen and staying on the monitor.
    pub fn place_near(&self, anchor: PhysicalPosition<i32>) {
        let scale = self.window.scale_factor();
        let (w, h) = ((POPUP_WIDTH * scale).round() as i32, (self.height.get() * scale).round() as i32);
        let monitor = self
            .window
            .available_monitors()
            .find(|m| {
                let (p, s) = (m.position(), m.size());
                anchor.x >= p.x && anchor.x < p.x + s.width as i32 && anchor.y >= p.y && anchor.y < p.y + s.height as i32
            })
            .or_else(|| self.window.primary_monitor());
        let Some(monitor) = monitor else {
            self.window.set_outer_position(anchor);
            return;
        };
        let (mp, ms) = (monitor.position(), monitor.size());
        let (mw, mh) = (ms.width as i32, ms.height as i32);
        let below = anchor.y < mp.y + mh / 2;
        let mut x = anchor.x - w / 2;
        let mut y = if below { anchor.y + POPUP_MARGIN } else { anchor.y - h - POPUP_MARGIN };
        // Panels usually sit at the very edge; keep clear of them.
        let edge = (32.0 * monitor.scale_factor()) as i32;
        x = x.clamp(mp.x + POPUP_MARGIN, (mp.x + mw - w - POPUP_MARGIN).max(mp.x));
        y = y.clamp(mp.y + if below { edge } else { 0 }, (mp.y + mh - h - if below { 0 } else { edge }).max(mp.y));
        self.window.set_outer_position(PhysicalPosition::new(x, y));
    }
}

/// The app logo as a window icon, shown by docks and task switchers. It's a large,
/// transparent-cornered rendition of the logo (the bundled artwork has white corners
/// and is too small to stay sharp at dock sizes). GDK silently drops X11 window icons
/// that don't fit a single X request (65535 words without BIG-REQUESTS), so it's
/// 240x240.
pub fn app_icon() -> Option<tao::window::Icon> {
    let img = image::load_from_memory(APP_ICON_PNG).inspect_err(|e| log::warn!("decoding app icon: {e}")).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    tao::window::Icon::from_rgba(img.into_raw(), w, h).inspect_err(|e| log::warn!("app icon: {e}")).ok()
}

pub fn build_webview(builder: WebViewBuilder, window: &Window) -> Result<WebView> {
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(window)?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().context("window has no GTK container")?)?
    };
    Ok(webview)
}

/// A small modal question; `on_accept` is sent if the user agrees.
#[cfg(target_os = "linux")]
pub fn confirm(text: &str, accept: &str, proxy: EventLoopProxy<AppEvent>, on_accept: AppEvent) {
    use gtk::prelude::*;

    let dialog = gtk::MessageDialog::new(
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Question,
        gtk::ButtonsType::None,
        text,
    );
    dialog.set_title(crate::APP_NAME);
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button(accept, gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Cancel);
    dialog.set_keep_above(true);
    dialog.set_position(gtk::WindowPosition::Center);
    let on_accept = std::cell::Cell::new(Some(on_accept));
    dialog.connect_response(move |d, response| {
        if response == gtk::ResponseType::Accept
            && let Some(ev) = on_accept.take()
        {
            let _ = proxy.send_event(ev);
        }
        d.close();
    });
    dialog.show_all();
    dialog.present();
}

pub fn browser_title(mode: MediaKind) -> &'static str {
    match mode {
        MediaKind::Channel => "All channels",
        MediaKind::Playlist => "All playlists",
        MediaKind::Show => "All shows",
    }
}

pub fn status_name(status: &crate::player::Status) -> &'static str {
    use crate::player::Status;
    match status {
        Status::Stopped => "stopped",
        Status::Loading => "loading",
        Status::Playing => "playing",
        Status::Paused => "paused",
    }
}


/// Makes the popup behave like a native menu on X11: the window manager neither places
/// nor manages it (override-redirect, so it opens exactly at the tray icon), and while
/// open it holds a pointer grab so that any click outside closes it. The keyboard is left
/// alone: an active keyboard grab would swallow global shortcuts such as media keys.
#[cfg(target_os = "linux")]
mod menu_like {
    use gtk::gdk::{self, prelude::*};
    use gtk::glib;
    use gtk::prelude::*;
    use tao::event_loop::EventLoopProxy;
    use tao::platform::unix::WindowExtUnix;
    use tao::window::Window;

    use super::{UiMsg, ViewKind};
    use crate::AppEvent;

    /// The panel may still hold its own grab right after the click; retry briefly.
    const GRAB_ATTEMPTS: u32 = 20;

    pub fn setup(window: &Window, proxy: EventLoopProxy<AppEvent>) {
        let gw = window.gtk_window();
        gw.set_type_hint(gdk::WindowTypeHint::PopupMenu);
        gw.set_skip_pager_hint(true);
        gw.realize();
        if let Some(gdk_window) = gw.window() {
            gdk_window.set_override_redirect(true);
        }
        gw.add_events(gdk::EventMask::BUTTON_PRESS_MASK);

        let hide = {
            let proxy = proxy.clone();
            move || {
                let _ = proxy.send_event(AppEvent::Ui(ViewKind::Popup, UiMsg::Hide));
            }
        };
        let on_press = hide.clone();
        // With owner_events, clicks inside go to the webview as usual; clicks anywhere
        // else are reported to the toplevel with coordinates outside of it.
        gw.connect_button_press_event(move |w, ev| {
            let (x, y) = ev.position();
            let (width, height) = (w.allocated_width() as f64, w.allocated_height() as f64);
            if x < 0.0 || y < 0.0 || x >= width || y >= height {
                log::debug!("click outside popup at {x},{y}");
                on_press();
            }
            glib::Propagation::Proceed
        });
        gw.connect_grab_broken_event(move |_, _| {
            log::debug!("popup grab broken");
            hide();
            glib::Propagation::Proceed
        });
    }

    pub fn grab(window: &Window) {
        let gw = window.gtk_window().clone();
        let mut attempts = 0;
        glib::timeout_add_local(std::time::Duration::from_millis(25), move || {
            attempts += 1;
            let (Some(gdk_window), Some(seat)) = (gw.window(), gdk::Display::default().and_then(|d| d.default_seat()))
            else {
                return glib::ControlFlow::Break;
            };
            if !gw.is_visible() {
                return glib::ControlFlow::Break;
            }
            let status = seat.grab(&gdk_window, gdk::SeatCapabilities::ALL_POINTING, true, None, None, None);
            if status == gdk::GrabStatus::Success {
                log::debug!("popup grabbed the pointer after {attempts} attempt(s)");
                return glib::ControlFlow::Break;
            }
            if attempts >= GRAB_ATTEMPTS {
                log::warn!("could not grab the pointer for the popup: {status:?}");
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    pub fn ungrab(_window: &Window) {
        if let Some(seat) = gdk::Display::default().and_then(|d| d.default_seat()) {
            seat.ungrab();
        }
    }
}
