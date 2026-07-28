// Sehcontrol ScreenCam — Fase 1 MVP (see docs/SCREENCAM_PLAN.md).
//
// Turns one monitor of this machine into an RTSP source a DVR/NVR (or, for
// this MVP, VLC) can pull directly: `rtsp://<this-machine-ip>:8554/live/main`.
// Video never goes through the Sehcontrol panel/server — this module only
// captures, encodes and serves RTP; the panel-driven licensing, policy and
// PIN-protected local config described in the plan's Fase 3/4 are not wired
// up yet on purpose, so this can be validated against VLC and a real NVR
// first (Fase 1/2 acceptance criteria) before any of that is built on top.
//
// Decision from docs/SCREENCAM_PLAN.md §7: option A — hardware H.264 only,
// no software fallback. A machine with no hardware H.264 encoder (no GPU, or
// a GPU whose driver isn't installed) is expected to fail loudly with
// `last_error: "no_h264_encoder"` rather than silently degrading to a codec
// no NVR can ingest.
//
// Known Fase-1 simplification: this module opens its **own** capturer
// instance instead of sharing frames with an active remote-control session
// (docs/SCREENCAM_PLAN.md §3.4). Running ScreenCam and a live remote session
// against the same monitor at the same time is not yet safe — that fan-out
// refactor is planned before this is considered done, not before it's first
// tested against VLC/an NVR.

mod auth;
mod display;
mod onvif;
mod rtp;
mod rtsp;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hbb_common::{
    anyhow::anyhow,
    bail, config, log,
    message_proto::video_frame,
    serde_derive::{Deserialize, Serialize},
    ResultType,
};
use scrap::{
    codec::{Encoder, EncoderCfg},
    hwcodec::{HwRamEncoder, HwRamEncoderConfig},
    CodecFormat, TraitCapturer,
};

use rtsp::Session;

/// Persisted alongside the rest of Sehcontrol's local config (see [`load`](Self::load)),
/// since there's no admin panel or in-app settings page wired up for this yet
/// (that needs new `flutter_ffi.rs` bridge functions, which in turn need
/// `flutter_rust_bridge_codegen` to regenerate `flutter/lib/generated_bridge.dart` —
/// not available in every environment touching this code, so hand-editing the
/// config file is the supported way to change these until that lands).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenCamConfig {
    pub monitor_index: usize,
    pub fps: u32,
    pub rtsp_port: u16,
    /// 0.0-1.0, forwarded to the same quality->bitrate curve the remote
    /// desktop encoders already use (see HwRamEncoder::bitrate in
    /// libs/scrap/src/common/hwcodec.rs) — 0.5 lands in the "Media" range
    /// the plan's UI mock calls for.
    pub quality: f32,
    /// HTTP port for the ONVIF device/media SOAP services (docs/SCREENCAM_PLAN.md
    /// Fase 6). Deliberately not 80 — that's what most ONVIF cameras use, but
    /// it risks colliding with something else already on the host, and NVR
    /// software is expected to read the real address from WS-Discovery's
    /// `XAddrs` rather than assume a fixed port.
    pub onvif_port: u16,
    /// Stable device identity for WS-Discovery's `EndpointReference` and
    /// ONVIF's `GetDeviceInformation` (`SerialNumber`). Generated once (see
    /// [`load`](Self::load)) and persisted, so re-discovery after a restart
    /// reports the same identity instead of looking like a new device to an
    /// NVR that remembers cameras by this UUID.
    pub device_uuid: String,
}

impl Default for ScreenCamConfig {
    fn default() -> Self {
        Self {
            monitor_index: 0,
            fps: 10,
            rtsp_port: 8554,
            quality: 0.5,
            onvif_port: 8080,
            device_uuid: String::new(),
        }
    }
}

impl ScreenCamConfig {
    const CONFIG_SUFFIX: &'static str = "_screencam";

    /// Loads from disk, falling back to [`Default`] for any field missing or
    /// invalid (a fresh install, or a config file from before some field
    /// existed) — never fails, matching the rest of this codebase's local
    /// config loaders (e.g. `HwCodecConfig::get()`).
    ///
    /// File location follows the same convention as Sehcontrol's other local
    /// config (`hbb_common::config::Config::file_`) — on Windows that's
    /// `%APPDATA%\Sehcontrol\Sehcontrol_screencam.toml`; on Linux/macOS the
    /// platform-appropriate config dir with the same `_screencam` suffix.
    /// Edit it by hand and restart the `--server` process (or Windows) to
    /// apply changes — there is no live-reload yet.
    pub fn load() -> Self {
        let mut cfg = config::common_load::<Self>(Self::CONFIG_SUFFIX);
        if cfg.device_uuid.is_empty() {
            cfg.device_uuid = uuid::Uuid::new_v4().to_string();
            cfg.store();
        }
        cfg
    }

    pub fn store(&self) {
        config::common_store(self, Self::CONFIG_SUFFIX);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StreamDescriptorState {
    epoch: u64,
    width: usize,
    height: usize,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    idr_ready: bool,
}

impl StreamDescriptorState {
    fn new() -> Self {
        Self {
            epoch: 0,
            width: 0,
            height: 0,
            sps: None,
            pps: None,
            idr_ready: false,
        }
    }

    fn invalidate(&mut self) -> u64 {
        self.width = 0;
        self.height = 0;
        self.sps = None;
        self.pps = None;
        self.idr_ready = false;
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }

    fn is_ready(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.sps.is_some()
            && self.pps.is_some()
            && self.idr_ready
    }

    fn set_dimensions(&mut self, epoch: u64, width: usize, height: usize) -> bool {
        if self.epoch != epoch {
            return false;
        }
        self.width = width;
        self.height = height;
        true
    }

    fn apply_access_unit(&mut self, epoch: u64, nals: &[&[u8]]) -> bool {
        if self.epoch != epoch {
            return false;
        }
        for nal in nals {
            match rtp::nal_unit_type(nal) {
                rtp::NAL_TYPE_SPS => self.sps = Some(nal.to_vec()),
                rtp::NAL_TYPE_PPS => self.pps = Some(nal.to_vec()),
                rtp::NAL_TYPE_IDR if rtp::is_complete_idr_nal(nal) => self.idr_ready = true,
                _ => {}
            }
        }
        true
    }
}

pub struct SharedState {
    pub sessions: Mutex<Vec<Arc<Session>>>,
    stream_descriptor: Mutex<StreamDescriptorState>,
    last_confirmed_resolution: Mutex<Option<(usize, usize)>>,
    display_selection: Mutex<display::DisplaySelectionState>,
    reconfigure_generation: AtomicU64,
}

impl SharedState {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            stream_descriptor: Mutex::new(StreamDescriptorState::new()),
            last_confirmed_resolution: Mutex::new(None),
            display_selection: Mutex::new(display::DisplaySelectionState::new(None, true)),
            reconfigure_generation: AtomicU64::new(0),
        }
    }

    fn apply_display_inventory<D>(
        &self,
        inventory: &display::DisplayInventory<D>,
    ) -> (display::DisplayStateUpdate, u64) {
        let mut selection = self.display_selection.lock().unwrap();
        let update = selection.apply(inventory);
        if update.desired_changed {
            advance_generation(&self.reconfigure_generation);
        }
        let generation = self.reconfigure_generation.load(Ordering::SeqCst);
        (update, generation)
    }

    fn activate_display_for_generation(
        &self,
        generation: u64,
        resolution: &display::DisplayResolution,
    ) -> bool {
        let mut selection = self.display_selection.lock().unwrap();
        if self.reconfigure_generation.load(Ordering::SeqCst) != generation {
            return false;
        }
        selection.activate(resolution);
        true
    }

    fn deactivate_display(&self) {
        self.display_selection.lock().unwrap().deactivate();
    }

    #[cfg(test)]
    fn display_snapshot(&self) -> display::DisplayRuntimeState {
        self.display_selection.lock().unwrap().snapshot()
    }

    fn stream_descriptor(&self) -> StreamDescriptorState {
        self.stream_descriptor.lock().unwrap().clone()
    }

    #[cfg(test)]
    fn stream_dimensions(&self) -> (usize, usize) {
        let descriptor = self.stream_descriptor.lock().unwrap();
        (descriptor.width, descriptor.height)
    }

    fn onvif_resolution(&self) -> Option<(usize, usize)> {
        let current = {
            let descriptor = self.stream_descriptor.lock().unwrap();
            descriptor
                .is_ready()
                .then_some((descriptor.width, descriptor.height))
        };
        current.or_else(|| *self.last_confirmed_resolution.lock().unwrap())
    }

    fn stream_epoch(&self) -> u64 {
        self.stream_descriptor.lock().unwrap().epoch
    }

    fn set_stream_dimensions(&self, epoch: u64, width: usize, height: usize) -> bool {
        self.stream_descriptor
            .lock()
            .unwrap()
            .set_dimensions(epoch, width, height)
    }

    fn apply_stream_access_unit(&self, epoch: u64, nals: &[&[u8]]) -> bool {
        let confirmed = {
            let mut descriptor = self.stream_descriptor.lock().unwrap();
            let was_ready = descriptor.is_ready();
            if !descriptor.apply_access_unit(epoch, nals) {
                return false;
            }
            (!was_ready && descriptor.is_ready()).then_some((descriptor.width, descriptor.height))
        };
        if let Some(resolution) = confirmed {
            *self.last_confirmed_resolution.lock().unwrap() = Some(resolution);
        }
        true
    }

    fn invalidate_stream(&self) -> u64 {
        let epoch = self.stream_descriptor.lock().unwrap().invalidate();
        drain_and_process(&self.sessions, |session| session.close());
        set_rtsp_clients(0);
        epoch
    }
}

fn drain_and_process<T>(items: &Mutex<Vec<T>>, mut process: impl FnMut(T)) {
    let drained = {
        let mut items = items.lock().unwrap();
        items.drain(..).collect::<Vec<_>>()
    };
    for item in drained {
        process(item);
    }
}

fn snapshot_matching<T: Clone>(
    items: &Mutex<Vec<T>>,
    mut matches: impl FnMut(&T) -> bool,
) -> Vec<T> {
    items
        .lock()
        .unwrap()
        .iter()
        .filter(|item| matches(item))
        .cloned()
        .collect()
}

fn take_matching_arcs<T>(
    items: &Mutex<Vec<Arc<T>>>,
    mut matches: impl FnMut(&Arc<T>) -> bool,
) -> Vec<Arc<T>> {
    let mut removed = Vec::new();
    items.lock().unwrap().retain(|item| {
        if matches(item) {
            removed.push(item.clone());
            false
        } else {
            true
        }
    });
    removed
}

fn dispatch_access_unit_to_sessions<T, B, E>(
    sessions: Vec<Arc<T>>,
    access_unit: Arc<B>,
    mut dispatch: impl FnMut(&T, Arc<B>) -> std::result::Result<(), E>,
) -> Vec<(Arc<T>, E)> {
    let mut failed = Vec::new();
    for session in sessions {
        if let Err(error) = dispatch(&session, access_unit.clone()) {
            failed.push((session, error));
        }
    }
    failed
}

fn take_failed_session_instances<T, E>(
    sessions: &Mutex<Vec<Arc<T>>>,
    failed: &[(Arc<T>, E)],
    id_of: impl for<'a> Fn(&'a T) -> &'a str,
) -> Vec<Arc<T>> {
    take_matching_arcs(sessions, |registered| {
        failed.iter().any(|(failed, _)| {
            id_of(registered) == id_of(failed) && Arc::ptr_eq(registered, failed)
        })
    })
}

fn advance_generation(generation: &AtomicU64) -> u64 {
    generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
}

struct ResolvedCapturePlan<D> {
    generation: u64,
    selected: display::CapturableDisplay<D>,
    resolution: display::DisplayResolution,
}

fn wait_for_encoder_for_resolved_display<T>(
    resolution: &display::DisplayResolution,
    wait: impl FnOnce() -> T,
) -> Option<T> {
    if resolution.position.is_some() {
        Some(wait())
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureExit {
    Reconfigure,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatchdogDisposition {
    RestartImmediately,
    RemainDisabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncoderWait<T> {
    Found(T),
    TimedOut,
    Disabled,
}

fn wait_for_encoder_with<T>(
    timeout: Duration,
    poll_interval: Duration,
    mut is_enabled: impl FnMut() -> bool,
    mut probe: impl FnMut() -> Option<T>,
    mut sleep: impl FnMut(Duration),
) -> EncoderWait<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if !is_enabled() {
            return EncoderWait::Disabled;
        }
        if let Some(encoder) = probe() {
            return EncoderWait::Found(encoder);
        }
        if Instant::now() >= deadline {
            return EncoderWait::TimedOut;
        }
        sleep(poll_interval);
    }
}

struct ConsecutiveCaptureErrors {
    count: u32,
    maximum: u32,
}

impl ConsecutiveCaptureErrors {
    fn new(maximum: u32) -> Self {
        Self { count: 0, maximum }
    }

    fn record_success(&mut self) {
        self.count = 0;
    }

    fn record_would_block(&mut self) {}

    fn record_error(&mut self) -> bool {
        self.count = self.count.saturating_add(1);
        self.count >= self.maximum
    }

    fn count(&self) -> u32 {
        self.count
    }
}

fn watchdog_disposition(exit: CaptureExit) -> WatchdogDisposition {
    match exit {
        CaptureExit::Reconfigure => WatchdogDisposition::RestartImmediately,
        CaptureExit::Disabled => WatchdogDisposition::RemainDisabled,
    }
}

/// Starts ScreenCam in the background. Never blocks the caller — all failures
/// (no encoder, capture errors, port already in use, ...) are logged from the
/// spawned thread, matching how the rest of src/server's background services
/// report trouble (there is no synchronous "did it start ok" return here on
/// purpose, since the panel-facing status reporting from Fase 4 is what's
/// meant to surface this, not the caller of `start()`).
///
/// Watchdog (Fase 3): `run()` propagating any error (a transient capture
/// hiccup, the monitor disappearing, an encoder error, ...) used to kill this
/// thread permanently until the whole --server process restarted. It's
/// retried instead now, with a backoff that resets once a run has stayed up
/// long enough to call the previous failure unrelated (a genuine crash loop,
/// e.g. no encoder found, otherwise backs off up to MIN(30s) instead of
/// hammering retries).
pub fn start(cfg: ScreenCamConfig) {
    std::thread::spawn(move || {
        // The RTSP listener is started exactly once and lives for as long as
        // the process does. It has no shutdown path (see rtsp::start_listener),
        // so it must NOT be part of what the watchdog below retries — binding
        // the same port again on every retry would fail with "address already
        // in use" from the still-running old listener thread, masking
        // whatever actually failed. A session hitting DESCRIBE while nothing
        // is capturing just gets a 503 until capture_loop comes back up.
        let state = Arc::new(SharedState::new());
        if let Err(e) = rtsp::start_listener(cfg.rtsp_port, state.clone()) {
            log::error!("[screencam] failed to start RTSP listener, giving up: {e:?}");
            return;
        }
        // Best-effort: WS-Discovery needs UDP 3702, which some other ONVIF
        // responder or app on the host may already hold — that must not take
        // ScreenCam's actual video down with it, so a failure here only logs
        // and disables discovery, same "degrade to RTSP-only" reasoning as
        // the rest of this Fase 6 rollout.
        onvif::start(
            cfg.rtsp_port,
            cfg.onvif_port,
            cfg.device_uuid.clone(),
            state.clone(),
        );

        const MIN_BACKOFF: Duration = Duration::from_secs(2);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);
        const HEALTHY_UPTIME: Duration = Duration::from_secs(60);
        const DISABLED_POLL_INTERVAL: Duration = Duration::from_secs(2);
        let mut backoff = MIN_BACKOFF;
        loop {
            if !is_enabled() {
                set_status("disabled");
                std::thread::sleep(DISABLED_POLL_INTERVAL);
                continue;
            }
            let attempt_start = Instant::now();
            match capture_loop(cfg.clone(), state.clone()) {
                Ok(exit) => match watchdog_disposition(exit) {
                    WatchdogDisposition::RestartImmediately => {
                        state.deactivate_display();
                        state.invalidate_stream();
                        log::info!("[screencam] rebuilding capture for display topology change");
                        backoff = MIN_BACKOFF;
                        continue;
                    }
                    WatchdogDisposition::RemainDisabled => {
                        state.deactivate_display();
                        state.invalidate_stream();
                        log::info!("[screencam] capture stopped (switched off)");
                        set_status("disabled");
                        set_last_error("");
                        backoff = MIN_BACKOFF;
                        continue;
                    }
                },
                Err(e) => {
                    state.deactivate_display();
                    state.invalidate_stream();
                    log::error!("[screencam] capture loop crashed: {e:?}");
                    set_status("error");
                    set_last_error(&e.to_string());
                }
            }
            if attempt_start.elapsed() > HEALTHY_UPTIME {
                backoff = MIN_BACKOFF;
            }
            log::info!("[screencam] restarting capture in {:?}", backoff);
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    });
}

/// PIN-protected on/off switch (Settings → ScreenCam), stored under the same
/// `LocalConfig` key/value store that `bind.mainGetLocalOption`/
/// `mainSetLocalOption` already expose to Flutter — no new bridge function
/// needed, those are already generated and already used elsewhere (e.g. for
/// `access_token`). Defaults to enabled when the key was never set, matching
/// "Activado" being the out-of-the-box state in the plan's UI mock.
const ENABLED_OPTION_KEY: &str = "screencam-enabled";

/// Three modes from the original plan (section 2.9):
/// - "local" (default): the PIN-gated checkbox in Settings fully controls it.
/// - "managed": the panel is the only authority — [`ENABLED_OPTION_KEY`] is
///   ignored entirely, exactly like "supervised" below. This used to behave
///   like "local" (back when Settings → ScreenCam still had a PIN-gated
///   checkbox), but that checkbox was removed once the panel took over, which
///   left any machine that had ever been switched off locally stuck with
///   `screencam-enabled=N` and **no way to clear it** — the panel could say
///   `licensed: true` + `desired_state: running` and capture would still
///   refuse to start (found in testing 27/07: panel on, client "Apagado",
///   nothing on RTSP).
/// - "supervised" ("supervisión permanente"): the local toggle is ignored
///   entirely, even with the PIN — cannot be turned off from this machine.
const MODE_OPTION_KEY: &str = "screencam-mode";

/// Cross-process staleness: every policy key above is written by the *UI*
/// process (Flutter/Dart, via `mainSetLocalOption` from
/// `UserModel._persistScreenCamPolicy`), while everything in this module
/// runs in the separate `--server` process. `LocalConfig::get_option` reads
/// an in-memory copy loaded once when *this* process started — a change the
/// UI process writes and persists to disk never shows up through it, so a
/// license/desired_state flip from the panel would appear to do nothing
/// (found in testing 27/07: turning it on/off in the panel had zero effect
/// on either the running stream or the reported status).
/// `LocalConfig::get_option_from_file` re-reads the TOML on every call
/// instead, which is what actually observes the other process's writes —
/// but doing that on every single frame (this is checked once per frame in
/// capture_loop) would mean a full file parse ~10 times/second. Cached here
/// for `POLICY_CACHE_TTL` instead: fresh enough to feel immediate to a human
/// flipping a switch in a panel, cheap enough to call from the hot loop.
const POLICY_CACHE_TTL: Duration = Duration::from_secs(2);

struct PolicyCache {
    fetched_at: Option<Instant>,
    licensed: bool,
    desired_state_stopped: bool,
    supervised: bool,
    /// "managed" or "supervised" — i.e. every mode in which the panel, not
    /// this machine, decides whether capture runs. See [`MODE_OPTION_KEY`].
    panel_controlled: bool,
    locally_disabled: bool,
}

impl Default for PolicyCache {
    fn default() -> Self {
        // Matches each getter's own fail-open default, used only until the
        // very first refresh populates real values.
        Self {
            fetched_at: None,
            licensed: true,
            desired_state_stopped: false,
            supervised: false,
            panel_controlled: false,
            locally_disabled: false,
        }
    }
}

lazy_static::lazy_static! {
    static ref POLICY_CACHE: Mutex<PolicyCache> = Mutex::new(PolicyCache::default());
}

fn refresh_policy_cache_if_stale() {
    let mut cache = POLICY_CACHE.lock().unwrap();
    let stale = match cache.fetched_at {
        None => true,
        Some(t) => t.elapsed() >= POLICY_CACHE_TTL,
    };
    if !stale {
        return;
    }
    use hbb_common::config::LocalConfig;
    cache.licensed = LocalConfig::get_option_from_file(LICENSED_OPTION_KEY) != "N";
    cache.desired_state_stopped =
        LocalConfig::get_option_from_file(DESIRED_STATE_OPTION_KEY) == "stopped";
    let mode = LocalConfig::get_option_from_file(MODE_OPTION_KEY);
    cache.supervised = mode == "supervised";
    cache.panel_controlled = mode == "managed" || cache.supervised;
    cache.locally_disabled = LocalConfig::get_option_from_file(ENABLED_OPTION_KEY) == "N";
    cache.fetched_at = Some(Instant::now());
}

/// Rate-limits the tamper-attempt log below so a supervised machine with a
/// stale `screencam-enabled=N` sitting in its config doesn't spam the log
/// once per frame (this is called from capture_loop's per-frame check).
static LAST_TAMPER_WARN_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn log_tamper_attempt_throttled() {
    use std::sync::atomic::Ordering as O;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_TAMPER_WARN_SECS.load(O::Relaxed);
    if now.saturating_sub(last) >= 30 {
        LAST_TAMPER_WARN_SECS.store(now, O::Relaxed);
        log::warn!(
            "[screencam] {} is set to disable capture while in supervised mode — ignoring it \
             (supervised mode can't be turned off from this machine). This is logged as a \
             possible tamper attempt; Fase 4 is where this should become a real panel alert.",
            ENABLED_OPTION_KEY
        );
    }
}

/// Written by the Dart side (`UserModel.fetchForceLogin`/`screen_cam.update`
/// WS event, see `flutter/lib/models/user_model.dart`) from the server's
/// `GET /api/client-policy?id=<rustdesk_id>` → `screen_cam.licensed`. Same
/// "unset/anything but N means yes" convention as [`ENABLED_OPTION_KEY`], so
/// a device that has never talked to a panel (no api_server configured, or
/// the very first policy fetch hasn't completed/failed) keeps working
/// exactly like before Fase 4 — this is a fail-*open* default matching how
/// `force_login`/`/api/client-policy` reachability already behaves elsewhere
/// in this codebase (see docs/CLIENT_INTEGRATION.md section 7, point 2).
/// A server that actually wants to gate this must positively answer
/// `licensed: false`, which Dart translates to an explicit "N".
const LICENSED_OPTION_KEY: &str = "screencam-licensed";

/// Written by the same Dart code from `screen_cam.desired_state`. Only
/// `"stopped"` has an effect; anything else (including unset) behaves like
/// `"running"` — same fail-open reasoning as [`LICENSED_OPTION_KEY`].
const DESIRED_STATE_OPTION_KEY: &str = "screencam-desired-state";

/// Server-side licensing is checked *before* the local supervised-mode
/// override below: a revoked license or an admin-issued stop must win over
/// "supervised" — supervised mode only protects against a *local* user
/// flipping the switch, it was never meant to override the panel that's the
/// actual source of truth once Fase 4 is wired up.
fn is_enabled() -> bool {
    refresh_policy_cache_if_stale();
    let cache = POLICY_CACHE.lock().unwrap();
    if !cache.licensed || cache.desired_state_stopped {
        return false;
    }
    if cache.panel_controlled {
        // Only "supervised" treats a local N as something worth flagging —
        // under plain "managed" it's almost always just a leftover from
        // before the local checkbox was removed, not a tamper attempt.
        if cache.supervised && cache.locally_disabled {
            log_tamper_attempt_throttled();
        }
        return true;
    }
    !cache.locally_disabled
}

/// Status keys this process writes (never reads) — the mirror image of the
/// policy keys above. Dart's heartbeat (`_readScreenCamStatus` in
/// user_model.dart) reads these and forwards them under `screen_cam` in
/// `POST /api/heartbeat`, per docs/SCREENCAM_PLAN.md section 11.2.
/// `LocalConfig::set_option` only touches disk when a value actually
/// changes (see libs/hbb_common/src/config.rs), so writing these every
/// frame is cheap — no throttling needed beyond what it already does.
const ACTUAL_STATE_OPTION_KEY: &str = "screencam-actual-state";
const ENCODER_OPTION_KEY: &str = "screencam-encoder";
const LAST_ERROR_OPTION_KEY: &str = "screencam-last-error";
const RTSP_CLIENTS_OPTION_KEY: &str = "screencam-rtsp-clients";
/// Server dev confirmed (docs/SCREENCAM_PLAN.md section 12.3) these two raw
/// fields, not a pre-built URL — same convention as `hostname`/`os` — so the
/// panel can rebuild `rtsp://{local_ip}:{rtsp_port}/live/main` itself and
/// isn't locked to today's path if it changes later (`/live/sub`, auth, ...).
const LOCAL_IP_OPTION_KEY: &str = "screencam-local-ip";
const RTSP_PORT_OPTION_KEY: &str = "screencam-rtsp-port";

fn set_status(state: &str) {
    hbb_common::config::LocalConfig::set_option(
        ACTUAL_STATE_OPTION_KEY.to_owned(),
        state.to_owned(),
    );
}

fn set_encoder_status(name: &str) {
    hbb_common::config::LocalConfig::set_option(ENCODER_OPTION_KEY.to_owned(), name.to_owned());
}

fn set_last_error(message: &str) {
    hbb_common::config::LocalConfig::set_option(
        LAST_ERROR_OPTION_KEY.to_owned(),
        message.to_owned(),
    );
}

fn set_rtsp_clients(count: usize) {
    hbb_common::config::LocalConfig::set_option(
        RTSP_CLIENTS_OPTION_KEY.to_owned(),
        count.to_string(),
    );
}

fn set_rtsp_port(port: u16) {
    hbb_common::config::LocalConfig::set_option(RTSP_PORT_OPTION_KEY.to_owned(), port.to_string());
}

fn set_local_ip(ip: &str) {
    hbb_common::config::LocalConfig::set_option(LOCAL_IP_OPTION_KEY.to_owned(), ip.to_owned());
}

/// Same no-packet-actually-sent UDP "connect" trick as
/// `rtsp::local_ip_for_peer`, but anchored to a fixed well-known address
/// instead of a specific RTSP client's — this is for the heartbeat, which
/// needs one representative LAN IP for the whole machine, not a per-viewer
/// answer. 8.8.8.8:80 is never actually contacted.
fn detect_local_ip() -> Option<String> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip().to_string())
}

fn capture_loop(cfg: ScreenCamConfig, state: Arc<SharedState>) -> ResultType<CaptureExit> {
    const TOPOLOGY_POLL_INTERVAL: Duration = Duration::from_secs(3);

    set_status("starting");
    let plan = loop {
        if !is_enabled() {
            return Ok(CaptureExit::Disabled);
        }
        let generation_before_enumeration = state.reconfigure_generation.load(Ordering::SeqCst);
        let displays = display::DisplayInventory::enumerate()?;
        let (update, generation) = state.apply_display_inventory(&displays);
        if !update.desired_changed && generation != generation_before_enumeration {
            continue;
        }
        if update.topology_changed {
            log_display_inventory(&displays);
        }
        if let Some(position) = update.resolution.position {
            let selected = displays.into_display_at(position).ok_or_else(|| {
                anyhow!("resolved display position disappeared from the current inventory")
            })?;
            break ResolvedCapturePlan {
                generation,
                selected,
                resolution: update.resolution,
            };
        }

        set_status("waiting_for_display");
        set_last_error("");
        if let Some(warning) = update.resolution.warning.as_deref() {
            log::debug!("[screencam] {warning}");
        }
        let deadline = Instant::now() + TOPOLOGY_POLL_INTERVAL;
        while Instant::now() < deadline {
            if !is_enabled() {
                return Ok(CaptureExit::Disabled);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };

    let stream_epoch = state.stream_epoch();
    let (encoder_name, encoder_mc_name) =
        match wait_for_encoder_for_resolved_display(&plan.resolution, || {
            wait_for_h264_encoder(Duration::from_secs(35))
        }) {
            Some(EncoderWait::Found(encoder)) => encoder,
            Some(EncoderWait::Disabled) => return Ok(CaptureExit::Disabled),
            Some(EncoderWait::TimedOut) | None => {
                return Err(anyhow!(
                    "no_h264_encoder: no hardware H.264 encoder detected on this machine \
                 (needs a working NVENC/QuickSync/AMF/VAAPI driver — see \
                 docs/SCREENCAM_PLAN.md §3.1, option A has no software fallback)"
                ));
            }
        };
    let display_id = plan
        .resolution
        .active_display_id
        .clone()
        .ok_or_else(|| anyhow!("resolved display has no canonical display id"))?;
    let width = plan.selected.info.width;
    let height = plan.selected.info.height;
    let mut capturer = scrap::Capturer::new(plan.selected.display)?;

    let keyframe_interval = (cfg.fps as usize * 2).max(1); // ~2s GOP, per the plan
    let encoder_status_name = encoder_name.clone();
    let encoder_cfg = EncoderCfg::HWRAM(HwRamEncoderConfig {
        name: encoder_name,
        mc_name: encoder_mc_name,
        width,
        height,
        quality: cfg.quality,
        keyframe_interval: Some(keyframe_interval),
    });
    let mut encoder = Encoder::new(encoder_cfg, false)?;

    if state.reconfigure_generation.load(Ordering::SeqCst) != plan.generation {
        return Ok(CaptureExit::Reconfigure);
    }
    if !state.set_stream_dimensions(stream_epoch, width, height) {
        return Ok(CaptureExit::Reconfigure);
    }
    if !state.activate_display_for_generation(plan.generation, &plan.resolution) {
        return Ok(CaptureExit::Reconfigure);
    }
    log::info!(
        "[screencam] using hardware encoder: {}",
        encoder_status_name
    );
    set_encoder_status(&encoder_status_name);
    log::info!(
        "[screencam] capturing display '{}' at {}x{}{}",
        display_id,
        width,
        height,
        if plan.resolution.fallback_active {
            " (primary fallback)"
        } else {
            ""
        }
    );

    let mut payloader = rtp::H264Payloader::new();
    let spf = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    let start = Instant::now();
    let mut yuv = Vec::new();
    let mut mid_data = Vec::new();
    const RTP_MTU: usize = 1400; // payload only, well under Ethernet MTU with headroom for IP/UDP/RTP headers

    set_status("running");
    set_last_error("");
    let clients = {
        let sessions = state.sessions.lock().unwrap();
        sessions.len()
    };
    set_rtsp_clients(clients);
    set_rtsp_port(cfg.rtsp_port);
    if let Some(ip) = detect_local_ip() {
        set_local_ip(&ip);
    }
    let mut last_status_report = Instant::now();
    let mut last_topology_check = Instant::now();
    let observed_generation = plan.generation;
    const MAX_CONSECUTIVE_CAPTURE_ERRORS: u32 = 3;
    let mut capture_errors = ConsecutiveCaptureErrors::new(MAX_CONSECUTIVE_CAPTURE_ERRORS);

    loop {
        if !is_enabled() {
            log::info!("[screencam] switched off, stopping capture");
            return Ok(CaptureExit::Disabled);
        }
        if state.reconfigure_generation.load(Ordering::SeqCst) != observed_generation {
            return Ok(CaptureExit::Reconfigure);
        }
        if last_topology_check.elapsed() >= TOPOLOGY_POLL_INTERVAL {
            last_topology_check = Instant::now();
            let displays = display::DisplayInventory::enumerate()?;
            let (update, _) = state.apply_display_inventory(&displays);
            if update.topology_changed {
                log_display_inventory(&displays);
            }
            if update.requires_reconfigure {
                return Ok(CaptureExit::Reconfigure);
            }
        }
        if last_status_report.elapsed() >= Duration::from_secs(5) {
            last_status_report = Instant::now();
            let clients = {
                let sessions = state.sessions.lock().unwrap();
                sessions.len()
            };
            set_rtsp_clients(clients);
        }
        let loop_start = Instant::now();
        match capturer.frame(spf) {
            Ok(frame) => {
                capture_errors.record_success();
                if frame.valid() {
                    let input = frame.to(encoder.yuvfmt(), &mut yuv, &mut mid_data)?;
                    let ms = start.elapsed().as_millis() as i64;
                    match encoder.encode_to_message(input, ms) {
                        Ok(vf) => {
                            if let Some(video_frame::Union::H264s(h264s)) = vf.union {
                                for f in h264s.frames.iter() {
                                    handle_access_unit(
                                        &state,
                                        &mut payloader,
                                        &f.data,
                                        stream_epoch,
                                        start.elapsed(),
                                        RTP_MTU,
                                    );
                                }
                            }
                        }
                        Err(e) => log::error!("[screencam] encode error: {e:?}"),
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                capture_errors.record_would_block();
            }
            Err(e) => {
                let rebuild = capture_errors.record_error();
                log::error!(
                    "[screencam] capture error ({}/{}): {e}",
                    capture_errors.count(),
                    MAX_CONSECUTIVE_CAPTURE_ERRORS
                );
                if rebuild {
                    bail!(
                        "capture_invalidated: rebuilding display capturer after repeated error: {e}"
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        let elapsed = loop_start.elapsed();
        if elapsed < spf {
            std::thread::sleep(spf - elapsed);
        }
    }
}

fn log_display_inventory<D>(displays: &display::DisplayInventory<D>) {
    for info in displays.infos() {
        log::info!(
            "[screencam] display {}: id='{}', name='{}', {}x{} at ({}, {}), primary={}, connected={}",
            info.index,
            info.display_id,
            info.name,
            info.width,
            info.height,
            info.origin.0,
            info.origin.1,
            info.primary,
            info.connected
        );
    }
}

fn handle_access_unit(
    state: &SharedState,
    payloader: &mut rtp::H264Payloader,
    data: &[u8],
    stream_epoch: u64,
    elapsed: Duration,
    mtu: usize,
) {
    let nals = rtp::split_annexb_nals(data);
    if nals.is_empty() {
        return;
    }
    if !state.apply_stream_access_unit(stream_epoch, &nals) {
        return;
    }

    let sessions = snapshot_matching(&state.sessions, |session| session.epoch() == stream_epoch);
    if sessions.is_empty() {
        return; // no one watching; still update SPS/PPS above so DESCRIBE works once someone connects
    }
    let timestamp_90k = (elapsed.as_secs_f64() * 90_000.0) as u32;
    let access_unit = Arc::new(rtsp::RtpAccessUnit::new(payloader.packetize(
        &nals,
        timestamp_90k,
        mtu,
    )));
    let failed = dispatch_access_unit_to_sessions(sessions, access_unit, |session, access_unit| {
        session.dispatch_access_unit(stream_epoch, access_unit)
    });
    if failed.is_empty() {
        return;
    }

    let removed =
        take_failed_session_instances(&state.sessions, &failed, |session| session.id.as_str());
    for session in removed {
        if let Some((_, error)) = failed
            .iter()
            .find(|(failed, _)| Arc::ptr_eq(&session, failed))
        {
            log::warn!("[screencam] removing RTSP session after RTP send failed: {error}");
        }
        session.close();
    }
}

/// The hwcodec capability probe runs on a background thread/subprocess
/// (scrap::hwcodec::start_check_process, kicked off earlier in
/// src/server.rs::start_server) and can take a few seconds. Poll instead of
/// failing on the very first check so a capable machine doesn't get a false
/// "no_h264_encoder" right after the server process starts.
///
/// Returns (encoder name, mc_name) rather than the hwcodec crate's
/// `CodecInfo` directly — that type isn't re-exported from `scrap::hwcodec`,
/// and nothing else in this codebase names it either (see e.g. `codec.rs`'s
/// `HwRamEncoder::try_get(...).map_or(None, |c| Some(c.name))`), so this
/// follows the same pattern instead of reaching into scrap's internals.
fn wait_for_h264_encoder(timeout: Duration) -> EncoderWait<(String, Option<String>)> {
    wait_for_encoder_with(
        timeout,
        Duration::from_secs(2),
        is_enabled,
        || HwRamEncoder::try_get(CodecFormat::H264).map(|info| (info.name, info.mc_name)),
        std::thread::sleep,
    )
}

#[cfg(test)]
mod delivery2_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    fn available_display(
        display_id: &str,
        index: usize,
        width: usize,
        height: usize,
        primary: bool,
    ) -> display::AvailableDisplay {
        display::AvailableDisplay {
            display_id: display_id.to_owned(),
            name: display_id.to_owned(),
            index,
            width,
            height,
            origin: (0, 0),
            primary,
            connected: true,
        }
    }

    fn inventory_with(displays: Vec<display::AvailableDisplay>) -> display::DisplayInventory<()> {
        display::DisplayInventory::from_test_infos(displays)
    }

    fn inventory(width: usize) -> display::DisplayInventory<()> {
        inventory_with(vec![display::AvailableDisplay {
            display_id: r"\\.\DISPLAY2".to_owned(),
            name: "Primary".to_owned(),
            index: 0,
            width,
            height: 1080,
            origin: (0, 0),
            primary: true,
            connected: true,
        }])
    }

    fn sps() -> &'static [u8] {
        &[0x67, 0x64, 0x00, 0x1f]
    }

    fn pps() -> &'static [u8] {
        &[0x68, 0xee, 0x3c, 0x80]
    }

    fn idr() -> &'static [u8] {
        &[0x65, 0x88, 0x84]
    }

    #[test]
    fn reconfigure_generation_changes_only_with_the_resolved_capture() {
        let state = SharedState::new();
        let first = inventory(1920);
        let (first_update, first_generation) = state.apply_display_inventory(&first);
        assert_eq!(state.reconfigure_generation.load(Ordering::SeqCst), 1);
        assert!(state.activate_display_for_generation(first_generation, &first_update.resolution));

        let unchanged = inventory(1920);
        let (unchanged_update, _) = state.apply_display_inventory(&unchanged);
        assert!(!unchanged_update.requires_reconfigure);
        assert_eq!(state.reconfigure_generation.load(Ordering::SeqCst), 1);

        let resized = inventory(2560);
        let (resized_update, _) = state.apply_display_inventory(&resized);
        assert!(resized_update.requires_reconfigure);
        assert_eq!(state.reconfigure_generation.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn reconfigure_generation_and_stream_epoch_are_independent() {
        let state = SharedState::new();
        let first = inventory(1920);

        state.apply_display_inventory(&first);
        assert_eq!(state.reconfigure_generation.load(Ordering::SeqCst), 1);
        assert_eq!(state.stream_epoch(), 0);

        state.invalidate_stream();
        assert_eq!(state.reconfigure_generation.load(Ordering::SeqCst), 1);
        assert_eq!(state.stream_epoch(), 1);
    }

    #[test]
    fn generations_increment_consecutively_and_wrap_explicitly() {
        let generation = AtomicU64::new(4);
        assert_eq!(advance_generation(&generation), 5);
        assert_eq!(advance_generation(&generation), 6);
        generation.store(u64::MAX, Ordering::SeqCst);
        assert_eq!(advance_generation(&generation), 0);

        let mut descriptor = StreamDescriptorState::new();
        descriptor.epoch = u64::MAX - 1;
        assert_eq!(descriptor.invalidate(), u64::MAX);
        assert_eq!(descriptor.invalidate(), 0);
    }

    #[test]
    fn every_stream_invalidation_creates_a_new_epoch() {
        let state = SharedState::new();

        assert_eq!(state.invalidate_stream(), 1);
        assert_eq!(state.invalidate_stream(), 2);
        assert_eq!(state.invalidate_stream(), 3);
        assert_eq!(state.reconfigure_generation.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalidation_clears_the_complete_descriptor() {
        let mut descriptor = StreamDescriptorState::new();
        assert!(descriptor.set_dimensions(0, 1920, 1080));
        assert!(descriptor.apply_access_unit(0, &[sps(), pps(), idr()]));
        assert!(descriptor.is_ready());

        assert_eq!(descriptor.invalidate(), 1);

        assert_eq!(descriptor.width, 0);
        assert_eq!(descriptor.height, 0);
        assert_eq!(descriptor.sps, None);
        assert_eq!(descriptor.pps, None);
        assert!(!descriptor.idr_ready);
        assert!(!descriptor.is_ready());
    }

    #[test]
    fn dimensions_are_observed_as_one_coherent_pair() {
        let state = Arc::new(SharedState::new());
        let writer_state = state.clone();
        let writer = thread::spawn(move || {
            for index in 0..10_000 {
                let dimensions = if index % 2 == 0 {
                    (1920, 1080)
                } else {
                    (2560, 1440)
                };
                assert!(writer_state.set_stream_dimensions(0, dimensions.0, dimensions.1));
            }
        });

        for _ in 0..10_000 {
            assert!(matches!(
                state.stream_dimensions(),
                (0, 0) | (1920, 1080) | (2560, 1440)
            ));
        }
        writer.join().unwrap();
    }

    #[test]
    fn sps_and_pps_without_idr_do_not_make_the_stream_ready() {
        let mut descriptor = StreamDescriptorState::new();
        assert!(descriptor.set_dimensions(0, 1920, 1080));
        assert!(descriptor.apply_access_unit(0, &[sps(), pps()]));

        assert!(!descriptor.idr_ready);
        assert!(!descriptor.is_ready());
    }

    #[test]
    fn current_epoch_idr_completes_the_descriptor() {
        let mut descriptor = StreamDescriptorState::new();
        assert!(descriptor.set_dimensions(0, 1920, 1080));
        assert!(descriptor.apply_access_unit(0, &[sps(), pps()]));
        assert!(descriptor.apply_access_unit(0, &[idr()]));

        assert!(descriptor.idr_ready);
        assert!(descriptor.is_ready());
    }

    #[test]
    fn stale_access_units_cannot_enable_a_new_epoch() {
        let mut descriptor = StreamDescriptorState::new();
        descriptor.invalidate();
        assert!(descriptor.set_dimensions(1, 1920, 1080));

        assert!(!descriptor.apply_access_unit(0, &[sps(), pps(), idr()]));
        assert_eq!(descriptor.sps, None);
        assert_eq!(descriptor.pps, None);
        assert!(!descriptor.idr_ready);
        assert!(!descriptor.is_ready());
    }

    #[test]
    fn annexb_sps_pps_and_idr_together_enable_the_descriptor() {
        let data = [
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0, 0, 1, 0x68, 0xee, 0, 0, 1, 0x65, 0x88,
        ];
        let nals = rtp::split_annexb_nals(&data);
        let mut descriptor = StreamDescriptorState::new();
        assert!(descriptor.set_dimensions(0, 1920, 1080));

        assert!(descriptor.apply_access_unit(0, &nals));
        assert!(descriptor.is_ready());
    }

    #[test]
    fn idr_before_sps_and_pps_becomes_ready_after_parameters_arrive() {
        let mut descriptor = StreamDescriptorState::new();
        assert!(descriptor.set_dimensions(0, 1920, 1080));

        assert!(descriptor.apply_access_unit(0, &[idr()]));
        assert!(descriptor.idr_ready);
        assert!(!descriptor.is_ready());

        assert!(descriptor.apply_access_unit(0, &[sps(), pps()]));
        assert!(descriptor.is_ready());
    }

    #[test]
    fn aud_sei_and_non_idr_slices_do_not_mark_idr_ready() {
        let mut descriptor = StreamDescriptorState::new();
        assert!(descriptor.set_dimensions(0, 1920, 1080));
        assert!(descriptor.apply_access_unit(
            0,
            &[sps(), pps(), &[0x69, 0x10], &[0x66, 0x20], &[0x61, 0x30]]
        ));

        assert!(!descriptor.idr_ready);
        assert!(!descriptor.is_ready());
    }

    #[test]
    fn draining_sessions_runs_shutdown_after_unlocking() {
        let sessions = Mutex::new(vec![1_u64, 2, 3]);
        let closed = Mutex::new(Vec::new());

        drain_and_process(&sessions, |session| {
            assert!(sessions.try_lock().is_ok());
            closed.lock().unwrap().push(session);
        });

        assert!(sessions.lock().unwrap().is_empty());
        assert_eq!(*closed.lock().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn current_epoch_session_snapshot_does_not_hold_or_mutate_registry() {
        #[derive(Clone)]
        struct FakeSession {
            epoch: u64,
            token: u64,
        }

        let sessions = Mutex::new(vec![
            Arc::new(FakeSession { epoch: 6, token: 1 }),
            Arc::new(FakeSession { epoch: 7, token: 2 }),
        ]);
        let recipients = snapshot_matching(&sessions, |session| session.epoch == 7);

        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].token, 2);
        assert_eq!(sessions.lock().unwrap().len(), 2);
        for recipient in recipients {
            assert!(sessions.try_lock().is_ok());
            assert_eq!(recipient.epoch, 7);
        }
    }

    #[test]
    fn failed_send_removes_only_the_same_session_instance() {
        #[derive(Debug)]
        struct FakeSession {
            id: String,
            epoch: u64,
            fail: bool,
        }

        let failed_instance = Arc::new(FakeSession {
            id: "same-id".to_owned(),
            epoch: 7,
            fail: true,
        });
        let replacement = Arc::new(FakeSession {
            id: "same-id".to_owned(),
            epoch: 8,
            fail: false,
        });
        let healthy = Arc::new(FakeSession {
            id: "healthy".to_owned(),
            epoch: 8,
            fail: false,
        });
        let registry = Mutex::new(vec![
            failed_instance.clone(),
            replacement.clone(),
            healthy.clone(),
        ]);
        let snapshot = vec![failed_instance.clone(), healthy.clone()];
        let access_unit = Arc::new(vec![vec![1], vec![2]]);

        let failed = dispatch_access_unit_to_sessions(snapshot, access_unit, |session, _| {
            if session.fail {
                Err("simulated send error")
            } else {
                Ok(())
            }
        });
        let removed =
            take_failed_session_instances(&registry, &failed, |session| session.id.as_str());

        assert_eq!(failed.len(), 1);
        assert_eq!(removed.len(), 1);
        assert!(Arc::ptr_eq(&removed[0], &failed_instance));
        let remaining = registry.lock().unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().any(|item| Arc::ptr_eq(item, &replacement)));
        assert!(remaining.iter().any(|item| Arc::ptr_eq(item, &healthy)));
        assert!(remaining.iter().all(|item| item.epoch == 8));
    }

    #[test]
    fn repeated_session_cleanup_is_idempotent() {
        #[derive(Debug)]
        struct FakeSession {
            id: String,
        }

        let session = Arc::new(FakeSession {
            id: "session".to_owned(),
        });
        let registry = Mutex::new(vec![session]);

        let first = take_matching_arcs(&registry, |item| item.id == "session");
        let second = take_matching_arcs(&registry, |item| item.id == "session");

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        assert!(registry.lock().unwrap().is_empty());
    }

    #[test]
    fn watchdog_reconfigure_restarts_without_backoff() {
        assert_eq!(
            watchdog_disposition(CaptureExit::Reconfigure),
            WatchdogDisposition::RestartImmediately
        );
        assert_eq!(
            watchdog_disposition(CaptureExit::Disabled),
            WatchdogDisposition::RemainDisabled
        );
    }

    #[test]
    fn capture_error_counter_preserves_would_block_reset_and_third_error_rules() {
        let mut errors = ConsecutiveCaptureErrors::new(3);

        assert!(!errors.record_error());
        errors.record_would_block();
        assert_eq!(errors.count(), 1);
        assert!(!errors.record_error());
        errors.record_success();
        assert_eq!(errors.count(), 0);
        assert!(!errors.record_error());
        assert!(!errors.record_error());
        assert!(errors.record_error());
        assert_eq!(errors.count(), 3);
    }

    #[test]
    fn encoder_wait_stops_when_screencam_is_disabled() {
        use std::cell::Cell;

        let enabled = Cell::new(true);
        let probes = Cell::new(0);
        let sleeps = Cell::new(0);
        let result = wait_for_encoder_with(
            Duration::from_secs(35),
            Duration::from_secs(2),
            || enabled.get(),
            || {
                probes.set(probes.get() + 1);
                None::<()>
            },
            |_| {
                sleeps.set(sleeps.get() + 1);
                enabled.set(false);
            },
        );

        assert_eq!(result, EncoderWait::Disabled);
        assert_eq!(probes.get(), 1);
        assert_eq!(sleeps.get(), 1);
    }

    #[test]
    fn absent_display_does_not_probe_encoder() {
        let inventory = inventory_with(Vec::new());
        let resolution = inventory.resolve(None, true);
        let calls = AtomicUsize::new(0);

        let encoder = wait_for_encoder_for_resolved_display(&resolution, || {
            calls.fetch_add(1, Ordering::SeqCst);
            "encoder"
        });

        assert_eq!(encoder, None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn returning_display_allows_encoder_creation() {
        let absent = inventory_with(Vec::new());
        let present = inventory_with(vec![available_display(
            r"\\.\DISPLAY3",
            0,
            1920,
            1080,
            true,
        )]);
        let calls = AtomicUsize::new(0);

        let absent_resolution = absent.resolve(None, true);
        assert_eq!(
            wait_for_encoder_for_resolved_display(&absent_resolution, || {
                calls.fetch_add(1, Ordering::SeqCst);
                "encoder"
            }),
            None
        );

        let present_resolution = present.resolve(None, true);
        assert_eq!(
            wait_for_encoder_for_resolved_display(&present_resolution, || {
                calls.fetch_add(1, Ordering::SeqCst);
                "encoder"
            }),
            Some("encoder")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn encoder_creation_failure_does_not_publish_an_active_display() {
        let state = SharedState::new();
        let present = inventory(1920);
        let (update, _) = state.apply_display_inventory(&present);
        let encoder_result: std::result::Result<(), &str> = Err("simulated encoder failure");

        assert!(update.resolution.position.is_some());
        assert!(encoder_result.is_err());
        let runtime = state.display_snapshot();
        assert_eq!(runtime.active_display_id, None);
        assert!(!runtime.fallback_active);
        assert_eq!(runtime.available_displays.len(), 1);
    }

    #[test]
    fn generation_change_during_construction_rejects_the_stale_plan() {
        let state = SharedState::new();
        let initial = inventory(1920);
        let (initial_update, plan_generation) = state.apply_display_inventory(&initial);

        let changed = inventory(2560);
        let (_, changed_generation) = state.apply_display_inventory(&changed);

        assert_ne!(plan_generation, changed_generation);
        assert!(!state.activate_display_for_generation(plan_generation, &initial_update.resolution));
        assert_eq!(state.display_snapshot().active_display_id, None);
    }

    #[test]
    fn runtime_snapshot_changes_atomically_from_active_to_absent() {
        let state = SharedState::new();
        let present = inventory(1920);
        let (active, generation) = state.apply_display_inventory(&present);
        assert!(state.activate_display_for_generation(generation, &active.resolution));
        let active_snapshot = state.display_snapshot();
        assert_eq!(
            active_snapshot.active_display_id.as_deref(),
            Some(r"\\.\DISPLAY2")
        );

        let absent = inventory_with(Vec::new());
        let (update, _) = state.apply_display_inventory(&absent);
        assert!(update.requires_reconfigure);
        let absent_snapshot = state.display_snapshot();
        assert_eq!(absent_snapshot.active_display_id, None);
        assert!(!absent_snapshot.fallback_active);
        assert!(absent_snapshot.display_warning.is_some());
        assert!(absent_snapshot.available_displays.is_empty());
    }
}
