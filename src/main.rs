#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod config;
mod desktop;
mod login;
mod player;
mod stream;
mod tray;
mod ui;

use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use tao::dpi::PhysicalPosition;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};

use crate::api::{Api, Channel, ChannelFilter, Media, MediaKind, Network, Playlist, Quality, Show, SkipAllowance};
use crate::config::{Credentials, Settings};
use crate::login::LoginWindow;
use crate::player::{Command, PlayerHandle, Status};
use crate::tray::{Tray, TrayAction, TrayState};
use crate::ui::{CatalogItem, ChannelView, FilterView, LibraryView, NetworkView, QualityView, StateView, TileView, UiMsg, View, ViewKind};

/// Display name, used for the tray, window titles and notifications.
pub const APP_NAME: &str = "DI.FM";
/// Channel artwork size requested for the UI (2x the largest tile for HiDPI).
const ART_SIZE: u32 = 280;
/// A click on the tray icon right after the popup lost focus is the same click that
/// dismissed it; don't reopen.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub enum AppEvent {
    TrayClick(PhysicalPosition<i32>),
    TrayAction(TrayAction),
    Ui(ViewKind, UiMsg),
    Player(player::Event),
    /// A session for this network, from the login window or derived from another network's.
    LoggedIn(Network, Credentials),
    /// No session could be derived for this network; the user has to log in.
    LoginNeeded(Network),
    /// Wraps results of background work started for a network, so they're dropped if the
    /// user switched networks in the meantime.
    ForNetwork(Network, Box<AppEvent>),
    LoginFailed(String),
    Library(Result<(Vec<Channel>, Vec<ChannelFilter>, Vec<u64>), String>),
    Favorites(Vec<u64>),
    Playlists(Vec<Playlist>),
    Shows(Vec<Show>),
    /// A page of the catalogue browser, answering `UiMsg::LoadPage` number `seq`.
    CatalogPage { seq: u64, page: u32, result: Result<(CatalogItems, bool), String> },
    PlaylistTags(Vec<String>),
    Qualities { list: Vec<Quality>, preferred: Option<u64> },
    Skips(SkipAllowance),
    /// The skip window with this generation ran out; used skips are available again.
    SkipsExpired(u64),
    QualityChanged(Result<(), String>),
    ReloadFollowed,
    /// The user confirmed a tray menu action that asked first.
    Confirmed(TrayAction),
    StaleSession,
}

type Target = EventLoopWindowTarget<AppEvent>;

struct App {
    api: Api,
    proxy: EventLoopProxy<AppEvent>,
    player: PlayerHandle,
    settings: Settings,
    creds: Option<Credentials>,
    tray: Option<Tray>,
    popup: Option<View>,
    channels_window: Option<View>,
    login: Option<LoginWindow>,
    library: LibraryView,
    favorites: Vec<u64>,
    loading: bool,
    error: Option<String>,
    status: Status,
    media: Option<Media>,
    track: Option<String>,
    playlists: Vec<Playlist>,
    shows: Vec<Show>,
    /// Catalogue entries the browser has shown, so they can be played.
    seen_playlists: Vec<Playlist>,
    seen_shows: Vec<Show>,
    playlist_tags: Option<Vec<String>>,
    browser_mode: MediaKind,
    skips: Option<SkipAllowance>,
    skips_generation: u64,
    qualities: Vec<Quality>,
    quality: Option<u64>,
    linked_at: Option<Instant>,
    popup_hidden_at: Option<Instant>,
    popup_anchor: Option<PhysicalPosition<i32>>,
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("difm {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let settings = Settings::load();
    let api = Api::new(settings.network)?;
    let creds = Credentials::load(settings.network);
    let player = {
        let proxy = proxy.clone();
        player::spawn(api.clone(), creds.clone(), settings.volume, move |e| {
            let _ = proxy.send_event(AppEvent::Player(e));
        })
    };

    let mut app = App {
        api,
        proxy,
        player,
        settings,
        creds,
        tray: None,
        popup: None,
        channels_window: None,
        login: None,
        library: LibraryView::default(),
        favorites: Vec::new(),
        loading: false,
        error: None,
        status: Status::Stopped,
        media: None,
        track: None,
        playlists: Vec::new(),
        shows: Vec::new(),
        seen_playlists: Vec::new(),
        seen_shows: Vec::new(),
        playlist_tags: None,
        browser_mode: MediaKind::Channel,
        skips: None,
        skips_generation: 0,
        qualities: Vec::new(),
        quality: None,
        linked_at: None,
        popup_hidden_at: None,
        popup_anchor: None,
    };

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        app.handle(event, target, control_flow);
    });
}

impl App {
    fn handle(&mut self, event: Event<AppEvent>, target: &Target, control_flow: &mut ControlFlow) {
        match event {
            Event::NewEvents(StartCause::Init) => self.start(target, control_flow),

            Event::WindowEvent { window_id, event, .. } => {
                let is_popup = self.popup.as_ref().is_some_and(|p| p.id() == window_id);
                let is_channels = self.channels_window.as_ref().is_some_and(|w| w.id() == window_id);
                let is_login = self.login.as_ref().is_some_and(|l| l.id() == window_id);
                match event {
                    WindowEvent::Focused(false) if is_popup => {
                        log::debug!("popup lost focus");
                        self.hide_popup()
                    }
                    WindowEvent::CloseRequested if is_popup => self.hide_popup(),
                    // Keep the channel browser alive (and its scroll position); just hide it.
                    WindowEvent::CloseRequested if is_channels => {
                        if let Some(w) = &self.channels_window {
                            w.hide();
                        }
                    }
                    WindowEvent::CloseRequested if is_login => self.login = None,
                    _ => {}
                }
            }

            Event::UserEvent(ev) => self.user_event(ev, target, control_flow),
            _ => {}
        }
    }

    fn start(&mut self, target: &Target, control_flow: &mut ControlFlow) {
        match Tray::new(self.proxy.clone()) {
            Ok(t) => self.tray = Some(t),
            Err(e) => {
                log::error!("cannot create tray icon: {e:#}");
                *control_flow = ControlFlow::Exit;
                return;
            }
        }
        // Create the popup up front so it opens instantly.
        match View::popup(target, self.proxy.clone()) {
            Ok(p) => self.popup = Some(p),
            Err(e) => log::error!("creating popup: {e:#}"),
        }
        if let Err(e) = desktop::install() {
            log::warn!("installing the application entry: {e:#}");
        }
        self.apply_autostart();
        self.media = self.last_channel_media();
        if self.creds.is_some() {
            self.load_library();
        } else {
            self.obtain_session(target);
        }
        self.refresh();
    }

    fn network(&self) -> Network {
        self.api.network()
    }

    /// Sends events for the current network; see `AppEvent::ForNetwork`.
    fn net_proxy(&self) -> NetProxy {
        NetProxy { proxy: self.proxy.clone(), network: self.network() }
    }

    fn last_channel_media(&self) -> Option<Media> {
        let id = self.settings.last_channel(self.network())?;
        Some(Media { kind: MediaKind::Channel, id, name: String::new() })
    }

    /// Without a session for the current network, derives one from another network's
    /// session (one account works on all of them), or asks the user to log in.
    fn obtain_session(&mut self, target: &Target) {
        let network = self.network();
        let api_key = Network::ALL
            .into_iter()
            .filter(|&n| n != network)
            .filter_map(Credentials::load)
            .map(|c| c.api_key)
            .find(|k| !k.is_empty());
        let Some(api_key) = api_key else {
            self.open_login(target);
            return;
        };
        self.loading = true;
        let (api, proxy) = (self.api.clone(), self.proxy.clone());
        thread::spawn(move || {
            let result = api.create_session_from_api_key(&api_key).and_then(|s| login::from_session(&s));
            let _ = proxy.send_event(match result {
                Ok(creds) => AppEvent::LoggedIn(network, creds),
                Err(e) => {
                    log::warn!("creating a {} session: {e:#}", network.name());
                    AppEvent::LoginNeeded(network)
                }
            });
        });
    }

    fn switch_network(&mut self, target: &Target, network: Network) {
        if network == self.network() {
            return;
        }
        log::info!("switching to {}", network.name());
        self.settings.network = network;
        self.settings.save();
        self.api = self.api.for_network(network);
        self.creds = Credentials::load(network);
        self.player.send(Command::SetNetwork { api: self.api.clone(), creds: self.creds.clone() });
        // Forget everything that belongs to the previous network.
        self.library = LibraryView::default();
        self.favorites.clear();
        self.playlists.clear();
        self.shows.clear();
        self.seen_playlists.clear();
        self.seen_shows.clear();
        self.playlist_tags = None;
        self.skips = None;
        self.qualities.clear();
        self.quality = None;
        self.error = None;
        self.status = Status::Stopped;
        self.track = None;
        self.media = self.last_channel_media();
        if !self.network_has(self.browser_mode) {
            self.browser_mode = MediaKind::Channel;
        }
        self.push_library();
        self.push_browser_mode();
        if self.creds.is_some() {
            self.load_library();
        } else {
            self.obtain_session(target);
        }
        self.refresh();
    }

    fn network_has(&self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Channel => true,
            MediaKind::Playlist => self.network().has_playlists(),
            MediaKind::Show => self.network().has_shows(),
        }
    }

    fn user_event(&mut self, ev: AppEvent, target: &Target, control_flow: &mut ControlFlow) {
        match ev {
            AppEvent::TrayClick(pos) => self.toggle_popup(pos),
            AppEvent::TrayAction(action) => self.tray_action(action, target),
            AppEvent::Ui(kind, msg) => self.ui_message(kind, msg, target, control_flow),

            AppEvent::ForNetwork(network, ev) => {
                if network == self.network() {
                    self.user_event(*ev, target, control_flow);
                }
            }

            AppEvent::LoggedIn(network, creds) => {
                if let Err(e) = creds.save(network) {
                    log::error!("saving credentials: {e:#}");
                }
                if network != self.network() {
                    return;
                }
                self.login = None;
                log::info!("linked {} account #{}", network.name(), creds.id);
                self.loading = false;
                self.player.send(Command::SetCredentials(Some(creds.clone())));
                self.creds = Some(creds);
                self.linked_at = Some(Instant::now());
                self.load_library();
                self.refresh();
            }

            AppEvent::LoginNeeded(network) => {
                if network == self.network() {
                    self.loading = false;
                    self.open_login(target);
                    self.refresh();
                }
            }

            AppEvent::LoginFailed(msg) => {
                log::info!("login failed: {msg}");
                if let Some(l) = &self.login {
                    l.show_error(&msg);
                }
            }

            AppEvent::Library(result) => {
                self.loading = false;
                match result {
                    Ok((channels, filters, favorites)) => {
                        self.set_library(channels, filters);
                        self.favorites = favorites;
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
                self.push_library();
                self.refresh();
            }

            AppEvent::Favorites(favorites) => {
                self.favorites = favorites;
                self.refresh();
            }

            AppEvent::Playlists(playlists) => {
                self.playlists = playlists;
                self.refresh();
            }

            AppEvent::ReloadFollowed => self.load_followed(),

            AppEvent::Confirmed(action) => match action {
                TrayAction::Logout => self.forget_credentials(),
                TrayAction::Quit => self.quit(control_flow),
                _ => {}
            },

            AppEvent::Shows(shows) => {
                self.shows = shows;
                self.refresh();
            }

            AppEvent::CatalogPage { seq, page, result } => {
                let reply = match result {
                    Ok((items, more)) => {
                        let items = match items {
                            CatalogItems::Playlists(list) => {
                                let views = list.iter().map(playlist_item).collect::<Vec<_>>();
                                self.seen_playlists.extend(list);
                                views
                            }
                            CatalogItems::Shows(list) => {
                                let views = list.iter().map(show_item).collect::<Vec<_>>();
                                self.seen_shows.extend(list);
                                views
                            }
                        };
                        serde_json::json!({ "seq": seq, "page": page, "items": items, "more": more })
                    }
                    Err(e) => {
                        log::warn!("loading catalogue page: {e}");
                        serde_json::json!({ "seq": seq, "page": page, "error": "Could not load the list. Check your connection." })
                    }
                };
                if let Some(w) = &self.channels_window {
                    w.call("appendPage", &reply);
                }
            }

            AppEvent::PlaylistTags(tags) => {
                self.playlist_tags = Some(tags);
                self.push_playlist_tags();
            }

            AppEvent::Qualities { list, preferred } => {
                self.quality = preferred.or_else(|| list.iter().find(|q| q.default).map(|q| q.id));
                self.qualities = list;
                self.refresh();
            }

            AppEvent::Skips(allowance) => self.set_skips(allowance),
            AppEvent::SkipsExpired(generation) => {
                if generation == self.skips_generation {
                    self.load_skips();
                }
            }

            AppEvent::QualityChanged(result) => match result {
                Ok(()) => self.player.send(Command::Reload),
                Err(e) => {
                    log::warn!("changing quality: {e}");
                    notify("Audio quality", "Could not change the stream quality.");
                    self.load_account();
                }
            },

            AppEvent::StaleSession | AppEvent::Player(player::Event::StaleSession) => {
                log::warn!("session rejected by the API");
                self.forget_credentials();
                // A session rejected right after linking won't get better by linking
                // again, so don't loop.
                if self.linked_at.take().is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
                    notify("Login rejected", "The session could not be used by the app.");
                } else {
                    notify("Session expired", "Please log in again.");
                    self.open_login(target);
                }
            }

            AppEvent::Player(ev) => match ev {
                player::Event::State { status, media, track } => {
                    self.status = status;
                    if media.is_some() {
                        self.media = media;
                    }
                    self.track = track;
                    self.refresh();
                }
                player::Event::TakenOver(other) => notify(
                    "Streaming paused",
                    &format!("Playback was started on {}. Only one stream per account is allowed.", other.describe()),
                ),
                player::Event::Error(msg) => notify("Playback error", &msg),
                player::Event::Skips(allowance) => self.set_skips(allowance),
                player::Event::SkipFailed(msg) => {
                    notify("Can't skip", &msg);
                    self.load_skips();
                }
                player::Event::Ended(name) => notify("Playlist finished", &format!("You've heard all of {name}.")),
                player::Event::StaleSession => unreachable!("handled above"),
            },
        }
    }

    fn tray_action(&mut self, action: TrayAction, target: &Target) {
        match action {
            TrayAction::TogglePause => self.toggle_playback(),
            TrayAction::Stop => self.player.send(Command::Stop),
            TrayAction::OpenChannels => self.open_browser(target, MediaKind::Channel),
            TrayAction::Login => self.open_login(target),
            TrayAction::Logout | TrayAction::Quit => self.confirm(action),
        }
    }

    fn ui_message(&mut self, kind: ViewKind, msg: UiMsg, target: &Target, control_flow: &mut ControlFlow) {
        match msg {
            UiMsg::Ready => {
                let view = match kind {
                    ViewKind::Popup => self.popup.as_mut(),
                    ViewKind::Channels => self.channels_window.as_mut(),
                };
                if let Some(v) = view {
                    v.ready = true;
                }
                self.push_library();
                if kind == ViewKind::Channels {
                    self.push_playlist_tags();
                    self.push_browser_mode();
                }
                self.refresh();
            }
            UiMsg::Play { id } => self.play(id),
            UiMsg::PlayPlaylist { id } => self.play_playlist(id),
            UiMsg::PlayShow { id } => self.play_show(id),
            UiMsg::Toggle => self.toggle_playback(),
            UiMsg::Skip => self.player.send(Command::Skip),
            UiMsg::Quality { id } => self.set_quality(id),
            UiMsg::Network { network } => self.switch_network(target, network),
            UiMsg::Autostart { on } => {
                self.settings.autostart = on;
                self.settings.save();
                self.apply_autostart();
                self.refresh();
            }
            UiMsg::Stop => self.player.send(Command::Stop),
            UiMsg::Volume { value, commit } => {
                let value = value.clamp(0.0, 1.0);
                self.player.send(Command::SetVolume(value));
                self.settings.volume = value;
                if commit {
                    self.settings.save();
                    self.refresh();
                }
            }
            UiMsg::Favorite { id, on } => self.set_favorite(id, on),
            UiMsg::OpenBrowser { mode } => {
                self.hide_popup();
                self.open_browser(target, mode);
            }
            UiMsg::Follow { media, id, on } => self.set_following(media, id, on),
            UiMsg::LoadPage { seq, mode, page, query, genre } => self.load_page(seq, mode, page, query, genre),
            UiMsg::Login => {
                self.hide_popup();
                self.open_login(target);
            }
            UiMsg::Logout => self.forget_credentials(),
            UiMsg::Quit => self.quit(control_flow),
            UiMsg::Size { height } => {
                if let (ViewKind::Popup, Some(p)) = (kind, &self.popup) {
                    p.set_height(height);
                    if let (true, Some(anchor)) = (p.is_visible(), self.popup_anchor) {
                        p.place_near(anchor);
                    }
                }
            }
            UiMsg::Hide => self.hide_popup(),
        }
    }

    fn toggle_popup(&mut self, anchor: PhysicalPosition<i32>) {
        let Some(popup) = &self.popup else { return };
        if popup.is_visible() {
            self.hide_popup();
            return;
        }
        if self.popup_hidden_at.is_some_and(|t| t.elapsed() < REOPEN_GUARD) {
            return;
        }
        self.popup_anchor = Some(anchor);
        popup.place_near(anchor);
        popup.show();
        // Pick up playlists and shows followed on the website since the last look.
        self.load_followed();
    }

    fn hide_popup(&mut self) {
        if let Some(p) = &self.popup {
            if p.is_visible() {
                p.call("closeMenu", &());
                p.hide();
                self.popup_hidden_at = Some(Instant::now());
            }
        }
    }

    fn open_browser(&mut self, target: &Target, mode: MediaKind) {
        if self.creds.is_none() {
            self.open_login(target);
            return;
        }
        self.browser_mode = mode;
        if mode == MediaKind::Playlist {
            self.load_playlist_tags();
        }
        if self.channels_window.is_none() {
            match View::channels(target, self.proxy.clone()) {
                Ok(w) => self.channels_window = Some(w),
                Err(e) => {
                    log::error!("opening channels window: {e:#}");
                    return;
                }
            }
        }
        self.push_browser_mode();
        if let Some(w) = &self.channels_window {
            w.show();
            w.call("focusSearch", &());
        }
    }

    fn push_browser_mode(&self) {
        if let Some(w) = &self.channels_window {
            w.set_title(ui::browser_title(self.browser_mode));
            w.call("setMode", &self.browser_mode);
        }
    }

    fn load_page(&self, seq: u64, mode: MediaKind, page: u32, query: String, genre: Option<String>) {
        log::debug!("loading {mode:?} page {page} (#{seq}, query {query:?}, genre {genre:?})");
        let (api, proxy) = (self.api.clone(), self.net_proxy());
        thread::spawn(move || {
            let result = match mode {
                MediaKind::Playlist => {
                    api.playlists_page(page, &query, genre.as_deref()).map(|(l, more)| (CatalogItems::Playlists(l), more))
                }
                MediaKind::Show => {
                    let genre = genre.and_then(|g| g.parse().ok());
                    api.shows_page(page, &query, genre).map(|(l, more)| (CatalogItems::Shows(l), more))
                }
                MediaKind::Channel => Err(anyhow::anyhow!("channels aren't paged")),
            };
            let result = result.map_err(|e| format!("{e:#}"));
            proxy.send_event(AppEvent::CatalogPage { seq, page, result });
        });
    }

    fn load_playlist_tags(&self) {
        if self.playlist_tags.is_some() {
            return;
        }
        let (api, proxy) = (self.api.clone(), self.net_proxy());
        thread::spawn(move || match api.playlist_tags() {
            Ok(tags) => {
                proxy.send_event(AppEvent::PlaylistTags(tags));
            }
            Err(e) => log::warn!("loading playlist tags: {e:#}"),
        });
    }

    fn push_playlist_tags(&self) {
        if let (Some(w), Some(tags)) = (&self.channels_window, &self.playlist_tags) {
            w.call("setTags", tags);
        }
    }

    fn set_following(&mut self, kind: MediaKind, id: u64, on: bool) {
        let Some(creds) = self.creds.clone() else { return };
        // Optimistic update (newest first, like the service lists them); the server's
        // lists replace it afterwards.
        match kind {
            MediaKind::Playlist => {
                self.playlists.retain(|p| p.id != id);
                if let Some(p) = self.seen_playlists.iter().find(|p| p.id == id).filter(|_| on) {
                    self.playlists.insert(0, p.clone());
                }
            }
            MediaKind::Show => {
                self.shows.retain(|s| s.id != id);
                if let Some(s) = self.seen_shows.iter().find(|s| s.id == id).filter(|_| on) {
                    self.shows.insert(0, s.clone());
                }
            }
            MediaKind::Channel => return self.set_favorite(id, on),
        }
        self.refresh();
        let api = self.api.clone();
        let proxy = self.net_proxy();
        thread::spawn(move || {
            if let Err(e) = api.set_following(&creds, kind, id, on) {
                log::warn!("updating followed {kind:?}: {e:#}");
                notify("Following", "Could not update what you follow.");
            }
            proxy.send_event(AppEvent::ReloadFollowed);
        });
    }

    fn open_login(&mut self, target: &Target) {
        if let Some(l) = &self.login {
            l.focus();
            return;
        }
        match LoginWindow::open(target, self.proxy.clone(), self.api.clone()) {
            Ok(l) => self.login = Some(l),
            Err(e) => {
                log::error!("opening login window: {e:#}");
                notify("Login unavailable", &format!("Could not open the login window: {e:#}"));
            }
        }
    }

    fn play(&mut self, channel_id: u64) {
        let Some(ch) = self.library.channels.iter().find(|c| c.id == channel_id) else { return };
        self.settings.set_last_channel(self.network(), channel_id);
        self.settings.save();
        self.player.send(Command::Play(Media { kind: MediaKind::Channel, id: channel_id, name: ch.name.clone() }));
    }

    fn play_playlist(&mut self, playlist_id: u64) {
        let Some(pl) = self.playlists.iter().chain(self.seen_playlists.iter()).find(|p| p.id == playlist_id) else {
            return;
        };
        self.player.send(Command::Play(Media { kind: MediaKind::Playlist, id: playlist_id, name: pl.name.clone() }));
    }

    fn play_show(&mut self, show_id: u64) {
        let Some(show) = self.shows.iter().chain(self.seen_shows.iter()).find(|s| s.id == show_id) else {
            return;
        };
        self.player.send(Command::Play(Media { kind: MediaKind::Show, id: show_id, name: show.name.trim().to_owned() }));
    }

    fn toggle_playback(&mut self) {
        // Restarting from scratch also covers a channel remembered from the last run,
        // which the player doesn't know about.
        match (&self.status, &self.media) {
            (Status::Stopped, Some(Media { kind: MediaKind::Channel, id, .. })) => self.play(*id),
            _ => self.player.send(Command::TogglePause),
        }
    }

    /// Asks before logging out or quitting from the tray menu (the popup asks itself).
    fn confirm(&self, action: TrayAction) {
        let (text, accept) = match action {
            TrayAction::Logout => ("Log out of DI.FM? Playback stops and you will need to log in again.", "Log out"),
            TrayAction::Quit => ("Quit DI.FM? Playback stops.", "Quit"),
            _ => return,
        };
        #[cfg(target_os = "linux")]
        ui::confirm(text, accept, self.proxy.clone(), AppEvent::Confirmed(action));
        // Without a native dialog elsewhere yet, act right away.
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (text, accept);
            let _ = self.proxy.send_event(AppEvent::Confirmed(action));
        }
    }

    fn apply_autostart(&self) {
        if let Err(e) = desktop::set_autostart(self.settings.autostart) {
            log::warn!("updating start at login: {e:#}");
        }
    }

    fn set_quality(&mut self, quality_id: u64) {
        let Some(creds) = self.creds.clone() else { return };
        if self.quality == Some(quality_id) {
            return;
        }
        self.quality = Some(quality_id);
        self.refresh();
        let (api, proxy) = (self.api.clone(), self.net_proxy());
        thread::spawn(move || {
            let result = api.set_preferred_quality(&creds, quality_id).map_err(|e| format!("{e:#}"));
            proxy.send_event(AppEvent::QualityChanged(result));
        });
    }

    fn set_skips(&mut self, allowance: SkipAllowance) {
        self.skips = Some(allowance);
        self.skips_generation += 1;
        if let Some(expires_at) = allowance.expires_at {
            let (generation, proxy) = (self.skips_generation, self.net_proxy());
            let wait = (expires_at - unix_now()).max(0) as u64 + 2;
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(wait));
                proxy.send_event(AppEvent::SkipsExpired(generation));
            });
        }
        self.refresh();
    }

    fn set_favorite(&mut self, channel_id: u64, on: bool) {
        let Some(creds) = self.creds.clone() else { return };
        // Optimistic update; the server's list replaces it afterwards.
        self.favorites.retain(|&id| id != channel_id);
        if on {
            self.favorites.push(channel_id);
        }
        self.refresh();
        let (api, proxy) = (self.api.clone(), self.net_proxy());
        thread::spawn(move || {
            if let Err(e) = api.set_favorite(&creds, channel_id, on) {
                log::warn!("updating favourite: {e:#}");
                notify("Favourites", "Could not update favourites.");
            }
            match api.favorite_channel_ids(&creds) {
                Ok(favs) => {
                    proxy.send_event(AppEvent::Favorites(favs));
                }
                Err(e) if api::is_stale_session(&e) => {
                    proxy.send_event(AppEvent::StaleSession);
                }
                Err(e) => log::warn!("reloading favourites: {e:#}"),
            }
        });
    }

    fn forget_credentials(&mut self) {
        Credentials::clear();
        self.creds = None;
        self.player.send(Command::SetCredentials(None));
        self.favorites.clear();
        self.playlists.clear();
        self.shows.clear();
        self.skips = None;
        self.qualities.clear();
        self.quality = None;
        self.error = None;
        self.refresh();
    }

    fn quit(&mut self, control_flow: &mut ControlFlow) {
        self.player.send(Command::Stop);
        *control_flow = ControlFlow::Exit;
    }

    /// Account-wide settings: stream qualities and the skip allowance.
    fn load_account(&self) {
        let Some(creds) = self.creds.clone() else { return };
        let (api, proxy) = (self.api.clone(), self.net_proxy());
        thread::spawn(move || {
            match api.qualities(&creds).and_then(|list| Ok((list, api.preferred_quality(&creds)?))) {
                Ok((list, preferred)) => {
                    proxy.send_event(AppEvent::Qualities { list, preferred });
                }
                Err(e) => log::warn!("loading stream qualities: {e:#}"),
            }
        });
        self.load_skips();
    }

    fn load_skips(&self) {
        let Some(creds) = self.creds.clone() else { return };
        let (api, proxy) = (self.api.clone(), self.net_proxy());
        thread::spawn(move || match api.skip_allowance(&creds) {
            Ok(allowance) => {
                proxy.send_event(AppEvent::Skips(allowance));
            }
            Err(e) => log::warn!("loading skip allowance: {e:#}"),
        });
    }

    fn load_followed(&self) {
        let Some(creds) = self.creds.clone() else { return };
        let (api, proxy) = (self.api.clone(), self.net_proxy());
        thread::spawn(move || {
            match api.followed_playlists(&creds) {
                Ok(playlists) => {
                    proxy.send_event(AppEvent::Playlists(playlists));
                }
                Err(e) => log::warn!("loading followed playlists: {e:#}"),
            }
            match api.followed_shows(&creds) {
                Ok(shows) => {
                    proxy.send_event(AppEvent::Shows(shows));
                }
                Err(e) => log::warn!("loading followed shows: {e:#}"),
            }
        });
    }

    fn load_library(&mut self) {
        self.load_account();
        self.load_followed();
        let Some(creds) = self.creds.clone() else { return };
        self.loading = true;
        let (api, proxy) = (self.api.clone(), self.net_proxy());
        thread::spawn(move || {
            let result = (|| -> Result<_> {
                let channels = api.channels()?;
                let filters = api.channel_filters().unwrap_or_else(|e| {
                    log::warn!("loading genres: {e:#}");
                    Vec::new()
                });
                Ok((channels, filters, api.favorite_channel_ids(&creds)?))
            })();
            let event = match result {
                Ok(lib) => AppEvent::Library(Ok(lib)),
                Err(e) if api::is_stale_session(&e) => AppEvent::StaleSession,
                Err(e) => {
                    log::warn!("loading channels: {e:#}");
                    AppEvent::Library(Err("Could not load channels. Check your connection.".to_owned()))
                }
            };
            proxy.send_event(event);
        });
    }

    fn set_library(&mut self, channels: Vec<Channel>, filters: Vec<ChannelFilter>) {
        if let Some(Media { kind: MediaKind::Channel, id, name }) = &mut self.media {
            if let Some(ch) = channels.iter().find(|c| c.id == *id) {
                name.clone_from(&ch.name);
            }
        }
        self.library = LibraryView {
            channels: channels
                .iter()
                .map(|c| ChannelView {
                    id: c.id,
                    name: c.name.clone(),
                    description: c.description_short.clone(),
                    image: c.image_url(ART_SIZE),
                })
                .collect(),
            filters: filters.into_iter().map(|f| FilterView { id: f.id, name: f.name, ids: f.channel_ids }).collect(),
        };
    }

    fn views(&self) -> impl Iterator<Item = &View> {
        self.popup.iter().chain(self.channels_window.iter())
    }

    fn push_library(&self) {
        for v in self.views() {
            v.call("setLibrary", &self.library);
        }
    }

    /// Pushes the current state to the tray and both views.
    fn refresh(&mut self) {
        let media_name = self.media.as_ref().map(|m| m.name.clone()).filter(|n| !n.is_empty());
        let media_image = self.media.as_ref().and_then(|m| match m.kind {
            MediaKind::Channel => self.library.channels.iter().find(|c| c.id == m.id).and_then(|c| c.image.clone()),
            MediaKind::Playlist => (self.playlists.iter().chain(self.seen_playlists.iter()))
                .find(|p| p.id == m.id)
                .and_then(|p| p.image_url(ART_SIZE)),
            MediaKind::Show => (self.shows.iter().chain(self.seen_shows.iter()))
                .find(|s| s.id == m.id)
                .and_then(|s| s.image_url(ART_SIZE)),
        });
        let state = StateView {
            logged_in: self.creds.is_some(),
            loading: self.loading,
            error: self.error.clone(),
            status: ui::status_name(&self.status),
            media_kind: self.media.as_ref().map(|m| m.kind),
            media_id: self.media.as_ref().map(|m| m.id),
            media_name: media_name.clone(),
            media_image,
            track: self.track.clone(),
            volume: self.settings.volume,
            favorites: self.favorites.clone(),
            playlists: self
                .playlists
                .iter()
                .map(|p| TileView { id: p.id, name: p.name.clone(), image: p.image_url(ART_SIZE) })
                .collect(),
            shows: self
                .shows
                .iter()
                .map(|s| TileView { id: s.id, name: s.name.trim().to_owned(), image: s.image_url(ART_SIZE) })
                .collect(),
            skips_remaining: self.skips.map(|s| s.remaining),
            qualities: self.qualities.iter().map(|q| QualityView { id: q.id, label: q.label() }).collect(),
            quality: self.quality,
            autostart: desktop::autostart_supported().then_some(self.settings.autostart),
            network: self.network(),
            networks: Network::ALL.into_iter().map(|n| NetworkView { id: n, name: n.name() }).collect(),
            has_playlists: self.network().has_playlists(),
            has_shows: self.network().has_shows(),
        };
        for v in self.views() {
            v.call("render", &state);
        }

        let tooltip = match (&self.status, &media_name, &self.track) {
            (Status::Stopped, _, _) | (_, None, _) => "Not playing".to_owned(),
            (Status::Paused, Some(ch), _) => format!("{ch} (paused)"),
            (_, Some(ch), Some(track)) => format!("{ch}\n{track}"),
            (_, Some(ch), None) => ch.clone(),
        };
        let network = self.network();
        let can_play = self.creds.is_some() && self.settings.last_channel(network).is_some();
        if let Some(t) = self.tray.as_mut() {
            t.update(TrayState {
                network,
                status: self.status.clone(),
                tooltip,
                logged_in: self.creds.is_some(),
                can_play,
            });
        }
    }
}

/// A page of the catalogue browser.
#[derive(Debug)]
pub enum CatalogItems {
    Playlists(Vec<Playlist>),
    Shows(Vec<Show>),
}

fn playlist_item(p: &Playlist) -> CatalogItem {
    CatalogItem { id: p.id, name: p.name.clone(), description: p.description.clone().unwrap_or_default(), image: p.image_url(ART_SIZE) }
}

fn show_item(s: &Show) -> CatalogItem {
    CatalogItem {
        id: s.id,
        name: s.name.trim().to_owned(),
        description: s.artists_tagline.clone().unwrap_or_default(),
        image: s.image_url(ART_SIZE),
    }
}

/// An event proxy that tags events with the network they were produced for.
#[derive(Clone)]
struct NetProxy {
    proxy: EventLoopProxy<AppEvent>,
    network: Network,
}

impl NetProxy {
    /// Like `EventLoopProxy::send_event`; a closed loop means the app is quitting.
    fn send_event(&self, event: AppEvent) {
        let _ = self.proxy.send_event(AppEvent::ForNetwork(self.network, Box::new(event)));
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn notify(summary: &str, body: &str) {
    let (summary, body) = (summary.to_owned(), body.to_owned());
    // D-Bus notifications can block briefly; keep them off the UI thread.
    thread::spawn(move || {
        if let Err(e) = notify_rust::Notification::new().appname(APP_NAME).summary(&summary).body(&body).show() {
            log::debug!("notification failed: {e}");
        }
    });
}
