//! Playback engine.
//!
//! DI.FM premium playback (as done by the browser extension) is track based: the API
//! hands out a "routine" of upcoming tracks for a channel (or batches of a playlist's
//! tracks), each a signed CDN URL. We queue them back to back in a rodio player, tune
//! into the live position of a channel's first track, report listens and skips, and
//! poll the backend so that starting playback on another device stops this one.
//!
//! Picking and opening the next track involves several requests, some of which the
//! backend can be very slow to answer. That work runs as a task on a small Tokio runtime
//! so the engine stays responsive; switching to something else aborts the task, which
//! cancels its in-flight API requests.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use rodio::decoder::{Decoder, DecoderBuilder};
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::api::{self, Api, CompetingStream, Media, MediaKind, SkipAllowance, Track};
use crate::config::Credentials;
use crate::stream::HttpRangeReader;

const TICK: Duration = Duration::from_millis(250);
/// Same cadence as the extension's StreamValidator.
const STREAM_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// After a long pause, resuming the stale track makes little sense for a radio.
const RETUNE_AFTER_PAUSE: Duration = Duration::from_secs(30 * 60);
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// Show episodes are fetched a few at a time, like the extension does.
const EPISODES_PER_PAGE: u32 = 3;

pub enum Command {
    Play(Media),
    TogglePause,
    /// Moves on to the next track, if the skip allowance permits.
    Skip,
    /// Re-fetches the current track so a changed stream quality applies right away.
    Reload,
    Stop,
    SetVolume(f32),
    SetCredentials(Option<Credentials>),
    /// Switches to another network: stops playback and forgets the selection.
    SetNetwork { api: Api, creds: Option<Credentials> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Debug, Clone)]
pub enum Event {
    State { status: Status, media: Option<Media>, track: Option<String> },
    Skips(SkipAllowance),
    SkipFailed(String),
    /// A playlist played through to its last track.
    Ended(String),
    /// The backend reported that this account started streaming somewhere else.
    TakenOver(CompetingStream),
    StaleSession,
    Error(String),
}

#[derive(Clone)]
pub struct PlayerHandle {
    tx: Sender<Msg>,
}

impl PlayerHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(Msg::Command(cmd));
    }
}

pub fn spawn(api: Api, creds: Option<Credentials>, volume: f32, emit: impl Fn(Event) + Send + 'static) -> PlayerHandle {
    let (tx, rx) = mpsc::channel();
    let engine_tx = tx.clone();
    thread::Builder::new()
        .name("player".into())
        .spawn(move || Engine::new(api, creds, volume, Box::new(emit), engine_tx).run(rx))
        .expect("spawning player thread");
    PlayerHandle { tx }
}

enum Msg {
    Command(Command),
    Loaded(Loaded),
}

/// Where the queue stands; a load task takes it over and hands it back.
struct Plan {
    /// Tracks fetched from the routine but not yet handed to the player.
    upcoming: VecDeque<Track>,
    /// The playlist has no tracks beyond `upcoming`.
    playlist_exhausted: bool,
    /// Next page of show episodes to fetch.
    episode_page: u32,
}

type Opened = Decoder<HttpRangeReader>;

/// Outcome of a load task. `None` means a playlist has nothing left.
struct Loaded {
    generation: u64,
    plan: Plan,
    resumed: bool,
    result: Result<Option<(Track, Opened)>>,
}

struct Output {
    // Keep the device open only while something is playing, so an idle tray app
    // doesn't hold an audio stream.
    _sink: MixerDeviceSink,
    player: Player,
}

struct Engine {
    api: Api,
    runtime: Runtime,
    tx: Sender<Msg>,
    /// The task loading the next track, if any.
    load: Option<JoinHandle<()>>,
    /// Bumped whenever queued work becomes moot, so late results are ignored.
    generation: u64,
    creds: Option<Credentials>,
    emit: Box<dyn Fn(Event) + Send>,
    volume: f32,
    output: Option<Output>,
    media: Option<Media>,
    status: Status,
    upcoming: VecDeque<Track>,
    playlist_exhausted: bool,
    episode_page: u32,
    /// Start the next track at the channel's live position (fresh tune-in).
    tune_in: bool,
    /// Start the next track at this offset instead (reload after a quality change).
    resume_at: Option<f64>,
    /// Tracks handed to the player, front = currently audible.
    queued: VecDeque<Track>,
    current: Option<Track>,
    paused_at: Option<Instant>,
    last_stream_check: Instant,
    failures: u32,
    retry_at: Option<Instant>,
}

impl Engine {
    fn new(api: Api, creds: Option<Credentials>, volume: f32, emit: Box<dyn Fn(Event) + Send>, tx: Sender<Msg>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("player-load")
            .enable_all()
            .build()
            .expect("starting the loader runtime");
        Self {
            api,
            runtime,
            tx,
            load: None,
            generation: 0,
            creds,
            emit,
            volume,
            output: None,
            media: None,
            status: Status::Stopped,
            upcoming: VecDeque::new(),
            playlist_exhausted: false,
            episode_page: 1,
            tune_in: true,
            resume_at: None,
            queued: VecDeque::new(),
            current: None,
            paused_at: None,
            last_stream_check: Instant::now(),
            failures: 0,
            retry_at: None,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Msg::Command(cmd)) => self.handle(cmd),
                Ok(Msg::Loaded(loaded)) => self.on_loaded(loaded),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.status == Status::Playing || self.status == Status::Loading {
                if let Err(e) = self.tick() {
                    self.fail(e);
                }
            }
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Play(media) => {
                self.reset();
                self.media = Some(media);
                self.set_status(Status::Loading);
            }
            Command::TogglePause => match self.status {
                Status::Playing => {
                    if let Some(out) = &self.output {
                        out.player.pause();
                    }
                    self.paused_at = Some(Instant::now());
                    self.set_status(Status::Paused);
                }
                Status::Paused => self.resume(),
                Status::Stopped if self.media.is_some() => self.set_status(Status::Loading),
                _ => {}
            },
            Command::Skip => self.skip(),
            Command::Reload => self.reload(),
            Command::Stop => {
                self.reset();
                self.set_status(Status::Stopped);
            }
            Command::SetVolume(v) => {
                self.volume = v;
                if let Some(out) = &self.output {
                    out.player.set_volume(v as _);
                }
            }
            Command::SetCredentials(creds) => {
                let logged_out = creds.is_none();
                self.creds = creds;
                if logged_out {
                    self.reset();
                    self.set_status(Status::Stopped);
                }
            }
            Command::SetNetwork { api, creds } => {
                self.reset();
                (self.api, self.creds, self.media) = (api, creds, None);
                self.set_status(Status::Stopped);
            }
        }
    }

    fn resume(&mut self) {
        let long_pause = self.paused_at.is_some_and(|t| t.elapsed() > RETUNE_AFTER_PAUSE);
        if long_pause || self.output.is_none() {
            self.reset();
            self.set_status(Status::Loading);
            return;
        }
        if let Some(out) = &self.output {
            out.player.play();
        }
        self.paused_at = None;
        // Re-validate right away: someone may have started streaming during the pause.
        self.last_stream_check = Instant::now() - STREAM_CHECK_INTERVAL;
        self.set_status(Status::Playing);
    }

    fn skip(&mut self) {
        if self.status != Status::Playing {
            return;
        }
        let (Some(media), Some(track), Some(out), Ok(creds)) =
            (self.media.clone(), self.current.clone(), self.output.as_ref(), self.creds())
        else {
            return;
        };
        let at = out.player.get_pos().as_secs_f64();
        // Skipping a show episode is free; tracks of channels and playlists are rationed.
        if media.kind != MediaKind::Show {
            match self.api.record_skip(&creds, track.id, media.kind, media.id, at) {
                Ok(allowance) => (self.emit)(Event::Skips(allowance)),
                Err(e) if api::is_stale_session(&e) => return self.fail(e),
                Err(e) => {
                    log::warn!("skip refused: {e:#}");
                    (self.emit)(Event::SkipFailed(format!("{e:#}")));
                    return;
                }
            }
        }
        log::debug!("skipping {}", track.label());
        out.player.skip_one();
        self.queued.pop_front();
        self.current = None;
        // A skip moves on through the routine, it doesn't tune back into the live track.
        self.tune_in = false;
        if self.queued.is_empty() {
            self.set_status(Status::Loading);
        } else {
            self.on_track_started(&media, true);
        }
    }

    fn reload(&mut self) {
        // Tracks fetched from now on come in the new quality.
        self.cancel_load();
        self.upcoming.clear();
        if self.status != Status::Playing {
            return;
        }
        let (Some(media), Some(track), Some(out), Ok(creds)) =
            (self.media.clone(), self.current.clone(), self.output.as_ref(), self.creds())
        else {
            return;
        };
        let at = out.player.get_pos().as_secs_f64();
        let fresh = match self.api.track(&creds, track.id, media.kind, media.id) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("reloading current track: {e:#}");
                return;
            }
        };
        self.reset();
        self.upcoming.push_back(fresh);
        self.tune_in = false;
        self.resume_at = Some(at);
        self.set_status(Status::Loading);
    }

    /// Drops everything queued and releases the audio device. Keeps the media so
    /// play/pause can restart it.
    fn reset(&mut self) {
        self.cancel_load();
        self.output = None;
        self.upcoming.clear();
        self.playlist_exhausted = false;
        self.episode_page = 1;
        self.tune_in = true;
        self.resume_at = None;
        self.queued.clear();
        self.current = None;
        self.paused_at = None;
        self.failures = 0;
        self.retry_at = None;
    }

    fn cancel_load(&mut self) {
        if let Some(task) = self.load.take() {
            task.abort();
        }
        self.generation += 1;
    }

    fn set_status(&mut self, status: Status) {
        self.status = status;
        self.notify();
    }

    fn notify(&self) {
        (self.emit)(Event::State {
            status: self.status.clone(),
            media: self.media.clone(),
            track: self.current.as_ref().map(Track::label),
        });
    }

    fn fail(&mut self, e: anyhow::Error) {
        if api::is_stale_session(&e) {
            self.reset();
            self.set_status(Status::Stopped);
            (self.emit)(Event::StaleSession);
            return;
        }
        log::warn!("playback: {e:#}");
        self.failures += 1;
        let backoff = Duration::from_secs((1u64 << self.failures.min(5)).min(30));
        self.retry_at = Some(Instant::now() + backoff);
        // While a track is still audible, keep retrying in the background.
        if self.queued.is_empty() && self.failures >= MAX_CONSECUTIVE_FAILURES {
            self.reset();
            self.set_status(Status::Stopped);
            (self.emit)(Event::Error(format!("Unable to play right now: {e:#}")));
        }
    }

    fn creds(&self) -> Result<Credentials> {
        self.creds.clone().ok_or_else(|| anyhow!("not logged in"))
    }

    fn tick(&mut self) -> Result<()> {
        let media = self.media.clone().context("nothing selected to play")?;

        // Notice when the player moved on to the next queued track.
        if let Some(out) = &self.output {
            let mut advanced = false;
            while self.queued.len() > out.player.len() {
                self.queued.pop_front();
                advanced = true;
            }
            if advanced {
                self.on_track_started(&media, true);
            }
        }

        // Keep one track queued behind the current one for gapless playback.
        let backing_off = self.retry_at.is_some_and(|t| Instant::now() < t);
        if self.queued.len() < 2 && !backing_off && self.load.is_none() {
            let finished = media.kind == MediaKind::Playlist && self.playlist_exhausted && self.upcoming.is_empty();
            if !finished {
                self.start_load(&media)?;
            } else if self.queued.is_empty() {
                self.finish(&media);
                return Ok(());
            }
        }

        if self.status == Status::Playing && self.last_stream_check.elapsed() >= STREAM_CHECK_INTERVAL {
            self.last_stream_check = Instant::now();
            self.check_stream();
        }
        Ok(())
    }

    fn on_track_started(&mut self, media: &Media, record: bool) {
        self.current = self.queued.front().cloned();
        self.notify();
        let (Some(track), Ok(creds), true) = (self.current.clone(), self.creds(), record) else { return };
        let (api, kind, id) = (self.api.clone(), media.kind, media.id);
        thread::spawn(move || {
            if let Err(e) = api.record_listen(&creds, track.id, kind, id) {
                log::debug!("recording listen: {e:#}");
            }
        });
    }

    fn check_stream(&mut self) {
        let Ok(creds) = self.creds() else { return };
        match self.api.check_stream(&creds) {
            Ok(Some(other)) => {
                log::info!("streaming taken over by {}", other.describe());
                if let Some(out) = &self.output {
                    out.player.pause();
                }
                self.paused_at = Some(Instant::now());
                self.set_status(Status::Paused);
                (self.emit)(Event::TakenOver(other));
            }
            Ok(None) => {}
            // Like the extension, treat network errors as "still allowed".
            Err(e) => log::debug!("stream check failed: {e:#}"),
        }
    }

    fn start_load(&mut self, media: &Media) -> Result<()> {
        let creds = self.creds()?;
        let plan = Plan {
            upcoming: std::mem::take(&mut self.upcoming),
            playlist_exhausted: self.playlist_exhausted,
            episode_page: self.episode_page,
        };
        let known: Vec<u64> = self.queued.iter().map(|t| t.id).collect();
        let (api, media, tx, generation) = (self.api.clone(), media.clone(), self.tx.clone(), self.generation);
        let (tune_in, resume_at) = (self.tune_in, self.resume_at);
        self.load = Some(self.runtime.spawn(async move {
            let mut plan = plan;
            let result = next_track(&api, &creds, &media, &mut plan, tune_in, resume_at, &known).await;
            let _ = tx.send(Msg::Loaded(Loaded { generation, plan, resumed: resume_at.is_some(), result }));
        }));
        Ok(())
    }

    fn on_loaded(&mut self, loaded: Loaded) {
        if loaded.generation != self.generation {
            return;
        }
        self.load = None;
        let Some(media) = self.media.clone() else { return };
        let Plan { upcoming, playlist_exhausted, episode_page } = loaded.plan;
        (self.upcoming, self.playlist_exhausted, self.episode_page) = (upcoming, playlist_exhausted, episode_page);
        match loaded.result {
            Err(e) => self.fail(e),
            Ok(None) if self.queued.is_empty() => self.finish(&media),
            Ok(None) => {}
            Ok(Some((track, source))) => {
                if let Err(e) = self.append(track, source) {
                    return self.fail(e);
                }
                if self.queued.len() == 1 {
                    // A reloaded track was already reported when it first started.
                    self.on_track_started(&media, !loaded.resumed);
                    self.last_stream_check = Instant::now();
                    self.set_status(Status::Playing);
                }
            }
        }
    }

    fn append(&mut self, track: Track, source: Opened) -> Result<()> {
        if self.output.is_none() {
            let mut sink = DeviceSinkBuilder::open_default_sink().context("opening audio output")?;
            sink.log_on_drop(false);
            let player = Player::connect_new(sink.mixer());
            player.set_volume(self.volume as _);
            self.output = Some(Output { _sink: sink, player });
        }
        let out = self.output.as_ref().expect("just created");
        out.player.append(source);
        self.queued.push_back(track);
        self.tune_in = false;
        self.resume_at = None;
        self.failures = 0;
        self.retry_at = None;
        Ok(())
    }

    fn finish(&mut self, media: &Media) {
        log::info!("reached the end of {}", media.name);
        self.reset();
        self.set_status(Status::Stopped);
        (self.emit)(Event::Ended(media.name.clone()));
    }
}

/// Picks the next track (fetching more from the backend when needed) and opens it.
/// Returns `None` when a playlist has nothing left.
async fn next_track(
    api: &Api,
    creds: &Credentials,
    media: &Media,
    plan: &mut Plan,
    tune_in: bool,
    resume_at: Option<f64>,
    known: &[u64],
) -> Result<Option<(Track, Opened)>> {
    let now = unix_now();
    plan.upcoming.retain(|t| !t.is_expired(now));
    if plan.upcoming.is_empty() {
        let tracks = match media.kind {
            MediaKind::Channel => match api.channel_routine(creds, media.id, tune_in).await {
                Err(e) if tune_in && api::is_timeout(&e) => {
                    log::warn!("tuning in to {} timed out, starting with its upcoming tracks", media.name);
                    api.channel_routine(creds, media.id, false).await?
                }
                result => result?,
            },
            MediaKind::Playlist if plan.playlist_exhausted => return Ok(None),
            MediaKind::Playlist => {
                let batch = api.playlist_tracks(creds, media.id).await?;
                plan.playlist_exhausted = batch.last_tracks;
                batch.tracks
            }
            MediaKind::Show => {
                let episodes = api.show_episodes(creds, media.id, plan.episode_page, EPISODES_PER_PAGE).await?;
                // After the oldest episode, start over from the newest.
                let full = episodes.len() as u32 >= EPISODES_PER_PAGE;
                plan.episode_page = if full { plan.episode_page + 1 } else { 1 };
                episodes.into_iter().filter_map(|e| e.tracks.into_iter().next()).collect()
            }
        };
        plan.upcoming.extend(tracks.into_iter().filter(|t| !known.contains(&t.id) && !t.is_expired(now)));
        if plan.upcoming.is_empty() {
            if plan.playlist_exhausted {
                return Ok(None);
            }
            return Err(anyhow!("{} returned no playable tracks", media.name));
        }
    }
    // A track that fails to open is dropped; the next attempt tries the following one.
    let track = plan.upcoming.pop_front().expect("checked above");
    let offset = if let Some(at) = resume_at {
        at
    } else if tune_in && media.kind == MediaKind::Channel {
        api.live_offset(media.id, track.id).await.unwrap_or_else(|e| {
            log::warn!("cannot determine live position, starting track from the beginning: {e:#}");
            0.0
        })
    } else {
        0.0
    };
    // The decoder reads through a blocking HTTP reader. If the task is aborted meanwhile,
    // this finishes in the background and its result is dropped.
    let (http, opening) = (api.http(), track.clone());
    let source = tokio::task::spawn_blocking(move || open_track(http, &opening, offset))
        .await?
        .with_context(|| format!("opening track {}", track.id))?;
    Ok(Some((track, source)))
}

fn open_track(http: reqwest::blocking::Client, track: &Track, offset: f64) -> Result<Opened> {
    let url = track.url().context("track has no audio asset")?;
    log::debug!("opening {} at {offset:.0}s: {url}", track.label());
    let reader = HttpRangeReader::open(http, &url)?;
    let len = reader.len();
    let hint = url.split('?').next().and_then(|p| p.rsplit('.').next()).unwrap_or("mp4").to_owned();
    let mut decoder = DecoderBuilder::new()
        .with_data(reader)
        .with_byte_len(len)
        .with_seekable(true)
        .with_hint(&hint)
        .build()
        .context("decoding track")?;
    if offset > 1.0 {
        if let Err(e) = decoder.try_seek(Duration::from_secs_f64(offset)) {
            log::warn!("could not tune in at {offset:.0}s: {e}");
        }
    }
    Ok(decoder)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
