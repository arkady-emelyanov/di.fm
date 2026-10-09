//! Login window.
//!
//! The default path is a local email/password form that authenticates straight against
//! the AudioAddict API (`/member_sessions`, like the official apps); nothing from the
//! website is loaded.
//!
//! Accounts created through Google or Apple sign-in have no password, so the form also
//! offers "Sign in via website". That path links the account the same way the official
//! browser extension does: its content script on www.di.fm polls the page with
//! `EXTENSION_SYNC_STATUS` until the site's sync module answers `EXTENSION_SYNC_READY`,
//! then sends `QUERY_USER_INFO` and receives the logged-in member as `USER_INFO`. We
//! inject the same handshake and forward the member over wry's IPC bridge.

use std::thread;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tao::dpi::LogicalSize;
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::AppEvent;
use crate::api::Api;
use crate::config::{self, Credentials};

const EXTENSION_VERSION: &str = "2.4.3";

const FORM_HTML: &str = include_str!("login.html");

const HANDSHAKE_JS: &str = r#"
(function () {
  if (window.top !== window || !/(^|\.)(di\.fm|jazzradio\.com)$/.test(window.location.hostname)) return;
  var linked = false;
  function post(type) { window.postMessage({ type: type }, window.location.origin); }
  window.addEventListener('message', function (e) {
    if (e.source !== window || !e.data || !e.data.type) return;
    if (e.data.type === 'EXTENSION_SYNC_READY') {
      clearInterval(statusPoll);
      post('QUERY_USER_INFO');
      // Logging in may not reload the page; keep asking until we get a member.
      setInterval(function () { if (!linked) post('QUERY_USER_INFO'); }, 2000);
    } else if (e.data.type === 'USER_INFO' && e.data.user && e.data.user.session_key && !linked) {
      linked = true;
      window.postMessage({ type: 'LINK_SUCCESS', version: '__VERSION__' }, window.location.origin);
      window.ipc.postMessage(JSON.stringify({ kind: 'site', user: e.data.user, userAgent: navigator.userAgent }));
    }
  });
  var statusPoll = setInterval(function () { post('EXTENSION_SYNC_STATUS'); }, 500);
})();
"#;

pub struct LoginWindow {
    // Field order matters: the webview must be dropped before its window and context.
    webview: WebView,
    window: Window,
    _context: WebContext,
}

impl LoginWindow {
    pub fn open(target: &EventLoopWindowTarget<AppEvent>, proxy: EventLoopProxy<AppEvent>, api: Api) -> Result<Self> {
        let window = WindowBuilder::new()
            .with_title(format!("Log in to {}", api.network().name()))
            .with_window_icon(crate::ui::app_icon())
            .with_inner_size(LogicalSize::new(460.0, 640.0))
            .build(target)
            .context("creating login window")?;

        // Persist website cookies so linking via the website later doesn't require
        // signing in there again.
        let mut context = WebContext::new(config::data_dir().ok().map(|d| d.join("webview")));
        let builder = WebViewBuilder::new_with_web_context(&mut context)
            .with_html(FORM_HTML.replace("__NAME__", api.network().name()).replace("__SITE__", api.network().site_url()))
            .with_initialization_script(HANDSHAKE_JS.replace("__VERSION__", EXTENSION_VERSION))
            .with_ipc_handler(move |req| handle_ipc(req.body(), &proxy, &api));

        let webview = crate::ui::build_webview(builder, &window)?;

        Ok(Self { webview, window, _context: context })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn focus(&self) {
        self.window.set_visible(true);
        self.window.set_focus();
    }

    /// Shows a password-login failure in the form.
    pub fn show_error(&self, message: &str) {
        let js = format!("window.loginFailed && window.loginFailed({});", json!(message));
        if let Err(e) = self.webview.evaluate_script(&js) {
            log::warn!("showing login error: {e}");
        }
    }
}

fn handle_ipc(body: &str, proxy: &EventLoopProxy<AppEvent>, api: &Api) {
    let Ok(msg) = serde_json::from_str::<Value>(body) else { return };
    match msg.get("kind").and_then(Value::as_str) {
        Some("password") => {
            let field = |k: &str| msg.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_owned();
            let (email, password) = (field("email"), msg.get("password").and_then(Value::as_str).unwrap_or_default().to_owned());
            let (proxy, api) = (proxy.clone(), api.clone());
            thread::spawn(move || {
                let result = api.create_session(&email, &password).and_then(|session| from_session(&session));
                let _ = proxy.send_event(match result {
                    Ok(creds) => AppEvent::LoggedIn(api.network(), creds),
                    Err(e) => AppEvent::LoginFailed(format!("{e:#}")),
                });
            });
        }
        Some("site") => {
            let user_agent = msg.get("userAgent").and_then(Value::as_str).map(str::to_owned);
            match msg.get("user").context("user missing").and_then(|u| from_member(u, user_agent)) {
                Ok(creds) => {
                    let _ = proxy.send_event(AppEvent::LoggedIn(api.network(), creds));
                }
                Err(e) => log::warn!("ignoring user info from website: {e:#}"),
            }
        }
        _ => {}
    }
}

/// Builds credentials from a `/member_sessions` response.
pub fn from_session(session: &Value) -> Result<Credentials> {
    let mut member = session.get("member").cloned().context("login response has no member")?;
    let obj = member.as_object_mut().context("member is not an object")?;
    for (from, to) in [("key", "session_key"), ("audio_token", "audio_token")] {
        if let Some(v) = session.get(from) {
            obj.insert(to.to_owned(), v.clone());
        }
    }
    from_member(&member, None)
}

/// Builds credentials from a member record, as returned by `/members/authenticate` or
/// handed over by the website.
fn from_member(member: &Value, user_agent: Option<String>) -> Result<Credentials> {
    let field = |name: &str| member.get(name).and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
    let id = match member.get("id") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    };
    let keys = || member.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
    let creds = Credentials {
        id: id.with_context(|| format!("member id missing (got: {})", keys()))?,
        session_key: field("session_key"),
        audio_token: field("audio_token"),
        listen_key: field("listen_key"),
        api_key: field("api_key"),
        user_agent,
    };
    anyhow::ensure!(!creds.session_key.is_empty(), "session key missing (got: {})", keys());
    if creds.audio_token.is_empty() {
        log::warn!("no audio token provided; stream takeover detection is disabled");
    }
    Ok(creds)
}
