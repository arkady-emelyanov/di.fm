//! Minimal AudioAddict API client, mirroring the calls made by the official
//! DI.FM browser extension (v2.4.3). AudioAddict runs several stations on one backend
//! (DI.FM, JAZZRADIO.com, ...); each is a "network" with its own catalogue and sessions,
//! but one member account.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::header::USER_AGENT;
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Credentials;

const API_ROOT: &str = "https://api.audioaddict.com/v1";
/// Client credentials of the official AudioAddict apps, sent as HTTP Basic auth to
/// `/member_sessions` (and only there) to create member sessions.
///
/// They identify the client application, not a person, and grant nothing by themselves:
/// creating a session still takes the member's password or API key, and every other
/// request is authenticated with the member's own session key. They aren't a secret:
/// they ship inside every copy of the official apps, where anyone can read them, and are
/// widely reproduced in open-source AudioAddict clients. If AudioAddict ever rotates or
/// blocks them, password login and per-network session creation stop working until they
/// are updated; existing sessions are unaffected.
const APP_CLIENT_USER: &str = "ephemeron";
const APP_CLIENT_PASSWORD: &str = "dayeiph0ne@pp";
/// Page size for catalogue listings, loaded as the user scrolls.
const CATALOG_PAGE: u32 = 48;
pub const APP_VERSION: &str = concat!("difm-tray-", env!("CARGO_PKG_VERSION"));

#[derive(Debug)]
pub enum ApiError {
    /// The session key was revoked (logged out elsewhere, password change, ...).
    StaleSession,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::StaleSession => write!(f, "session is no longer valid, please log in again"),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub id: u64,
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub description_short: String,
    #[serde(default)]
    pub images: Value,
}

impl Channel {
    /// Square artwork URL, sized for a menu icon.
    pub fn image_url(&self, size: u32) -> Option<String> {
        image_url(&self.images, "default", size)
    }
}

/// A curated, finite list of tracks (as opposed to a channel's endless routine).
#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub images: Value,
    /// Genres (channel filters) the playlist belongs to.
    #[serde(default)]
    pub channel_filter_ids: Vec<u64>,
}

impl Playlist {
    pub fn image_url(&self, size: u32) -> Option<String> {
        image_url(&self.images, "square", size).or_else(|| image_url(&self.images, "default", size))
    }
}

/// A radio show; its on-demand episodes are single long mixes.
#[derive(Debug, Clone, Deserialize)]
pub struct Show {
    pub id: u64,
    pub name: String,
    /// "with Armin van Buuren".
    #[serde(default)]
    pub artists_tagline: Option<String>,
    #[serde(default)]
    pub images: Value,
    #[serde(default)]
    pub channel_filter_ids: Vec<u64>,
}

impl Show {
    pub fn image_url(&self, size: u32) -> Option<String> {
        image_url(&self.images, "compact", size).or_else(|| image_url(&self.images, "default", size))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Episode {
    #[serde(default)]
    pub tracks: Vec<Track>,
}

/// Image templates look like `//cdn/.../x.jpg{?size,height,width,quality,pad}`.
fn image_url(images: &Value, variant: &str, size: u32) -> Option<String> {
    let tpl = images.get(variant).and_then(Value::as_str)?;
    let base = tpl.split('{').next()?;
    let base = if base.starts_with("//") { format!("https:{base}") } else { base.to_owned() };
    Some(format!("{base}?size={size}x{size}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Channel,
    Playlist,
    Show,
}

impl MediaKind {
    /// Name of the id parameter in listen/skip events and of the path segment in track
    /// lookups. Show episodes are addressed by track alone.
    fn key(self) -> Option<&'static str> {
        match self {
            MediaKind::Channel => Some("channel"),
            MediaKind::Playlist => Some("playlist"),
            MediaKind::Show => None,
        }
    }
}

/// Something that can be played: a channel or a playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    pub kind: MediaKind,
    pub id: u64,
    pub name: String,
}

/// Skips are rationed per rolling window (e.g. 15 per hour for premium members).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkipAllowance {
    pub remaining: u32,
    /// Unix time when used skips are given back; `None` while none are used.
    pub expires_at: Option<i64>,
}

/// A stream quality the member can pick, e.g. "Ultra (320 kbit/s MP3)".
#[derive(Debug, Clone, Deserialize)]
pub struct Quality {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub position: i64,
    #[serde(default)]
    pub default: bool,
    pub content_format: NamedItem,
    pub content_quality: NamedItem,
}

impl Quality {
    pub fn label(&self) -> String {
        format!("{} ({} {})", self.name, self.content_quality.name, self.content_format.name)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedItem {
    pub name: String,
}

/// Batch of playlist tracks; `last_tracks` marks the end of the playlist.
#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistBatch {
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub last_tracks: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelFilter {
    pub id: u64,
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub display: bool,
    #[serde(default)]
    pub genre: bool,
    #[serde(default)]
    pub channel_ids: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: u64,
    #[serde(default)]
    pub display_artist: Option<String>,
    #[serde(default)]
    pub display_title: Option<String>,
    #[serde(default)]
    pub track: Option<String>,
    #[serde(default)]
    pub content: Option<Content>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Content {
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub url: String,
}

impl Track {
    pub fn url(&self) -> Option<String> {
        let url = &self.content.as_ref()?.assets.first()?.url;
        Some(if url.starts_with("//") { format!("https:{url}") } else { url.clone() })
    }

    pub fn label(&self) -> String {
        match (&self.display_artist, &self.display_title) {
            (Some(a), Some(t)) if !a.is_empty() => format!("{a} – {t}"),
            (_, Some(t)) => t.clone(),
            _ => self.track.clone().unwrap_or_else(|| format!("Track #{}", self.id)),
        }
    }

    /// Signed CDN URLs carry an `exp=YYYY-MM-DDTHH:MM:SSZ` parameter.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        let Some(url) = self.url() else { return true };
        let Some(exp) = url.split(['?', '&']).find_map(|kv| kv.strip_prefix("exp=")) else {
            return false;
        };
        let exp = exp.replace("%3A", ":");
        parse_iso8601_utc(&exp).is_some_and(|t| now_unix >= t)
    }
}

/// Details about the session that took over streaming, returned with HTTP 429.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CompetingStream {
    #[serde(default)]
    pub network: Option<NamedNetwork>,
    #[serde(default)]
    pub device: Option<Device>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NamedNetwork {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Device {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub operating_system: String,
}

impl CompetingStream {
    pub fn describe(&self) -> String {
        let device = match &self.device {
            Some(d) if !d.operating_system.is_empty() => {
                format!("{} on {}", d.name.trim(), d.operating_system.trim())
            }
            Some(d) if !d.name.trim().is_empty() => d.name.trim().to_owned(),
            _ => "another device".to_owned(),
        };
        match &self.network {
            Some(n) if !n.name.is_empty() => format!("{device} ({})", n.name),
            _ => device,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CurrentlyPlaying {
    channel_id: u64,
    track: CurrentTrack,
}

#[derive(Debug, Clone, Deserialize)]
struct CurrentTrack {
    id: u64,
    start_time: String,
    duration: f64,
}

/// The AudioAddict stations the app can play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Di,
    Jazzradio,
}

impl Network {
    pub const ALL: [Network; 2] = [Network::Di, Network::Jazzradio];

    pub fn key(self) -> &'static str {
        match self {
            Network::Di => "di",
            Network::Jazzradio => "jazzradio",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Network::Di => "DI.FM",
            Network::Jazzradio => "JAZZRADIO",
        }
    }

    pub fn site_url(self) -> &'static str {
        match self {
            Network::Di => "https://www.di.fm/",
            Network::Jazzradio => "https://www.jazzradio.com/",
        }
    }

    /// Curated playlists and radio shows only exist on DI.FM.
    pub fn has_playlists(self) -> bool {
        self == Network::Di
    }

    pub fn has_shows(self) -> bool {
        self == Network::Di
    }
}

#[derive(Clone)]
pub struct Api {
    network: Network,
    client: Client,
    /// For the calls that pick the next track: they run as tasks the player aborts when
    /// the listener switches away, which drops the request and its connection.
    async_client: reqwest::Client,
}

impl Api {
    pub fn new(network: Network) -> Result<Self> {
        let client = Client::builder()
            .user_agent(APP_VERSION)
            .connect_timeout(Duration::from_secs(10))
            // Track bodies are streamed for minutes; only bound the connect phase there.
            .timeout(None)
            .build()?;
        let async_client = reqwest::Client::builder().user_agent(APP_VERSION).connect_timeout(Duration::from_secs(10)).build()?;
        Ok(Self { network, client, async_client })
    }

    /// The same client, talking to another network.
    pub fn for_network(&self, network: Network) -> Self {
        Self { network, ..self.clone() }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn http(&self) -> Client {
        self.client.clone()
    }

    fn url(&self, path: &str) -> String {
        format!("{API_ROOT}/{}{path}", self.network.key())
    }

    fn send(req: RequestBuilder) -> Result<Response> {
        Self::send_within(req, Duration::from_secs(20))
    }

    async fn send_async(req: reqwest::RequestBuilder, timeout: Duration) -> Result<reqwest::Response> {
        let resp = req.timeout(timeout).send().await?;
        if resp.status() == StatusCode::FORBIDDEN {
            let body = resp.text().await.unwrap_or_default();
            if body.trim() == "Invalid Session" {
                return Err(ApiError::StaleSession.into());
            }
            bail!("HTTP 403: {body}");
        }
        Ok(resp.error_for_status()?)
    }

    fn send_within(req: RequestBuilder, timeout: Duration) -> Result<Response> {
        let resp = req.timeout(timeout).send()?;
        if resp.status() == StatusCode::FORBIDDEN {
            let body = resp.text().unwrap_or_default();
            if body.trim() == "Invalid Session" {
                return Err(ApiError::StaleSession.into());
            }
            bail!("HTTP 403: {body}");
        }
        Ok(resp.error_for_status()?)
    }

    /// Direct email/password login, as done by the official apps: creates a member
    /// session and returns `{ key, audio_token, member: {...}, ... }`.
    pub fn create_session(&self, username: &str, password: &str) -> Result<Value> {
        let resp = self
            .client
            .post(self.url("/member_sessions"))
            .basic_auth(APP_CLIENT_USER, Some(APP_CLIENT_PASSWORD))
            .json(&json!({ "member_session": { "username": username, "password": password } }))
            .timeout(Duration::from_secs(20))
            .send()?;
        if resp.status() == StatusCode::UNPROCESSABLE_ENTITY {
            bail!("Wrong email or password");
        }
        Ok(resp.error_for_status()?.json()?)
    }

    /// A session on this network for a member already signed in elsewhere: sessions are
    /// per network, but the member's API key is accepted by all of them.
    pub fn create_session_from_api_key(&self, api_key: &str) -> Result<Value> {
        let resp = self
            .client
            .post(self.url("/member_sessions"))
            .basic_auth(APP_CLIENT_USER, Some(APP_CLIENT_PASSWORD))
            .json(&json!({ "member_session": { "api_key": api_key } }))
            .timeout(Duration::from_secs(20))
            .send()?;
        Ok(resp.error_for_status()?.json()?)
    }

    pub fn channels(&self) -> Result<Vec<Channel>> {
        #[derive(Deserialize)]
        struct Filter {
            channels: Vec<Channel>,
        }
        let req = self.client.get(self.url("/channel_filters/key/default")).query(&[("shallow", "0")]);
        let filter: Filter = Self::send(req)?.json().context("parsing channel list")?;
        Ok(filter.channels)
    }

    /// Genre filters ("Trance", "House", ...) with their channel ids.
    pub fn channel_filters(&self) -> Result<Vec<ChannelFilter>> {
        let req = self.client.get(self.url("/channel_filters")).query(&[("shallow", "1")]);
        let filters: Vec<ChannelFilter> = Self::send(req)?.json().context("parsing channel filters")?;
        Ok(filters.into_iter().filter(|f| f.display && f.genre).collect())
    }

    pub fn set_favorite(&self, creds: &Credentials, channel_id: u64, favorite: bool) -> Result<()> {
        let url = self.url(&format!("/members/{}/favorites/channel/{channel_id}", creds.id));
        let req = if favorite { self.client.post(url) } else { self.client.delete(url) };
        Self::send(req.header("X-Session-Key", &creds.session_key).header(USER_AGENT, user_agent(creds)))?;
        Ok(())
    }

    pub fn favorite_channel_ids(&self, creds: &Credentials) -> Result<Vec<u64>> {
        #[derive(Deserialize)]
        struct Fav {
            channel_id: u64,
        }
        let req = self
            .client
            .get(self.url(&format!("/members/{}/favorites/channels", creds.id)))
            .query(&[("order_by", "position")])
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        let favs: Vec<Fav> = Self::send(req)?.json().context("parsing favorites")?;
        Ok(favs.into_iter().map(|f| f.channel_id).collect())
    }

    /// Upcoming tracks for a channel. `tune_in` asks for the track currently on air first;
    /// the backend sometimes takes half a minute to answer that for some channels, so it
    /// gets a short timeout (see [`is_timeout`]) and callers fall back to a plain routine.
    pub async fn channel_routine(&self, creds: &Credentials, channel_id: u64, tune_in: bool) -> Result<Vec<Track>> {
        #[derive(Deserialize)]
        struct Routine {
            tracks: Vec<Track>,
        }
        let req = self
            .async_client
            .get(self.url(&format!("/routines/channel/{channel_id}")))
            .query(&[("tune_in", if tune_in { "1" } else { "0" })])
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        let timeout = Duration::from_secs(if tune_in { 6 } else { 20 });
        let routine: Routine = Self::send_async(req, timeout).await?.json().await.context("parsing channel routine")?;
        Ok(routine.tracks)
    }

    /// Seconds into `track_id` that the live broadcast of `channel_id` currently is.
    pub async fn live_offset(&self, channel_id: u64, track_id: u64) -> Result<f64> {
        let now = self.server_time().await?;
        let req = self.async_client.get(self.url("/currently_playing"));
        let playing: Vec<CurrentlyPlaying> = Self::send_async(req, Duration::from_secs(20)).await?.json().await?;
        let Some(cur) = playing.into_iter().find(|c| c.channel_id == channel_id) else { return Ok(0.0) };
        if cur.track.id != track_id {
            return Ok(0.0);
        }
        let Some(start) = parse_iso8601(&cur.track.start_time) else { return Ok(0.0) };
        let offset = (now - start) as f64;
        Ok(if offset < 0.0 || offset > cur.track.duration { 0.0 } else { offset })
    }

    async fn server_time(&self) -> Result<i64> {
        #[derive(Deserialize)]
        struct Ping {
            time: String,
        }
        let req = self.async_client.get(format!("{API_ROOT}/ping"));
        let ping: Ping = Self::send_async(req, Duration::from_secs(20)).await?.json().await?;
        parse_rfc2822(&ping.time)
            .or_else(|| parse_iso8601(&ping.time))
            .with_context(|| format!("unrecognised server time {:?}", ping.time))
    }

    pub fn record_listen(&self, creds: &Credentials, track_id: u64, kind: MediaKind, media_id: u64) -> Result<()> {
        let mut body = json!({ "track_id": track_id });
        if let Some(key) = kind.key() {
            body[format!("{key}_id")] = json!(media_id);
        }
        let req = self
            .client
            .post(self.url("/listen_history"))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds))
            .json(&body);
        Self::send(req)?;
        Ok(())
    }

    /// Next batch of a playlist's tracks. The server keeps the member's progress
    /// through the playlist (advanced by listen events).
    pub async fn playlist_tracks(&self, creds: &Credentials, playlist_id: u64) -> Result<PlaylistBatch> {
        let req = self
            .async_client
            .post(self.url(&format!("/playlists/{playlist_id}/play")))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        Self::send_async(req, Duration::from_secs(20)).await?.json().await.context("parsing playlist tracks")
    }

    /// Playlists the member follows, in the order the service lists them.
    pub fn followed_playlists(&self, creds: &Credentials) -> Result<Vec<Playlist>> {
        let req = self
            .client
            .get(self.url(&format!("/members/{}/followed_items/playlist", creds.id)))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        Self::send(req)?.json().context("parsing followed playlists")
    }

    pub fn followed_shows(&self, creds: &Credentials) -> Result<Vec<Show>> {
        let req = self
            .client
            .get(self.url(&format!("/members/{}/followed_items/show", creds.id)))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        Self::send(req)?.json().context("parsing followed shows")
    }

    /// A page of a show's on-demand episodes, newest first.
    pub async fn show_episodes(&self, creds: &Credentials, show_id: u64, page: u32, per_page: u32) -> Result<Vec<Episode>> {
        let req = self
            .async_client
            .get(self.url(&format!("/shows/{show_id}/episodes")))
            .query(&[("page", page), ("per_page", per_page)])
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        Self::send_async(req, Duration::from_secs(20)).await?.json().await.context("parsing show episodes")
    }

    /// One page of the playlist catalogue (most popular first), optionally narrowed to
    /// a tag or a search. Returns the playlists and whether more pages follow.
    pub fn playlists_page(&self, page: u32, search: &str, tag: Option<&str>) -> Result<(Vec<Playlist>, bool)> {
        #[derive(Deserialize)]
        struct Search {
            results: Vec<Playlist>,
        }
        let page_s = page.to_string();
        let per_page = CATALOG_PAGE.to_string();
        if !search.is_empty() {
            let req = self
                .client
                .get(self.url("/search/playlists"))
                .query(&[("q", search), ("page", &page_s), ("per_page", &per_page)]);
            let resp = Self::send(req)?;
            let more = page < total_pages(&resp);
            return Ok((resp.json::<Search>().context("parsing playlist search")?.results, more));
        }
        let mut query = vec![
            ("legacy_result", "false"),
            ("order_by", "popularity"),
            ("page", &page_s),
            ("per_page", &per_page),
        ];
        if let Some(tag) = tag {
            query.push(("tag", tag));
        }
        let resp = Self::send(self.client.get(self.url("/playlists")).query(&query))?;
        let more = page < total_pages(&resp);
        Ok((resp.json().context("parsing playlists")?, more))
    }

    /// Playlist tags with the most playlists first.
    pub fn playlist_tags(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Tag {
            name: String,
            #[serde(default)]
            count: u32,
        }
        let mut tags: Vec<Tag> = Self::send(self.client.get(self.url("/playlists/tags")))?.json().context("parsing playlist tags")?;
        tags.sort_by(|a, b| b.count.cmp(&a.count));
        Ok(tags.into_iter().map(|t| t.name).collect())
    }

    /// One page of the show catalogue (running shows first), optionally narrowed to a
    /// genre (channel filter) or a search.
    pub fn shows_page(&self, page: u32, search: &str, genre: Option<u64>) -> Result<(Vec<Show>, bool)> {
        let page_s = page.to_string();
        let per_page = CATALOG_PAGE.to_string();
        let req = if !search.is_empty() {
            self.client
                .get(self.url("/search/shows"))
                .query(&[("q", search), ("page", &page_s), ("per_page", &per_page)])
        } else {
            let genre = genre.map(|g| g.to_string());
            let mut query = vec![
                ("order_by[]", "active"),
                ("order_by[]", "follows_count"),
                ("page", page_s.as_str()),
                ("per_page", per_page.as_str()),
            ];
            if let Some(g) = &genre {
                query.push(("channel_filter_id", g));
            }
            self.client.get(self.url("/shows/index")).query(&query)
        };
        let resp = Self::send(req)?;
        let more = page < total_pages(&resp);
        Ok((resp.json().context("parsing shows")?, more))
    }

    /// Follows or unfollows a playlist or show.
    pub fn set_following(&self, creds: &Credentials, kind: MediaKind, id: u64, follow: bool) -> Result<()> {
        let key = match kind {
            MediaKind::Playlist => "playlist",
            MediaKind::Show => "show",
            MediaKind::Channel => bail!("channels are followed as favourites"),
        };
        let url = self.url(&format!("/members/{}/followed_items/{key}/{id}", creds.id));
        let req = if follow { self.client.post(url) } else { self.client.delete(url) };
        Self::send(req.header("X-Session-Key", &creds.session_key).header(USER_AGENT, user_agent(creds)))?;
        Ok(())
    }

    /// A single track with a fresh asset URL (in the member's current quality).
    pub fn track(&self, creds: &Credentials, track_id: u64, kind: MediaKind, media_id: u64) -> Result<Track> {
        let req = self
            .client
            .get(self.url(&match kind.key() {
                Some(key) => format!("/tracks/{track_id}/{key}/{media_id}"),
                None => format!("/tracks/{track_id}"),
            }))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        Self::send(req)?.json().context("parsing track")
    }

    pub fn skip_allowance(&self, creds: &Credentials) -> Result<SkipAllowance> {
        #[derive(Deserialize)]
        struct Ruleset {
            limit: u32,
            skips_remaining: Option<u32>,
            expires_at: Option<String>,
        }
        let req = self
            .client
            .get(self.url("/skip_rulesets/active"))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        let r: Ruleset = Self::send(req)?.json().context("parsing skip ruleset")?;
        Ok(SkipAllowance {
            remaining: r.skips_remaining.unwrap_or(r.limit),
            expires_at: r.expires_at.as_deref().and_then(parse_iso8601),
        })
    }

    /// Reports a skip; the backend refuses it once the allowance is used up.
    pub fn record_skip(&self, creds: &Credentials, track_id: u64, kind: MediaKind, media_id: u64, skipped_at: f64) -> Result<SkipAllowance> {
        #[derive(Deserialize)]
        struct Skip {
            skips_remaining: u32,
            expires_at: Option<String>,
        }
        let mut body = json!({ "track_id": track_id, "skipped_at": skipped_at });
        if let Some(key) = kind.key() {
            body[format!("{key}_id")] = json!(media_id);
        }
        let req = self
            .client
            .post(self.url("/skip_events"))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds))
            .json(&body);
        let r: Skip = Self::send(req)?.json().context("parsing skip response")?;
        Ok(SkipAllowance { remaining: r.skips_remaining, expires_at: r.expires_at.as_deref().and_then(parse_iso8601) })
    }

    /// Available stream qualities, lowest first.
    pub fn qualities(&self, creds: &Credentials) -> Result<Vec<Quality>> {
        let req = self
            .client
            .get(self.url("/qualities"))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds));
        let mut list: Vec<Quality> = Self::send(req)?.json().context("parsing qualities")?;
        list.sort_by_key(|q| q.position);
        Ok(list)
    }

    /// The member's chosen quality; `None` until one is picked (the default applies).
    pub fn preferred_quality(&self, creds: &Credentials) -> Result<Option<u64>> {
        #[derive(Deserialize)]
        struct Preferred {
            quality_id: u64,
        }
        let resp = self
            .client
            .get(self.url(&format!("/members/{}/preferred_quality", creds.id)))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds))
            .timeout(Duration::from_secs(20))
            .send()?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let pref: Preferred = resp.error_for_status()?.json().context("parsing preferred quality")?;
        Ok(Some(pref.quality_id))
    }

    /// Changes the quality of the asset URLs handed out from now on.
    pub fn set_preferred_quality(&self, creds: &Credentials, quality_id: u64) -> Result<()> {
        let req = self
            .client
            .post(self.url(&format!("/members/{}/preferred_quality", creds.id)))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds))
            .json(&json!({ "quality_id": quality_id }));
        Self::send(req)?;
        Ok(())
    }

    /// Asks the backend whether this session may keep streaming. The service allows a
    /// single concurrent stream per account; when playback starts elsewhere this returns
    /// HTTP 429 together with details about the other session.
    pub fn check_stream(&self, creds: &Credentials) -> Result<Option<CompetingStream>> {
        let resp = self
            .client
            .post(self.url(&format!("/streaming/{}", creds.audio_token)))
            .header("X-Session-Key", &creds.session_key)
            .header(USER_AGENT, user_agent(creds))
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs(20))
            .send()?;
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            return Ok(Some(resp.json().unwrap_or_default()));
        }
        Ok(None)
    }
}

/// Page count of a paginated listing (`paginate-pages` header).
fn total_pages(resp: &Response) -> u32 {
    resp.headers()
        .get("paginate-pages")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

fn user_agent(creds: &Credentials) -> &str {
    creds.user_agent.as_deref().unwrap_or(APP_VERSION)
}

pub fn is_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.downcast_ref::<reqwest::Error>().is_some_and(reqwest::Error::is_timeout))
}

pub fn is_stale_session(err: &anyhow::Error) -> bool {
    matches!(err.downcast_ref::<ApiError>(), Some(ApiError::StaleSession))
}

/// Parses RFC 3339 timestamps such as `2024-11-05T12:32:58.472107517-05:00` or
/// `2024-11-05T17:32:58Z` into Unix seconds.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let (date_time, rest) = s.split_at(19);
    let base = parse_iso8601_utc(&format!("{date_time}Z"))?;
    // Skip fractional seconds.
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match rest {
        "" | "Z" | "z" => 0,
        tz => {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            let tz = tz.get(1..)?.replace(':', "");
            let hours: i64 = tz.get(0..2)?.parse().ok()?;
            let minutes: i64 = tz.get(2..4).unwrap_or("0").parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    Some(base - offset)
}

/// Parses `Thu, 08 Oct 2026 19:18:20 -0400` (the format `/ping` uses) into Unix seconds.
pub fn parse_rfc2822(s: &str) -> Option<i64> {
    let s = s.split_once(',').map_or(s, |(_, rest)| rest);
    let mut parts = s.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = parts.next()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| m.eq_ignore_ascii_case(month))?
        + 1;
    let year: u32 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let tz = match parts.next().unwrap_or("+0000") {
        "GMT" | "UTC" | "Z" => "+0000",
        tz => tz,
    };
    parse_iso8601(&format!("{year:04}-{month:02}-{day:02}T{time}{tz}"))
}

fn parse_iso8601_utc(s: &str) -> Option<i64> {
    // YYYY-MM-DDTHH:MM:SSZ
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    Some(days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Howard Hinnant's days-from-civil algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2024-11-05T17:32:58Z"), Some(1730827978));
        assert_eq!(parse_iso8601("2024-11-05T12:32:58.472107517-05:00"), Some(1730827978));
        assert_eq!(parse_iso8601("2024-11-05T18:32:58+01:00"), Some(1730827978));
        assert_eq!(parse_rfc2822("Tue, 05 Nov 2024 12:32:58 -0500"), Some(1730827978));
        assert_eq!(parse_rfc2822("Tue, 05 Nov 2024 17:32:58 GMT"), Some(1730827978));
    }

    #[test]
    fn detects_expired_tracks() {
        let track: Track = serde_json::from_value(json!({
            "id": 1,
            "content": { "assets": [{ "url": "//cdn.example/a.mp4?exp=2024-11-05T17:32:58Z&hash=x" }] }
        }))
        .unwrap();
        assert!(track.is_expired(1730827978));
        assert!(!track.is_expired(1730827977));
        assert_eq!(track.url().unwrap(), "https://cdn.example/a.mp4?exp=2024-11-05T17:32:58Z&hash=x");
    }
}
