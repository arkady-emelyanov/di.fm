use anyhow::Result;
use tao::dpi::PhysicalPosition;
use tao::event_loop::EventLoopProxy;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{TrayAction, TrayState, icon_pixels};
use crate::AppEvent;
use crate::api::Network;

const ACTIONS: [(&str, TrayAction); 6] = [
    ("toggle", TrayAction::TogglePause),
    ("stop", TrayAction::Stop),
    ("channels", TrayAction::OpenChannels),
    ("login", TrayAction::Login),
    ("logout", TrayAction::Logout),
    ("quit", TrayAction::Quit),
];

pub struct Tray {
    icon: TrayIcon,
    /// `(network, active, idle)` icons.
    icons: Vec<(Network, Icon, Icon)>,
    toggle: MenuItem,
    stop: MenuItem,
    channels: MenuItem,
    login: MenuItem,
    logout: MenuItem,
    state: Option<TrayState>,
}

impl Tray {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
        let icons = Network::ALL
            .into_iter()
            .map(|n| {
                let (active, idle, w, h) = icon_pixels(n)?;
                Ok((n, Icon::from_rgba(active, w, h)?, Icon::from_rgba(idle, w, h)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let idle = icons[0].2.clone();

        let toggle = MenuItem::with_id("toggle", "Play", false, None);
        let stop = MenuItem::with_id("stop", "Stop", false, None);
        let channels = MenuItem::with_id("channels", "All channels…", false, None);
        let login = MenuItem::with_id("login", "Log in…", true, None);
        let logout = MenuItem::with_id("logout", "Log out", true, None);
        let menu = Menu::new();
        menu.append(&toggle)?;
        menu.append(&stop)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&channels)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&login)?;
        menu.append(&logout)?;
        menu.append(&MenuItem::with_id("quit", "Quit", true, None))?;

        let icon = TrayIconBuilder::new()
            .with_id("difm")
            .with_icon(idle.clone())
            .with_tooltip(crate::APP_NAME)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        let click_proxy = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            log::debug!("tray event: {e:?}");
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, position, .. } = e {
                let pos = PhysicalPosition::new(position.x as i32, position.y as i32);
                let _ = click_proxy.send_event(AppEvent::TrayClick(pos));
            }
        }));
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if let Some((_, action)) = ACTIONS.iter().find(|(id, _)| e.id.0 == *id) {
                let _ = proxy.send_event(AppEvent::TrayAction(*action));
            }
        }));

        Ok(Self { icon, icons, toggle, stop, channels, login, logout, state: None })
    }

    pub fn update(&mut self, state: TrayState) {
        if self.state.as_ref() == Some(&state) {
            return;
        }
        let stopped = state.status == crate::player::Status::Stopped;
        self.toggle.set_text(state.toggle_label());
        self.toggle.set_enabled(state.can_play || !stopped);
        self.stop.set_enabled(!stopped);
        self.channels.set_enabled(state.logged_in);
        self.login.set_enabled(!state.logged_in);
        self.logout.set_enabled(state.logged_in);
        let _ = self.icon.set_tooltip(Some(&state.tooltip));
        if let Some((_, active, idle)) = self.icons.iter().find(|(n, ..)| *n == state.network) {
            let _ = self.icon.set_icon(Some(if state.active() { active.clone() } else { idle.clone() }));
        }
        self.state = Some(state);
    }
}
