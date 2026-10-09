use anyhow::{Context, Result};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};
use tao::dpi::PhysicalPosition;
use tao::event_loop::EventLoopProxy;

use super::{TrayAction, TrayState, icon_pixels};
use crate::AppEvent;
use crate::api::Network;

struct Sni {
    proxy: EventLoopProxy<AppEvent>,
    state: TrayState,
    /// `(network, active, idle)` icons.
    icons: Vec<(Network, ksni::Icon, ksni::Icon)>,
}

fn to_argb(rgba: &[u8], w: u32, h: u32) -> ksni::Icon {
    let data = rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
    ksni::Icon { width: w as i32, height: h as i32, data }
}

impl Sni {
    fn send(&self, event: AppEvent) {
        let _ = self.proxy.send_event(event);
    }

    fn item(&self, label: &str, enabled: bool, action: TrayAction) -> MenuItem<Self> {
        StandardItem {
            label: label.into(),
            enabled,
            activate: Box::new(move |t: &mut Self| t.send(AppEvent::TrayAction(action))),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for Sni {
    fn id(&self) -> String {
        "difm-tray".into()
    }

    fn title(&self) -> String {
        crate::APP_NAME.into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let Some((_, active, idle)) = self.icons.iter().find(|(n, ..)| *n == self.state.network) else { return Vec::new() };
        vec![if self.state.active() { active.clone() } else { idle.clone() }]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.state.tooltip.clone(), ..Default::default() }
    }

    fn activate(&mut self, x: i32, y: i32) {
        self.send(AppEvent::TrayClick(PhysicalPosition::new(x, y)));
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.send(AppEvent::TrayAction(TrayAction::TogglePause));
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.state;
        let mut items = vec![
            self.item(s.toggle_label(), s.can_play || s.status != crate::player::Status::Stopped, TrayAction::TogglePause),
            self.item("Stop", s.status != crate::player::Status::Stopped, TrayAction::Stop),
            MenuItem::Separator,
            self.item("All channels…", s.logged_in, TrayAction::OpenChannels),
            MenuItem::Separator,
        ];
        if s.logged_in {
            items.push(self.item("Log out", true, TrayAction::Logout));
        } else {
            items.push(self.item("Log in…", true, TrayAction::Login));
        }
        items.push(self.item("Quit", true, TrayAction::Quit));
        items
    }
}

pub struct Tray {
    handle: Handle<Sni>,
    state: TrayState,
}

impl Tray {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
        let icons = Network::ALL
            .into_iter()
            .map(|n| {
                let (active, idle, w, h) = icon_pixels(n)?;
                Ok((n, to_argb(&active, w, h), to_argb(&idle, w, h)))
            })
            .collect::<Result<_>>()?;
        let sni = Sni { proxy, state: TrayState::default(), icons };
        let handle = sni.spawn().context("registering StatusNotifierItem (is a tray host running?)")?;
        Ok(Self { handle, state: TrayState::default() })
    }

    pub fn update(&mut self, state: TrayState) {
        if state == self.state {
            return;
        }
        self.state = state.clone();
        self.handle.update(move |t| t.state = state);
    }
}
