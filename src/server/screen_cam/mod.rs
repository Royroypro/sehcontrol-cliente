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
mod onvif;
mod rtp;
mod rtsp;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hbb_common::{
    anyhow::anyhow, bail, config, log, message_proto::video_frame,
    serde_derive::{Deserialize, Serialize},
    ResultType,
};
use scrap::{
    codec::{Encoder, EncoderApi, EncoderCfg},
    hwcodec::{HwRamEncoder, HwRamEncoderConfig},
    CodecFormat, Display, TraitCapturer,
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

pub struct SharedState {
    pub sessions: Mutex<Vec<Session>>,
    pub sps: Mutex<Option<Vec<u8>>>,
    pub pps: Mutex<Option<Vec<u8>>>,
    pub width: AtomicUsize,
    pub height: AtomicUsize,
}

impl SharedState {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            sps: Mutex::new(None),
            pps: Mutex::new(None),
            width: AtomicUsize::new(0),
            height: AtomicUsize::new(0),
        }
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
        onvif::start(cfg.rtsp_port, cfg.onvif_port, cfg.device_uuid.clone(), state.clone());

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
                // A clean Ok(()) today only ever means capture_loop noticed
                // the on/off switch got flipped off mid-stream (see the check
                // at the top of its loop) — not a failure, so no backoff.
                Ok(()) => {
                    log::info!("[screencam] capture stopped (switched off)");
                    set_status("disabled");
                    set_last_error("");
                    backoff = MIN_BACKOFF;
                    continue;
                }
                Err(e) => {
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
    hbb_common::config::LocalConfig::set_option(ACTUAL_STATE_OPTION_KEY.to_owned(), state.to_owned());
}

fn set_encoder_status(name: &str) {
    hbb_common::config::LocalConfig::set_option(ENCODER_OPTION_KEY.to_owned(), name.to_owned());
}

fn set_last_error(message: &str) {
    hbb_common::config::LocalConfig::set_option(LAST_ERROR_OPTION_KEY.to_owned(), message.to_owned());
}

fn set_rtsp_clients(count: usize) {
    hbb_common::config::LocalConfig::set_option(RTSP_CLIENTS_OPTION_KEY.to_owned(), count.to_string());
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

fn capture_loop(cfg: ScreenCamConfig, state: Arc<SharedState>) -> ResultType<()> {
    set_status("starting");
    let (encoder_name, encoder_mc_name) = wait_for_h264_encoder(Duration::from_secs(35))
        .ok_or_else(|| {
            anyhow!(
                "no_h264_encoder: no hardware H.264 encoder detected on this machine \
                 (needs a working NVENC/QuickSync/AMF/VAAPI driver — see \
                 docs/SCREENCAM_PLAN.md §3.1, option A has no software fallback)"
            )
        })?;
    log::info!("[screencam] using hardware encoder: {}", encoder_name);
    set_encoder_status(&encoder_name);

    let mut displays = Display::all()?;
    if cfg.monitor_index >= displays.len() {
        bail!(
            "monitor index {} out of range ({} display(s) found)",
            cfg.monitor_index,
            displays.len()
        );
    }
    let display = displays.remove(cfg.monitor_index);
    let width = display.width() as usize;
    let height = display.height() as usize;
    let mut capturer = scrap::Capturer::new(display)?;
    state.width.store(width, Ordering::Relaxed);
    state.height.store(height, Ordering::Relaxed);
    log::info!(
        "[screencam] capturing monitor {} at {}x{}",
        cfg.monitor_index,
        width,
        height
    );

    let keyframe_interval = (cfg.fps as usize * 2).max(1); // ~2s GOP, per the plan
    let encoder_cfg = EncoderCfg::HWRAM(HwRamEncoderConfig {
        name: encoder_name,
        mc_name: encoder_mc_name,
        width,
        height,
        quality: cfg.quality,
        keyframe_interval: Some(keyframe_interval),
    });
    let mut encoder = Encoder::new(encoder_cfg, false)?;

    let mut payloader = rtp::H264Payloader::new();
    let spf = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    let start = Instant::now();
    let mut yuv = Vec::new();
    let mut mid_data = Vec::new();
    const RTP_MTU: usize = 1400; // payload only, well under Ethernet MTU with headroom for IP/UDP/RTP headers

    set_status("running");
    set_last_error("");
    set_rtsp_clients(state.sessions.lock().unwrap().len());
    set_rtsp_port(cfg.rtsp_port);
    if let Some(ip) = detect_local_ip() {
        set_local_ip(&ip);
    }
    let mut last_status_report = Instant::now();
    let mut consecutive_capture_errors = 0u32;
    const MAX_CONSECUTIVE_CAPTURE_ERRORS: u32 = 3;

    loop {
        if !is_enabled() {
            log::info!("[screencam] switched off, stopping capture");
            return Ok(());
        }
        if last_status_report.elapsed() >= Duration::from_secs(5) {
            last_status_report = Instant::now();
            set_rtsp_clients(state.sessions.lock().unwrap().len());
        }
        let loop_start = Instant::now();
        match capturer.frame(spf) {
            Ok(frame) => {
                consecutive_capture_errors = 0;
                if frame.valid() {
                    let input = frame.to(encoder.yuvfmt(), &mut yuv, &mut mid_data)?;
                    let ms = start.elapsed().as_millis() as i64;
                    match encoder.encode_to_message(input, ms) {
                        Ok(vf) => {
                            if let Some(video_frame::Union::H264s(h264s)) = vf.union {
                                for f in h264s.frames.iter() {
                                    handle_access_unit(&state, &mut payloader, &f.data, start.elapsed(), RTP_MTU);
                                }
                            }
                        }
                        Err(e) => log::error!("[screencam] encode error: {e:?}"),
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                consecutive_capture_errors += 1;
                log::error!(
                    "[screencam] capture error ({}/{}): {e}",
                    consecutive_capture_errors,
                    MAX_CONSECUTIVE_CAPTURE_ERRORS
                );
                if consecutive_capture_errors >= MAX_CONSECUTIVE_CAPTURE_ERRORS {
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

fn handle_access_unit(
    state: &SharedState,
    payloader: &mut rtp::H264Payloader,
    data: &[u8],
    elapsed: Duration,
    mtu: usize,
) {
    let nals = rtp::split_annexb_nals(data);
    if nals.is_empty() {
        return;
    }
    for nal in nals.iter().copied() {
        match rtp::nal_unit_type(nal) {
            rtp::NAL_TYPE_SPS => *state.sps.lock().unwrap() = Some(nal.to_vec()),
            rtp::NAL_TYPE_PPS => *state.pps.lock().unwrap() = Some(nal.to_vec()),
            _ => {}
        }
    }

    let sessions = state.sessions.lock().unwrap();
    if sessions.is_empty() {
        return; // no one watching; still update SPS/PPS above so DESCRIBE works once someone connects
    }
    let timestamp_90k = (elapsed.as_secs_f64() * 90_000.0) as u32;
    let packets = payloader.packetize(&nals, timestamp_90k, mtu);
    for pkt in &packets {
        for session in sessions.iter() {
            session.send_rtp(pkt);
        }
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
fn wait_for_h264_encoder(timeout: Duration) -> Option<(String, Option<String>)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(info) = HwRamEncoder::try_get(CodecFormat::H264) {
            return Some((info.name, info.mc_name));
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
