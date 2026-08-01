// Sehcontrol ScreenCam — Fase 1 MVP (see docs/SCREENCAM_PLAN.md).
//
// Turns one monitor of this machine into an RTSP source a DVR/NVR (or, for
// this MVP, VLC) can pull directly: `rtsp://<this-machine-ip>:8554/live/main`.
// Video never goes through the Sehcontrol panel/server — this module only
// captures, encodes and serves RTP. Panel policy is applied through the
// service IPC; it controls capture selection but never carries video data.
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
mod preview;
mod rtp;
mod rtsp;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
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
    CodecFormat, EncodeInput, TraitCapturer,
};

pub(crate) use preview::control::{PreviewControlOutcome, PreviewStartRequest, PreviewStopRequest};

/// The preview publisher's last reported state, as JSON safe to forward to the
/// panel. `{"state":"idle"}` when no session has reported anything yet — an
/// explicit answer rather than an absent one, so the caller never has to treat
/// a timeout as a state.
pub(crate) fn preview_lifecycle_status_json() -> String {
    match preview::publisher::lifecycle_snapshot() {
        Some(snapshot) => snapshot.to_json(),
        None => r#"{"state":"idle"}"#.to_owned(),
    }
}

#[cfg(test)]
pub(crate) fn reset_preview_lifecycle_for_test() {
    preview::publisher::reset_lifecycle_for_test();
}

/// Drives one `Started` through the real sink, so the IPC test exercises the
/// production path rather than a hand-built snapshot.
#[cfg(test)]
pub(crate) fn publish_preview_lifecycle_for_test() {
    use preview::publisher::{
        PreviewEventSink, PreviewLifecycleSink, PreviewOwnership, PreviewPublisherEvent,
    };
    use std::sync::{atomic::AtomicU64, Arc};

    let current = Arc::new(AtomicU64::new(1));
    let sink = PreviewLifecycleSink::new(
        "pv_status_test".to_owned(),
        "485236790".to_owned(),
        1,
        PreviewOwnership::new(current, 1),
    );
    sink.emit(PreviewPublisherEvent::Started { generation: 1 });
}
use preview::{control::PreviewControl, tap::PreviewTap};
use rtsp::Session;

const SELECTED_DISPLAY_ID_OPTION_KEY: &str = "screencam-selected-display-id";
const FALLBACK_TO_PRIMARY_OPTION_KEY: &str = "screencam-fallback-to-primary";
const DISPLAY_POLICY_RECONCILIATION_TIMEOUT: Duration = Duration::from_millis(500);
static INVALID_FALLBACK_WARNING_EMITTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static INVALID_STORED_SELECTION_WARNING_EMITTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static POLICY_NOT_PERSISTED_WARNING_EMITTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

lazy_static::lazy_static! {
    /// The service owns the live ScreenCam state. IPC and the heartbeat only
    /// borrow it briefly, so neither retains the capture loop after shutdown.
    static ref LIVE_STATE: Mutex<Option<Weak<SharedState>>> = Mutex::new(None);
    static ref LIVE_STATE_RECONCILED: Condvar = Condvar::new();
    /// Serializes persistence plus publication with startup reconciliation.
    static ref DISPLAY_POLICY_UPDATE: Mutex<()> = Mutex::new(());
}

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
    /// Bounded hand-off to the panel-driven preview publisher. Owned here on
    /// purpose: `SharedState` outlives every `capture_loop` run and every
    /// watchdog restart, whereas the capturer, the encoder and the RTSP
    /// sessions do not. A preview session can last up to 1800 s and can easily span a
    /// display change, so recreating the tap with the capture loop would
    /// silently reset the counters the publisher is watching.
    ///
    /// Starts inactive. `PreviewControl` activates it only while an IPC-owned
    /// preview session exists, so normal capture without preview remains a
    /// single atomic load per frame.
    preview_tap: Arc<PreviewTap>,
    /// Owns the single preview session, drives the same tap stored above and,
    /// since C1, the SRT publisher that drains it. Nothing it does can reach
    /// the capture loop: it only ever opens and closes the tap.
    preview_control: Arc<PreviewControl>,
}

impl SharedState {
    fn new() -> Self {
        let (selected_display_id, fallback_to_primary) = load_display_policy();
        Self::new_with_display_policy(selected_display_id, fallback_to_primary)
    }

    fn new_with_display_policy(
        selected_display_id: Option<String>,
        fallback_to_primary: bool,
    ) -> Self {
        let preview_tap = Arc::new(PreviewTap::new());
        let preview_control = Arc::new(PreviewControl::new(
            Arc::clone(&preview_tap),
            Arc::new(preview::publisher::SrtPublisherSpawner),
        ));
        Self {
            sessions: Mutex::new(Vec::new()),
            stream_descriptor: Mutex::new(StreamDescriptorState::new()),
            last_confirmed_resolution: Mutex::new(None),
            display_selection: Mutex::new(display::DisplaySelectionState::new(
                selected_display_id,
                fallback_to_primary,
            )),
            reconfigure_generation: AtomicU64::new(0),
            preview_tap,
            preview_control,
        }
    }

    /// A handle the future publisher can hold independently of this state.
    #[allow(dead_code)]
    fn preview_tap(&self) -> Arc<PreviewTap> {
        Arc::clone(&self.preview_tap)
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

    fn display_snapshot(&self) -> display::DisplayRuntimeState {
        self.display_selection.lock().unwrap().snapshot()
    }

    #[cfg(test)]
    fn apply_display_policy(
        &self,
        selected_display_id: Option<&str>,
        fallback_to_primary: Option<bool>,
    ) -> bool {
        let mut selection = self.display_selection.lock().unwrap();
        let requires_reconfigure =
            selection.update_policy(selected_display_id, fallback_to_primary);
        if requires_reconfigure {
            advance_generation(&self.reconfigure_generation);
        }
        requires_reconfigure
    }

    fn reconcile_display_policy(
        &self,
        selected_display_id: Option<&str>,
        fallback_to_primary: bool,
    ) -> bool {
        let mut selection = self.display_selection.lock().unwrap();
        let requires_reconfigure =
            selection.reconcile_policy(selected_display_id, fallback_to_primary);
        if requires_reconfigure {
            advance_generation(&self.reconfigure_generation);
        }
        requires_reconfigure
    }

    fn display_policy_matches(
        &self,
        selected_display_id: Option<&str>,
        fallback_to_primary: bool,
    ) -> bool {
        self.display_selection
            .lock()
            .unwrap()
            .policy_matches(selected_display_id, fallback_to_primary)
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
        // The descriptor lock is released by the end of this statement, and
        // the sessions lock is not taken until the next one. The tap is told
        // in between, holding neither: `PreviewTap::invalidate_stream` takes
        // no lock of its own, but calling it from inside either critical
        // section would add a second acquisition order to a module that
        // documents exactly one (see rtsp.rs's note on this pair).
        let epoch = self.stream_descriptor.lock().unwrap().invalidate();
        // Once per real invalidation, whether or not anything is previewing:
        // an inactive tap still records the epoch and bumps its generation,
        // which is what lets a publisher that activates later start from a
        // truthful snapshot instead of assuming epoch 0.
        self.preview_tap.invalidate_stream(epoch);
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

fn load_display_policy() -> (Option<String>, bool) {
    let selected =
        hbb_common::config::LocalConfig::get_option_from_file(SELECTED_DISPLAY_ID_OPTION_KEY);
    let selected_display_id = if selected.is_empty() {
        None
    } else if display::validate_display_id(&selected).is_ok() {
        Some(selected)
    } else {
        log::warn!("[screencam] ignoring invalid persisted display selection");
        None
    };
    let persisted_fallback =
        hbb_common::config::LocalConfig::get_option_from_file(FALLBACK_TO_PRIMARY_OPTION_KEY);
    let fallback_to_primary = match persisted_fallback.as_str() {
        "Y" => true,
        "N" => false,
        "" => true,
        _ => {
            if !INVALID_FALLBACK_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
                log::warn!("[screencam] ignoring invalid persisted fallback policy");
            }
            true
        }
    };
    (selected_display_id, fallback_to_primary)
}

/// Validates the only display identifier accepted through the service IPC.
/// The implementation is shared with Windows display enumeration so policy
/// cannot admit a value that inventory lookup would later reject.
pub(crate) fn validate_display_policy_id(display_id: &str) -> bool {
    display::validate_display_id(display_id).is_ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisplayPolicyRejection {
    InvalidPolicy,
    PersistenceFailed,
    IpcUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisplayPolicyApplyState {
    Applied,
    PendingReconciliation,
    Rejected(DisplayPolicyRejection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DisplayPolicyApplyOutcome {
    pub state: DisplayPolicyApplyState,
    pub changed: bool,
}

#[derive(Clone, Debug)]
struct PersistedDisplayPolicy {
    selected_display_id: Option<String>,
    fallback_to_primary: bool,
}

/// Backing store for the persisted display policy. Production reads and writes
/// the real `LocalConfig`; the decision logic in
/// [`persist_and_apply_display_policy_update_in`] is shared verbatim so tests
/// can drive the persistence outcomes (`Ok(false)`, `Err`) that a healthy
/// filesystem will not produce on demand.
pub(crate) trait DisplayPolicyStore {
    fn get(&self, key: &str) -> String;
    fn set_options_atomic(&self, updates: &[(String, String)]) -> ResultType<bool>;
}

pub(crate) struct LocalConfigDisplayPolicyStore;

impl DisplayPolicyStore for LocalConfigDisplayPolicyStore {
    fn get(&self, key: &str) -> String {
        hbb_common::config::LocalConfig::get_option(key)
    }

    fn set_options_atomic(&self, updates: &[(String, String)]) -> ResultType<bool> {
        hbb_common::config::LocalConfig::set_options_atomic(updates)
    }
}

fn registered_live_state() -> Option<Arc<SharedState>> {
    LIVE_STATE.lock().unwrap().as_ref().and_then(Weak::upgrade)
}

pub(crate) fn apply_preview_start(
    request: PreviewStartRequest,
    local_rustdesk_id: &str,
) -> PreviewControlOutcome {
    let session_id = request.session_id.clone();
    registered_live_state()
        .map(|state| state.preview_control.start(request, local_rustdesk_id))
        .unwrap_or_else(|| PreviewControlOutcome::unavailable(session_id))
}

pub(crate) fn apply_preview_stop(
    request: PreviewStopRequest,
    local_rustdesk_id: &str,
) -> PreviewControlOutcome {
    let session_id = request.session_id.clone();
    registered_live_state()
        .map(|state| state.preview_control.stop(request, local_rustdesk_id))
        .unwrap_or_else(|| PreviewControlOutcome::unavailable(session_id))
}

/// Maps an already-reconciled live state onto the ACK outcome. Kept separate so
/// the Condvar path and the direct path cannot drift apart.
fn outcome_for_live_state(
    state: &SharedState,
    policy: &PersistedDisplayPolicy,
    changed: bool,
) -> DisplayPolicyApplyOutcome {
    if state.display_policy_matches(
        policy.selected_display_id.as_deref(),
        policy.fallback_to_primary,
    ) {
        DisplayPolicyApplyOutcome {
            state: DisplayPolicyApplyState::Applied,
            changed,
        }
    } else {
        DisplayPolicyApplyOutcome {
            state: DisplayPolicyApplyState::Rejected(DisplayPolicyRejection::IpcUnavailable),
            changed: false,
        }
    }
}

fn publish_display_policy(
    policy: &PersistedDisplayPolicy,
    changed: bool,
) -> DisplayPolicyApplyOutcome {
    let Some(state) = registered_live_state() else {
        return DisplayPolicyApplyOutcome {
            state: DisplayPolicyApplyState::PendingReconciliation,
            changed,
        };
    };
    state.reconcile_display_policy(
        policy.selected_display_id.as_deref(),
        policy.fallback_to_primary,
    );
    outcome_for_live_state(&state, policy, changed)
}

fn wait_for_display_policy_reconciliation(
    policy: &PersistedDisplayPolicy,
    changed: bool,
    timeout: Duration,
) -> DisplayPolicyApplyOutcome {
    let unavailable = DisplayPolicyApplyOutcome {
        state: DisplayPolicyApplyState::Rejected(DisplayPolicyRejection::IpcUnavailable),
        changed: false,
    };
    let started = Instant::now();
    let mut live = LIVE_STATE.lock().unwrap();
    loop {
        if let Some(state) = live.as_ref().and_then(Weak::upgrade) {
            drop(live);
            return outcome_for_live_state(&state, policy, changed);
        }
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            return unavailable;
        };
        let (next, wait_result) = LIVE_STATE_RECONCILED.wait_timeout(live, remaining).unwrap();
        live = next;
        if wait_result.timed_out() {
            // A registration landing exactly at expiry must not be reported as
            // unavailable, so check the predicate once more before giving up.
            return match live.as_ref().and_then(Weak::upgrade) {
                Some(state) => {
                    drop(live);
                    outcome_for_live_state(&state, policy, changed)
                }
                None => unavailable,
            };
        }
    }
}

/// Persists the complete partial update atomically, then publishes it to the
/// live state while startup reconciliation is excluded. A successful ACK is
/// possible only after the live state contains the complete persisted policy.
pub(crate) fn persist_and_apply_display_policy_update(
    selected_display_id: Option<&str>,
    fallback_to_primary: Option<bool>,
) -> DisplayPolicyApplyOutcome {
    persist_and_apply_display_policy_update_in(
        &LocalConfigDisplayPolicyStore,
        selected_display_id,
        fallback_to_primary,
        DISPLAY_POLICY_RECONCILIATION_TIMEOUT,
    )
}

fn persist_and_apply_display_policy_update_in<S: DisplayPolicyStore>(
    store: &S,
    selected_display_id: Option<&str>,
    fallback_to_primary: Option<bool>,
    reconciliation_timeout: Duration,
) -> DisplayPolicyApplyOutcome {
    let rejected = |rejection| DisplayPolicyApplyOutcome {
        state: DisplayPolicyApplyState::Rejected(rejection),
        changed: false,
    };
    if selected_display_id
        .map(|display_id| !validate_display_policy_id(display_id))
        .unwrap_or(false)
        || (selected_display_id.is_none() && fallback_to_primary.is_none())
    {
        return rejected(DisplayPolicyRejection::InvalidPolicy);
    }

    let pending = {
        let _policy_update = DISPLAY_POLICY_UPDATE.lock().unwrap();

        // The persisted selection is re-validated exactly as startup does in
        // `load_display_policy`. Without this, an unrelated fallback-only update
        // would promote a corrupted identifier into the live state and the
        // heartbeat, and the next restart would silently drop it again.
        let stored_selected = store.get(SELECTED_DISPLAY_ID_OPTION_KEY);
        let stored_selection_is_corrupt =
            !stored_selected.is_empty() && !validate_display_policy_id(&stored_selected);
        if stored_selection_is_corrupt
            && !INVALID_STORED_SELECTION_WARNING_EMITTED.swap(true, Ordering::Relaxed)
        {
            log::warn!("[screencam] discarding invalid persisted display selection");
        }
        let current_selected = (!stored_selected.is_empty() && !stored_selection_is_corrupt)
            .then_some(stored_selected);
        let current_fallback = store.get(FALLBACK_TO_PRIMARY_OPTION_KEY) != "N";

        let selected_changed = selected_display_id
            .map(|display_id| {
                current_selected
                    .as_deref()
                    .map_or(true, |current| !current.eq_ignore_ascii_case(display_id))
            })
            .unwrap_or(false);
        let fallback_changed = fallback_to_primary
            .map(|fallback| fallback != current_fallback)
            .unwrap_or(false);
        // A corrupt stored selection is repaired in the same write, so the file
        // never keeps a value the live state and heartbeat refuse to show.
        let clear_stale_selection = stored_selection_is_corrupt && !selected_changed;
        let changed = selected_changed || fallback_changed || clear_stale_selection;

        let policy = PersistedDisplayPolicy {
            selected_display_id: if selected_changed {
                selected_display_id.map(str::to_owned)
            } else {
                current_selected
            },
            fallback_to_primary: fallback_to_primary.unwrap_or(current_fallback),
        };

        if changed {
            let mut updates = Vec::with_capacity(2);
            if selected_changed {
                updates.push((
                    SELECTED_DISPLAY_ID_OPTION_KEY.to_owned(),
                    selected_display_id.unwrap_or_default().to_owned(),
                ));
            } else if clear_stale_selection {
                updates.push((SELECTED_DISPLAY_ID_OPTION_KEY.to_owned(), String::new()));
            }
            if fallback_changed {
                updates.push((
                    FALLBACK_TO_PRIMARY_OPTION_KEY.to_owned(),
                    if policy.fallback_to_primary { "Y" } else { "N" }.to_owned(),
                ));
            }
            match store.set_options_atomic(&updates) {
                // Only `Ok(true)` means the requested configuration reached the
                // file. Publishing on anything else would leave the live state
                // ahead of the persisted policy.
                Ok(true) => {}
                Ok(false) => {
                    if !POLICY_NOT_PERSISTED_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
                        log::warn!(
                            "[screencam] display policy was not stored; refusing to publish it"
                        );
                    }
                    return rejected(DisplayPolicyRejection::PersistenceFailed);
                }
                Err(_) => return rejected(DisplayPolicyRejection::PersistenceFailed),
            }
        }
        let outcome = publish_display_policy(&policy, changed);
        (policy, outcome)
    };

    if pending.1.state == DisplayPolicyApplyState::PendingReconciliation {
        wait_for_display_policy_reconciliation(
            &pending.0,
            pending.1.changed,
            reconciliation_timeout,
        )
    } else {
        pending.1
    }
}

fn register_and_reconcile_display_policy(state: &Arc<SharedState>) {
    register_and_reconcile_display_policy_with(state, load_display_policy);
}

fn register_and_reconcile_display_policy_with<F>(state: &Arc<SharedState>, load_policy: F)
where
    F: FnOnce() -> (Option<String>, bool),
{
    let _policy_update = DISPLAY_POLICY_UPDATE.lock().unwrap();
    let (selected_display_id, fallback_to_primary) = load_policy();
    state.reconcile_display_policy(selected_display_id.as_deref(), fallback_to_primary);
    *LIVE_STATE.lock().unwrap() = Some(Arc::downgrade(state));
    LIVE_STATE_RECONCILED.notify_all();
}

/// Produces the display portion of the heartbeat from one cloned runtime
/// snapshot. The state lock is released before JSON serialization or HTTP.
pub(crate) fn heartbeat_display_status() -> Option<serde_json::Value> {
    let state = {
        let live = LIVE_STATE.lock().unwrap();
        live.as_ref().and_then(Weak::upgrade)
    }?;
    let snapshot = state.display_snapshot();
    Some(heartbeat_display_status_from_snapshot(snapshot))
}

pub(crate) fn heartbeat_initial_display_status() -> serde_json::Value {
    let selected =
        hbb_common::config::LocalConfig::get_option_from_file(SELECTED_DISPLAY_ID_OPTION_KEY);
    let selected_display_id =
        (!selected.is_empty() && validate_display_policy_id(&selected)).then_some(selected);
    heartbeat_display_status_from_snapshot(display::DisplayRuntimeState {
        available_displays: Vec::new(),
        selected_display_id,
        active_display_id: None,
        fallback_active: false,
        display_warning: None,
    })
}

fn heartbeat_display_status_from_snapshot(
    snapshot: display::DisplayRuntimeState,
) -> serde_json::Value {
    let available_displays = snapshot
        .available_displays
        .into_iter()
        .map(|display| {
            serde_json::json!({
                "display_id": display.display_id,
                "name": display.name,
                "index": display.index,
                "width": display.width,
                "height": display.height,
                "primary": display.primary,
                "connected": display.connected,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "available_displays": available_displays,
        "selected_display_id": snapshot.selected_display_id,
        "active_display_id": snapshot.active_display_id,
        "fallback_active": snapshot.fallback_active,
        "display_warning": snapshot.display_warning,
    })
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
        register_and_reconcile_display_policy(&state);
        if let Err(e) = rtsp::start_listener(cfg.rtsp_port, state.clone()) {
            log::error!("[screencam] failed to start RTSP listener, giving up: {e:?}");
            *LIVE_STATE.lock().unwrap() = None;
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
    let mut has_last_yuv = false;
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
                    has_last_yuv = matches!(&input, EncodeInput::YUV(_));
                    encode_screen_cam_input(
                        &mut encoder,
                        input,
                        &state,
                        &mut payloader,
                        stream_epoch,
                        start.elapsed(),
                        RTP_MTU,
                    );
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                capture_errors.record_would_block();
                // DXGI reports WouldBlock while the desktop and pointer are
                // unchanged. Keep feeding the last converted image only while
                // a preview is active, so the encoder's frame-count GOP still
                // produces an IDR about every two wall-clock seconds.
                if should_repeat_preview_frame(state.preview_tap.is_active(), has_last_yuv) {
                    encode_screen_cam_input(
                        &mut encoder,
                        EncodeInput::YUV(&yuv),
                        &state,
                        &mut payloader,
                        stream_epoch,
                        start.elapsed(),
                        RTP_MTU,
                    );
                }
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

fn should_repeat_preview_frame(preview_active: bool, has_last_yuv: bool) -> bool {
    preview_active && has_last_yuv
}

fn encode_screen_cam_input(
    encoder: &mut Encoder,
    input: EncodeInput<'_>,
    state: &SharedState,
    payloader: &mut rtp::H264Payloader,
    stream_epoch: u64,
    elapsed: Duration,
    mtu: usize,
) {
    let pts_ms = elapsed.as_millis() as i64;
    match encoder.encode_to_message(input, pts_ms) {
        Ok(vf) => {
            if let Some(video_frame::Union::H264s(h264s)) = vf.union {
                for frame in h264s.frames.iter() {
                    // `frame.pts`/`frame.key` are the encoder's own
                    // millisecond timestamp and keyframe flag, forwarded for
                    // the preview. `elapsed` remains the independent RTP clock.
                    handle_access_unit(
                        state,
                        payloader,
                        &frame.data,
                        stream_epoch,
                        frame.pts,
                        frame.key,
                        elapsed,
                        mtu,
                    );
                }
            }
        }
        Err(error) => log::error!("[screencam] encode error: {error:?}"),
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

/// `pts_ms` and `keyframe` come straight from the encoder's own
/// `EncodedVideoFrame` (`f.pts`, `f.key`) and exist for the preview tap.
/// `elapsed` remains the capture clock the RTP timestamp is derived from —
/// the two time bases stay separate on purpose, so wiring the preview cannot
/// perturb what an NVR already sees over RTSP.
fn handle_access_unit(
    state: &SharedState,
    payloader: &mut rtp::H264Payloader,
    data: &[u8],
    stream_epoch: u64,
    pts_ms: i64,
    keyframe: bool,
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

    // Preview tap. Placed exactly here on purpose: after the epoch has been
    // accepted, so a stale access unit can never reach the publisher, and
    // *before* the RTSP session lookup below, so the preview keeps receiving
    // frames even when nobody is watching over RTSP — which is the normal
    // case for a panel-driven preview, and the reason the early return a few
    // lines down would otherwise starve it.
    //
    // `has_sps`/`has_pps` reuse the NAL split above rather than parsing
    // `data` again, and describe *this* access unit rather than the cached
    // descriptor: the publisher needs to know whether the parameter sets are
    // in-band right here, while the descriptor only remembers that they were
    // seen at some point in this epoch.
    let has_sps = nals
        .iter()
        .any(|nal| rtp::nal_unit_type(nal) == rtp::NAL_TYPE_SPS);
    let has_pps = nals
        .iter()
        .any(|nal| rtp::nal_unit_type(nal) == rtp::NAL_TYPE_PPS);
    // Deliberately ignored. A frame the preview cannot take (a negative PTS
    // from the encoder, say) is skipped for preview only — RTSP carries on
    // untouched, nothing returns early, and nothing is logged, because this
    // runs once per frame and a rejection is a property of the frame, which
    // would turn any log into a per-frame log. While the tap is inactive this
    // is one atomic load and nothing else: no validation, no copy.
    let _ = state.preview_tap.push_annexb_copy_if_active(
        stream_epoch,
        pts_ms,
        keyframe,
        has_sps,
        has_pps,
        data,
    );

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

    static DISPLAY_POLICY_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Serializes every test that touches the process-wide policy globals and
    /// restores all of them on drop, so a failing test cannot leak state into
    /// the next one: `LIVE_STATE`, the two `LocalConfig` keys and the storage
    /// path the real `set_options_atomic` writes to. `DISPLAY_POLICY_UPDATE` is
    /// taken and released by the code under test itself.
    struct DisplayPolicyTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        app_name: String,
        redirected_root: Option<std::path::PathBuf>,
        previous_selected: String,
        previous_fallback: String,
    }

    impl DisplayPolicyTestGuard {
        fn new() -> Self {
            // Tolerate poisoning: a panicking test must not cascade into the
            // rest of the policy suite.
            let _lock = DISPLAY_POLICY_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Force LOCAL_CONFIG to load from the real file *before* the storage
            // path is redirected, so the developer's configuration is preserved
            // in memory and restored untouched on drop.
            let previous_selected =
                hbb_common::config::LocalConfig::get_option(SELECTED_DISPLAY_ID_OPTION_KEY);
            let previous_fallback =
                hbb_common::config::LocalConfig::get_option(FALLBACK_TO_PRIMARY_OPTION_KEY);
            let app_name = hbb_common::config::APP_NAME.read().unwrap().clone();
            *hbb_common::config::APP_NAME.write().unwrap() =
                format!("sehcontrol-screencam-test-{}", uuid::Uuid::new_v4());
            let redirected_root = hbb_common::config::Config::file()
                .parent()
                .and_then(std::path::Path::parent)
                .map(std::path::Path::to_path_buf);
            *LIVE_STATE.lock().unwrap() = None;
            Self {
                _lock,
                app_name,
                redirected_root,
                previous_selected,
                previous_fallback,
            }
        }

        fn seed(&self, selected: &str, fallback: &str) {
            hbb_common::config::LocalConfig::set_option(
                SELECTED_DISPLAY_ID_OPTION_KEY.to_owned(),
                selected.to_owned(),
            );
            hbb_common::config::LocalConfig::set_option(
                FALLBACK_TO_PRIMARY_OPTION_KEY.to_owned(),
                fallback.to_owned(),
            );
        }

        fn stored(&self, key: &str) -> String {
            hbb_common::config::LocalConfig::get_option(key)
        }

        fn stored_on_disk(&self, key: &str) -> String {
            hbb_common::config::LocalConfig::get_option_from_file(key)
        }
    }

    impl Drop for DisplayPolicyTestGuard {
        fn drop(&mut self) {
            *LIVE_STATE.lock().unwrap() = None;
            // Restore the two keys while still redirected, so the real
            // configuration file is never rewritten by the test suite.
            hbb_common::config::LocalConfig::set_option(
                SELECTED_DISPLAY_ID_OPTION_KEY.to_owned(),
                self.previous_selected.clone(),
            );
            hbb_common::config::LocalConfig::set_option(
                FALLBACK_TO_PRIMARY_OPTION_KEY.to_owned(),
                self.previous_fallback.clone(),
            );
            if let Some(root) = &self.redirected_root {
                let _ = std::fs::remove_dir_all(root);
            }
            *hbb_common::config::APP_NAME.write().unwrap() = self.app_name.clone();
        }
    }

    enum StubStoreOutcome {
        Stored,
        NotStored,
        Failed,
    }

    /// Drives the persistence outcomes a healthy filesystem will not produce on
    /// demand. The decision logic under test is the production one — only the
    /// store behind it is substituted.
    struct StubPolicyStore {
        selected: String,
        fallback: String,
        outcome: StubStoreOutcome,
        writes: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl StubPolicyStore {
        fn new(selected: &str, fallback: &str, outcome: StubStoreOutcome) -> Self {
            Self {
                selected: selected.to_owned(),
                fallback: fallback.to_owned(),
                outcome,
                writes: Mutex::new(Vec::new()),
            }
        }
    }

    impl DisplayPolicyStore for StubPolicyStore {
        fn get(&self, key: &str) -> String {
            if key == SELECTED_DISPLAY_ID_OPTION_KEY {
                self.selected.clone()
            } else {
                self.fallback.clone()
            }
        }

        fn set_options_atomic(&self, updates: &[(String, String)]) -> ResultType<bool> {
            self.writes.lock().unwrap().push(updates.to_vec());
            match self.outcome {
                StubStoreOutcome::Stored => Ok(true),
                StubStoreOutcome::NotStored => Ok(false),
                StubStoreOutcome::Failed => Err(anyhow!("simulated persistence failure")),
            }
        }
    }

    fn registered_state(selected: Option<&str>, fallback: bool) -> Arc<SharedState> {
        let state = Arc::new(SharedState::new_with_display_policy(
            selected.map(str::to_owned),
            fallback,
        ));
        let policy = (selected.map(str::to_owned), fallback);
        register_and_reconcile_display_policy_with(&state, move || policy);
        state
    }

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
    fn every_effective_display_policy_change_reconfigures_once() {
        let state = SharedState::new_with_display_policy(None, true);
        let displays = inventory_with(vec![
            available_display(r"\\.\DISPLAY1", 0, 1920, 1080, true),
            available_display(r"\\.\DISPLAY2", 1, 1920, 1080, false),
        ]);
        let (initial, generation) = state.apply_display_inventory(&displays);
        assert!(state.activate_display_for_generation(generation, &initial.resolution));

        // Selecting the display already resolved as primary still changes the
        // persistent intent and therefore advances exactly once.
        assert!(state.apply_display_policy(Some(r"\\.\display1"), None));
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation.wrapping_add(1)
        );
        assert_eq!(
            state.display_snapshot().selected_display_id.as_deref(),
            Some(r"\\.\display1")
        );

        assert!(state.apply_display_policy(Some(r"\\.\DISPLAY2"), None));
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation.wrapping_add(2)
        );
        assert!(!state.apply_display_policy(Some(r"\\.\display2"), None));
    }

    #[test]
    fn unavailable_selection_and_fallback_changes_advance_policy_generation() {
        let state = SharedState::new_with_display_policy(Some(r"\\.\DISPLAY8".to_owned()), true);
        let displays = inventory_with(vec![available_display(
            r"\\.\DISPLAY1",
            0,
            1920,
            1080,
            true,
        )]);
        let (_, generation) = state.apply_display_inventory(&displays);

        assert!(state.apply_display_policy(Some(r"\\.\DISPLAY9"), Some(false)));
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation.wrapping_add(1)
        );
        assert!(!state.apply_display_policy(Some(r"\\.\display9"), Some(false)));
    }

    #[test]
    fn fallback_policy_can_resolve_an_unavailable_selection_once() {
        let state = SharedState::new_with_display_policy(Some(r"\\.\DISPLAY9".to_owned()), false);
        let displays = inventory_with(vec![available_display(
            r"\\.\DISPLAY1",
            3,
            1920,
            1080,
            true,
        )]);
        let (_, generation) = state.apply_display_inventory(&displays);
        assert_eq!(generation, 0);

        assert!(state.apply_display_policy(None, Some(true)));
        assert_eq!(state.reconfigure_generation.load(Ordering::SeqCst), 1);
        assert!(!state.apply_display_policy(None, Some(true)));
        let snapshot = state.display_snapshot();
        assert_eq!(
            snapshot.selected_display_id.as_deref(),
            Some(r"\\.\DISPLAY9")
        );
        assert_eq!(snapshot.active_display_id, None);
    }

    #[test]
    fn heartbeat_display_snapshot_keeps_selected_active_and_warning_together() {
        let snapshot = display::DisplayRuntimeState {
            available_displays: vec![available_display(r"\\.\DISPLAY9", 4, 1360, 768, true)],
            selected_display_id: Some(r"\\.\DISPLAY2".to_owned()),
            active_display_id: Some(r"\\.\DISPLAY9".to_owned()),
            fallback_active: true,
            display_warning: Some("selected display is unavailable".to_owned()),
        };
        let value = heartbeat_display_status_from_snapshot(snapshot);
        assert_eq!(value["available_displays"][0]["index"], 4);
        assert_eq!(value["selected_display_id"], r"\\.\DISPLAY2");
        assert_eq!(value["active_display_id"], r"\\.\DISPLAY9");
        assert_eq!(value["fallback_active"], true);
        assert_eq!(value["display_warning"], "selected display is unavailable");
    }

    #[test]
    fn heartbeat_display_snapshot_emits_null_warning_without_an_active_display() {
        let value = heartbeat_display_status_from_snapshot(display::DisplayRuntimeState {
            available_displays: Vec::new(),
            selected_display_id: None,
            active_display_id: None,
            fallback_active: false,
            display_warning: None,
        });
        assert!(value["available_displays"].as_array().unwrap().is_empty());
        assert!(value["selected_display_id"].is_null());
        assert!(value["active_display_id"].is_null());
        assert!(value["display_warning"].is_null());
    }

    #[test]
    fn initial_heartbeat_shape_contains_every_dynamic_display_field() {
        let value = heartbeat_display_status_from_snapshot(display::DisplayRuntimeState {
            available_displays: Vec::new(),
            selected_display_id: Some(r"\\.\DISPLAY2".to_owned()),
            active_display_id: None,
            fallback_active: false,
            display_warning: None,
        });
        assert!(value["available_displays"].as_array().unwrap().is_empty());
        assert_eq!(value["selected_display_id"], r"\\.\DISPLAY2");
        assert!(value["active_display_id"].is_null());
        assert_eq!(value["fallback_active"], false);
        assert!(value["display_warning"].is_null());
    }

    #[test]
    fn startup_registration_reconciles_policy_loaded_after_state_creation() {
        let _guard = DisplayPolicyTestGuard::new();
        let state = Arc::new(SharedState::new_with_display_policy(
            Some(r"\\.\DISPLAY1".to_owned()),
            true,
        ));
        register_and_reconcile_display_policy_with(&state, || {
            (Some(r"\\.\DISPLAY2".to_owned()), false)
        });
        let snapshot = state.display_snapshot();
        assert_eq!(
            snapshot.selected_display_id.as_deref(),
            Some(r"\\.\DISPLAY2")
        );
        assert_eq!(state.reconfigure_generation.load(Ordering::SeqCst), 1);
        *LIVE_STATE.lock().unwrap() = None;
    }

    #[test]
    fn live_policy_is_published_and_generation_advances_before_applied() {
        let _guard = DisplayPolicyTestGuard::new();
        let state = Arc::new(SharedState::new_with_display_policy(
            Some(r"\\.\DISPLAY1".to_owned()),
            true,
        ));
        register_and_reconcile_display_policy_with(&state, || {
            (Some(r"\\.\DISPLAY1".to_owned()), true)
        });
        let generation = state.reconfigure_generation.load(Ordering::SeqCst);
        let policy = PersistedDisplayPolicy {
            selected_display_id: Some(r"\\.\DISPLAY2".to_owned()),
            fallback_to_primary: false,
        };
        let outcome = publish_display_policy(&policy, true);
        assert_eq!(outcome.state, DisplayPolicyApplyState::Applied);
        assert!(outcome.changed);
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation.wrapping_add(1)
        );
        assert!(state.display_policy_matches(Some(r"\\.\DISPLAY2"), false));
        *LIVE_STATE.lock().unwrap() = None;
    }

    #[test]
    fn absent_live_state_is_pending_and_times_out_as_ipc_unavailable() {
        let _guard = DisplayPolicyTestGuard::new();
        *LIVE_STATE.lock().unwrap() = None;
        let policy = PersistedDisplayPolicy {
            selected_display_id: Some(r"\\.\DISPLAY3".to_owned()),
            fallback_to_primary: true,
        };
        assert_eq!(
            publish_display_policy(&policy, true).state,
            DisplayPolicyApplyState::PendingReconciliation
        );
        let outcome =
            wait_for_display_policy_reconciliation(&policy, true, Duration::from_millis(10));
        assert_eq!(
            outcome,
            DisplayPolicyApplyOutcome {
                state: DisplayPolicyApplyState::Rejected(DisplayPolicyRejection::IpcUnavailable),
                changed: false,
            }
        );
    }

    #[test]
    fn startup_reconciliation_wakes_pending_policy_before_ack() {
        let _guard = DisplayPolicyTestGuard::new();
        *LIVE_STATE.lock().unwrap() = None;
        let policy = PersistedDisplayPolicy {
            selected_display_id: Some(r"\\.\DISPLAY4".to_owned()),
            fallback_to_primary: false,
        };
        let waiting_policy = policy.clone();
        let waiter = thread::spawn(move || {
            wait_for_display_policy_reconciliation(&waiting_policy, true, Duration::from_secs(1))
        });
        thread::sleep(Duration::from_millis(20));
        let state = Arc::new(SharedState::new_with_display_policy(None, true));
        register_and_reconcile_display_policy_with(&state, || {
            (
                policy.selected_display_id.clone(),
                policy.fallback_to_primary,
            )
        });
        let outcome = waiter.join().unwrap();
        assert_eq!(outcome.state, DisplayPolicyApplyState::Applied);
        assert!(outcome.changed);
        assert!(state.display_policy_matches(Some(r"\\.\DISPLAY4"), false));
        *LIVE_STATE.lock().unwrap() = None;
    }

    #[test]
    fn idempotent_live_policy_returns_applied_without_generation_change() {
        let _guard = DisplayPolicyTestGuard::new();
        let state = Arc::new(SharedState::new_with_display_policy(
            Some(r"\\.\DISPLAY5".to_owned()),
            false,
        ));
        register_and_reconcile_display_policy_with(&state, || {
            (Some(r"\\.\DISPLAY5".to_owned()), false)
        });
        let generation = state.reconfigure_generation.load(Ordering::SeqCst);
        let outcome = publish_display_policy(
            &PersistedDisplayPolicy {
                selected_display_id: Some(r"\\.\display5".to_owned()),
                fallback_to_primary: false,
            },
            false,
        );
        assert_eq!(
            outcome,
            DisplayPolicyApplyOutcome {
                state: DisplayPolicyApplyState::Applied,
                changed: false,
            }
        );
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation
        );
        *LIVE_STATE.lock().unwrap() = None;
    }

    // ---------------------------------------------------------------------
    // Productive route: these drive `persist_and_apply_display_policy_update`
    // itself — real LocalConfig reads, real `set_options_atomic`, real
    // LIVE_STATE publication, real generation accounting and the real ACK
    // mapping. Only the persistence *outcome* is substituted where a healthy
    // filesystem cannot produce it on demand.
    // ---------------------------------------------------------------------

    #[test]
    fn productive_policy_update_persists_publishes_and_matches_heartbeat() {
        let guard = DisplayPolicyTestGuard::new();
        guard.seed("", "Y");
        let state = registered_state(None, true);
        // A real topology, so the new selection actually resolves and the
        // heartbeat is exercised without a "display unavailable" warning.
        let displays = inventory_with(vec![
            available_display(r"\\.\DISPLAY1", 0, 1920, 1080, true),
            available_display(r"\\.\DISPLAY2", 1, 1360, 768, false),
        ]);
        state.apply_display_inventory(&displays);
        let generation = state.reconfigure_generation.load(Ordering::SeqCst);

        let outcome = persist_and_apply_display_policy_update(Some(r"\\.\DISPLAY2"), Some(false));

        assert_eq!(outcome.state, DisplayPolicyApplyState::Applied);
        assert!(outcome.changed);
        // One generation for the whole combined change.
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation.wrapping_add(1)
        );
        assert_eq!(
            guard.stored(SELECTED_DISPLAY_ID_OPTION_KEY),
            r"\\.\DISPLAY2"
        );
        assert_eq!(guard.stored(FALLBACK_TO_PRIMARY_OPTION_KEY), "N");
        // The file, not only the in-memory copy.
        assert_eq!(
            guard.stored_on_disk(SELECTED_DISPLAY_ID_OPTION_KEY),
            r"\\.\DISPLAY2"
        );
        assert_eq!(guard.stored_on_disk(FALLBACK_TO_PRIMARY_OPTION_KEY), "N");
        // The heartbeat reports the effective policy.
        let heartbeat = heartbeat_display_status().unwrap();
        assert_eq!(heartbeat["selected_display_id"], r"\\.\DISPLAY2");
        assert_eq!(heartbeat["fallback_active"], false);
        assert!(heartbeat["display_warning"].is_null());
        assert_eq!(heartbeat["available_displays"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn productive_policy_update_is_idempotent_for_a_repeated_policy() {
        let guard = DisplayPolicyTestGuard::new();
        guard.seed(r"\\.\DISPLAY2", "N");
        let state = registered_state(Some(r"\\.\DISPLAY2"), false);
        let generation = state.reconfigure_generation.load(Ordering::SeqCst);

        // Same policy, different ASCII case.
        let outcome = persist_and_apply_display_policy_update(Some(r"\\.\display2"), Some(false));

        assert_eq!(outcome.state, DisplayPolicyApplyState::Applied);
        assert!(!outcome.changed);
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation
        );
        // The stored casing is left alone.
        assert_eq!(
            guard.stored(SELECTED_DISPLAY_ID_OPTION_KEY),
            r"\\.\DISPLAY2"
        );
    }

    #[test]
    fn productive_policy_update_discards_a_corrupt_stored_selection() {
        let guard = DisplayPolicyTestGuard::new();
        guard.seed("not-a-display-id", "Y");
        let state = registered_state(None, true);
        let generation = state.reconfigure_generation.load(Ordering::SeqCst);

        // Fallback-only update on top of a corrupted stored selection.
        let outcome = persist_and_apply_display_policy_update(None, Some(false));

        assert_eq!(outcome.state, DisplayPolicyApplyState::Applied);
        assert!(outcome.changed);
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation.wrapping_add(1),
            "the corrupt selection must not cost an extra generation"
        );
        // The corrupt identifier never reaches the live state ...
        assert_eq!(state.display_snapshot().selected_display_id, None);
        // ... nor the heartbeat ...
        assert!(heartbeat_display_status().unwrap()["selected_display_id"].is_null());
        // ... and persistence is left coherent with both.
        assert_eq!(guard.stored(SELECTED_DISPLAY_ID_OPTION_KEY), "");
        assert_eq!(guard.stored(FALLBACK_TO_PRIMARY_OPTION_KEY), "N");
        assert_eq!(guard.stored_on_disk(SELECTED_DISPLAY_ID_OPTION_KEY), "");
    }

    #[test]
    fn productive_policy_update_refuses_to_publish_when_nothing_was_stored() {
        let _guard = DisplayPolicyTestGuard::new();
        let state = registered_state(None, true);
        let generation = state.reconfigure_generation.load(Ordering::SeqCst);
        let store = StubPolicyStore::new("", "Y", StubStoreOutcome::NotStored);

        let outcome = persist_and_apply_display_policy_update_in(
            &store,
            Some(r"\\.\DISPLAY3"),
            None,
            Duration::from_millis(20),
        );

        assert_eq!(
            outcome,
            DisplayPolicyApplyOutcome {
                state: DisplayPolicyApplyState::Rejected(DisplayPolicyRejection::PersistenceFailed),
                changed: false,
            }
        );
        assert_eq!(store.writes.lock().unwrap().len(), 1);
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation
        );
        assert_eq!(state.display_snapshot().selected_display_id, None);
    }

    #[test]
    fn productive_policy_update_leaves_live_state_untouched_when_persistence_fails() {
        let _guard = DisplayPolicyTestGuard::new();
        let state = registered_state(Some(r"\\.\DISPLAY1"), true);
        let generation = state.reconfigure_generation.load(Ordering::SeqCst);
        let store = StubPolicyStore::new(r"\\.\DISPLAY1", "Y", StubStoreOutcome::Failed);

        let outcome = persist_and_apply_display_policy_update_in(
            &store,
            Some(r"\\.\DISPLAY7"),
            Some(false),
            Duration::from_millis(20),
        );

        assert_eq!(
            outcome,
            DisplayPolicyApplyOutcome {
                state: DisplayPolicyApplyState::Rejected(DisplayPolicyRejection::PersistenceFailed),
                changed: false,
            }
        );
        assert_eq!(
            state.reconfigure_generation.load(Ordering::SeqCst),
            generation
        );
        assert!(state.display_policy_matches(Some(r"\\.\DISPLAY1"), true));
        assert_eq!(
            heartbeat_display_status().unwrap()["selected_display_id"],
            r"\\.\DISPLAY1"
        );
    }

    #[test]
    fn productive_policy_update_times_out_without_live_state() {
        let _guard = DisplayPolicyTestGuard::new();
        let store = StubPolicyStore::new("", "Y", StubStoreOutcome::Stored);

        let outcome = persist_and_apply_display_policy_update_in(
            &store,
            Some(r"\\.\DISPLAY4"),
            None,
            Duration::from_millis(20),
        );

        assert_eq!(
            outcome,
            DisplayPolicyApplyOutcome {
                state: DisplayPolicyApplyState::Rejected(DisplayPolicyRejection::IpcUnavailable),
                changed: false,
            }
        );
    }

    #[test]
    fn productive_policy_update_waits_for_startup_reconciliation() {
        let _guard = DisplayPolicyTestGuard::new();
        let registrar = thread::spawn(|| {
            thread::sleep(Duration::from_millis(30));
            registered_state(Some(r"\\.\DISPLAY5"), true)
        });
        let store = StubPolicyStore::new("", "Y", StubStoreOutcome::Stored);

        let outcome = persist_and_apply_display_policy_update_in(
            &store,
            Some(r"\\.\DISPLAY5"),
            None,
            Duration::from_secs(2),
        );

        let state = registrar.join().unwrap();
        assert_eq!(outcome.state, DisplayPolicyApplyState::Applied);
        assert!(outcome.changed);
        assert!(state.display_policy_matches(Some(r"\\.\DISPLAY5"), true));
    }

    #[test]
    fn productive_policy_update_rejects_invalid_and_empty_requests() {
        let _guard = DisplayPolicyTestGuard::new();
        let store = StubPolicyStore::new("", "Y", StubStoreOutcome::Stored);
        for (selected, fallback) in [(Some(r"\\.\DISPLAY0"), None), (None, None)] {
            let outcome = persist_and_apply_display_policy_update_in(
                &store,
                selected,
                fallback,
                Duration::from_millis(20),
            );
            assert_eq!(
                outcome,
                DisplayPolicyApplyOutcome {
                    state: DisplayPolicyApplyState::Rejected(DisplayPolicyRejection::InvalidPolicy),
                    changed: false,
                }
            );
        }
        // Nothing was even attempted against the store.
        assert!(store.writes.lock().unwrap().is_empty());
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

/// Preview tap integration (Entrega 4). These exercise the wiring only —
/// where the tap sits in `handle_access_unit`, what metadata reaches it, and
/// that invalidation is forwarded — never a publisher, a muxer or a socket.
#[cfg(test)]
mod preview_tap_tests {
    use super::*;

    #[test]
    fn static_frame_is_repeated_only_for_an_active_preview_with_a_cached_image() {
        assert!(should_repeat_preview_frame(true, true));
        assert!(!should_repeat_preview_frame(false, true));
        assert!(!should_repeat_preview_frame(true, false));
        assert!(!should_repeat_preview_frame(false, false));
    }

    /// SPS + PPS + IDR, the shape a keyframe access unit really has.
    const KEYFRAME_AU: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1F, // SPS
        0, 0, 1, 0x68, 0xEE, // PPS
        0, 0, 1, 0x65, 0x88, 0x84, // IDR
    ];
    /// A non-IDR slice on its own, as inter frames arrive.
    const INTER_AU: &[u8] = &[0, 0, 1, 0x61, 0x20, 0x40];

    fn feed(state: &SharedState, data: &[u8], epoch: u64, pts_ms: i64, keyframe: bool) {
        let mut payloader = rtp::H264Payloader::new();
        handle_access_unit(
            state,
            &mut payloader,
            data,
            epoch,
            pts_ms,
            keyframe,
            Duration::from_millis(1),
            1200,
        );
    }

    #[test]
    fn shared_state_owns_one_inactive_tap() {
        let state = SharedState::new();
        let tap = state.preview_tap();
        assert_eq!(tap.capacity(), 8, "default capacity");
        assert!(!tap.is_active(), "must not activate itself");
        assert!(tap.is_empty());
        let stats = tap.stats();
        assert_eq!(stats.dropped_total, 0);
        assert_eq!(stats.invalidation_generation, 0);
        assert_eq!(stats.discarded_on_invalidate_total, 0);
    }

    #[test]
    fn the_accessor_hands_out_the_same_instance() {
        let state = Arc::new(SharedState::new());
        let first = state.preview_tap();
        let second = state.preview_tap();
        assert!(Arc::ptr_eq(&first, &second), "one tap, not one per call");
        // A capture_loop restart re-reads this state; it does not rebuild it,
        // so a publisher holding this handle keeps its counters across one.
        first.activate();
        let after_restart = Arc::clone(&state).preview_tap();
        assert!(after_restart.is_active(), "state survived the restart");
        assert!(Arc::ptr_eq(&first, &after_restart));
    }

    #[test]
    fn an_inactive_tap_receives_nothing() {
        let state = SharedState::new();
        for index in 0..20i64 {
            feed(&state, KEYFRAME_AU, 0, index * 40, index == 0);
        }
        let tap = state.preview_tap();
        assert!(tap.is_empty());
        assert_eq!(tap.dropped_total(), 0);
    }

    /// The point of tapping before the RTSP session lookup: with zero
    /// sessions `handle_access_unit` returns early, and the preview must
    /// still have been fed.
    #[test]
    fn an_active_tap_is_fed_even_with_no_rtsp_sessions() {
        let state = SharedState::new();
        assert!(state.preview_tap().activate().activated);
        assert!(
            state.sessions.lock().unwrap().is_empty(),
            "no RTSP consumer at all"
        );

        feed(&state, KEYFRAME_AU, 0, 40, true);

        let tap = state.preview_tap();
        assert_eq!(tap.len(), 1, "the tap ran before the early return");
        let unit = tap.pop().expect("queued");
        assert_eq!(unit.epoch, 0);
        assert_eq!(unit.pts_ms, 40);
        assert!(unit.keyframe);
        assert!(unit.has_sps);
        assert!(unit.has_pps);
        assert_eq!(&*unit.annexb, KEYFRAME_AU, "payload must be byte-exact");
    }

    /// The C1 regression guard for the outlet that already works.
    ///
    /// Whatever the preview is doing — saturated, dropping frames, no consumer
    /// at all because the publisher died — `handle_access_unit` must hand RTSP
    /// and UDP exactly what it handed them before there was a tap. The proxy for
    /// "exactly what" is the stream descriptor: it is what DESCRIBE answers
    /// from, what the SPS/PPS and IDR readiness live in, and what the RTP
    /// dispatch below it is gated on.
    #[test]
    fn a_saturated_preview_tap_changes_nothing_the_rtsp_path_sees() {
        fn descriptor_after_a_burst(activate_tap: bool) -> (bool, bool, u64, (usize, usize)) {
            let state = SharedState::new();
            if activate_tap {
                assert!(state.preview_tap().activate().activated);
            }
            // Far more access units than the tap's 8 slots, and nobody popping,
            // so with the tap active this burst saturates it many times over.
            for pts in 0..50 {
                feed(&state, KEYFRAME_AU, 0, pts, true);
                feed(&state, INTER_AU, 0, pts, false);
            }
            let descriptor = state.stream_descriptor();
            (
                descriptor.is_ready(),
                descriptor.idr_ready,
                state.preview_tap().dropped_total(),
                state.stream_dimensions(),
            )
        }

        let (ready, idr, dropped, dimensions) = descriptor_after_a_burst(true);
        assert!(dropped > 0, "the burst must really have saturated the tap");
        let baseline = descriptor_after_a_burst(false);
        assert_eq!(
            (ready, idr, dimensions),
            (baseline.0, baseline.1, baseline.3),
            "an overflowing preview tap must not change what RTSP is told"
        );
        assert_eq!(baseline.2, 0, "an inactive tap cannot drop anything");
    }

    #[test]
    fn parameter_set_flags_describe_this_access_unit() {
        let cases: [(&[u8], bool, bool); 4] = [
            (KEYFRAME_AU, true, true),
            (&[0, 0, 1, 0x67, 0x64, 0x00, 0x1F], true, false), // SPS only
            (&[0, 0, 1, 0x68, 0xEE], false, true),             // PPS only
            (INTER_AU, false, false),                          // neither
        ];
        for (data, expect_sps, expect_pps) in cases {
            let state = SharedState::new();
            assert!(state.preview_tap().activate().activated);
            feed(&state, data, 0, 0, false);
            let unit = state
                .preview_tap()
                .pop()
                .unwrap_or_else(|| panic!("nothing queued for {data:02X?}"));
            assert_eq!(unit.has_sps, expect_sps, "SPS for {data:02X?}");
            assert_eq!(unit.has_pps, expect_pps, "PPS for {data:02X?}");
            assert_eq!(&*unit.annexb, data);
        }
    }

    #[test]
    fn the_keyframe_flag_is_the_encoders_own() {
        let state = SharedState::new();
        assert!(state.preview_tap().activate().activated);
        // Deliberately contradictory: an IDR payload flagged as not a
        // keyframe. This task forwards `f.key` and does not second-guess it.
        feed(&state, KEYFRAME_AU, 0, 0, false);
        assert!(!state.preview_tap().pop().expect("queued").keyframe);
        feed(&state, INTER_AU, 0, 40, true);
        assert!(state.preview_tap().pop().expect("queued").keyframe);
    }

    #[test]
    fn a_negative_pts_is_skipped_for_preview_without_disturbing_rtsp() {
        let state = SharedState::new();
        let epoch = state.stream_epoch();
        assert!(state.set_stream_dimensions(epoch, 1920, 1080));
        let tap = state.preview_tap();
        assert!(tap.activate().activated);
        let dropped_before = tap.dropped_total();
        assert_eq!(
            tap.push_annexb_copy_if_active(epoch, -1, true, true, true, KEYFRAME_AU),
            Err(super::preview::tap::AccessUnitError::NegativePts),
            "the active tap rejects the encoder's negative PTS"
        );
        assert!(tap.is_empty(), "the rejected probe was not queued");

        // A payload that completes the descriptor, so the RTSP side is
        // observably still doing its job.
        feed(&state, KEYFRAME_AU, epoch, -1, true);

        assert!(tap.is_empty(), "not queued for preview");
        assert_eq!(
            tap.dropped_total(),
            dropped_before,
            "and not counted as loss"
        );
        assert!(tap.is_active(), "the tap stays open");
        let descriptor = state.stream_descriptor();
        assert_eq!((descriptor.width, descriptor.height), (1920, 1080));
        assert_eq!(
            descriptor.sps.as_deref(),
            Some(&[0x67, 0x64, 0x00, 0x1F][..])
        );
        assert_eq!(descriptor.pps.as_deref(), Some(&[0x68, 0xEE][..]));
        assert!(descriptor.idr_ready, "the complete IDR was observed");
        assert!(descriptor.is_ready(), "the RTSP descriptor was completed");
    }

    #[test]
    fn a_stale_epoch_never_reaches_the_tap() {
        let state = SharedState::new();
        assert!(state.preview_tap().activate().activated);
        let current = state.stream_epoch();

        feed(&state, KEYFRAME_AU, current.wrapping_add(7), 0, true);

        let tap = state.preview_tap();
        assert!(tap.is_empty(), "apply_stream_access_unit refused it first");
        assert_eq!(tap.dropped_total(), 0);
        assert_eq!(tap.stats().discarded_on_invalidate_total, 0);
        assert!(
            !state.stream_descriptor().is_ready(),
            "and the descriptor was not touched either"
        );
    }

    #[test]
    fn saturation_keeps_the_most_recent_eight_access_units() {
        let state = SharedState::new();
        assert!(state.preview_tap().activate().activated);
        for index in 0..12i64 {
            feed(&state, INTER_AU, 0, index * 40, false);
        }
        let tap = state.preview_tap();
        assert_eq!(tap.len(), 8, "bounded at the default capacity");
        assert_eq!(tap.dropped_total(), 4, "12 fed, 8 kept");
        let kept: Vec<i64> = std::iter::from_fn(|| tap.pop())
            .map(|unit| unit.pts_ms)
            .collect();
        assert_eq!(kept, vec![160, 200, 240, 280, 320, 360, 400, 440]);
    }

    #[test]
    fn invalidation_reaches_the_tap_exactly_once() {
        let state = SharedState::new();
        assert!(state.preview_tap().activate().activated);
        for index in 0..3i64 {
            feed(&state, INTER_AU, 0, index * 40, false);
        }
        assert_eq!(state.preview_tap().len(), 3);

        let new_epoch = state.invalidate_stream();

        let tap = state.preview_tap();
        let stats = tap.stats();
        assert_eq!(new_epoch, 1, "the descriptor advanced");
        assert_eq!(stats.invalidated_epoch, new_epoch, "the tap got that epoch");
        assert_eq!(stats.invalidation_generation, 1, "exactly one event");
        assert_eq!(stats.queued, 0, "the queue was drained");
        assert_eq!(stats.discarded_on_invalidate_total, 3);
        assert_eq!(stats.dropped_total, 0, "not billed as saturation");
        assert!(stats.active, "invalidation must not close the tap");
    }

    #[test]
    fn successive_invalidations_advance_epoch_and_generation_together() {
        let state = SharedState::new();
        assert!(state.preview_tap().activate().activated);

        let first = state.invalidate_stream();
        let second = state.invalidate_stream();
        assert_eq!((first, second), (1, 2));

        let snapshot = state.preview_tap().invalidation_snapshot();
        assert_eq!(snapshot.epoch, second);
        assert_eq!(snapshot.generation, 2, "one generation per invalidation");
        assert_eq!(snapshot.epoch, state.stream_epoch(), "epochs agree");
    }

    #[test]
    fn invalidation_is_recorded_even_while_the_tap_is_inactive() {
        let state = SharedState::new();
        assert!(!state.preview_tap().is_active());
        state.invalidate_stream();
        state.invalidate_stream();
        let snapshot = state.preview_tap().invalidation_snapshot();
        assert_eq!(snapshot.generation, 2);
        assert_eq!(snapshot.epoch, state.stream_epoch());
        // Which is what lets a publisher activating later start from the
        // truth instead of assuming epoch 0.
        assert!(!state.preview_tap().is_active());
    }

    #[test]
    fn feeding_resumes_on_the_new_epoch_after_an_invalidation() {
        let state = SharedState::new();
        assert!(state.preview_tap().activate().activated);
        feed(&state, KEYFRAME_AU, 0, 0, true);
        let new_epoch = state.invalidate_stream();
        assert!(state.preview_tap().is_empty());

        // The old epoch is refused by apply_stream_access_unit; the new one
        // goes through.
        feed(&state, KEYFRAME_AU, 0, 40, true);
        assert!(state.preview_tap().is_empty(), "stale epoch refused");
        feed(&state, KEYFRAME_AU, new_epoch, 80, true);
        let unit = state.preview_tap().pop().expect("queued");
        assert_eq!(unit.epoch, new_epoch);
        assert_eq!(unit.pts_ms, 80);
    }

    /// The RTP side must be untouched by any of this: same functional
    /// packetization and same 90 kHz timestamp derived from `elapsed` — not
    /// from the `pts_ms` the preview now receives.
    #[test]
    fn rtp_packetization_is_unchanged_by_the_preview_wiring() {
        let state = SharedState::new();
        assert!(state.preview_tap().activate().activated);
        let nals = rtp::split_annexb_nals(KEYFRAME_AU);
        let elapsed = Duration::from_millis(250);
        let timestamp = (elapsed.as_secs_f64() * 90_000.0) as u32;

        let mut expected_payloader = rtp::H264Payloader::new();
        let expected = expected_payloader.packetize(&nals, timestamp, 1200);

        let mut actual_payloader = rtp::H264Payloader::new();
        handle_access_unit(
            &state,
            &mut actual_payloader,
            KEYFRAME_AU,
            0,
            // A wildly different preview timestamp: it must not leak into RTP.
            999_999,
            true,
            elapsed,
            1200,
        );

        let actual = actual_payloader.packetize(&nals, timestamp, 1200);
        assert_eq!(actual.len(), expected.len(), "same RTP packet count");
        assert!(!actual.is_empty(), "the fixture produces RTP packets");
        for (actual_packet, expected_packet) in actual.iter().zip(&expected) {
            assert!(actual_packet.len() >= 12 && expected_packet.len() >= 12);
            assert_eq!(actual_packet[0], expected_packet[0], "same RTP header");
            assert_eq!(
                actual_packet[1], expected_packet[1],
                "same marker and payload type"
            );
            assert_eq!(
                &actual_packet[4..8],
                &expected_packet[4..8],
                "same RTP timestamp"
            );
            assert_eq!(
                &actual_packet[12..],
                &expected_packet[12..],
                "same H.264 payload and fragmentation"
            );
        }

        let expected_sequences: Vec<u16> = expected
            .iter()
            .map(|packet| u16::from_be_bytes([packet[2], packet[3]]))
            .collect();
        let actual_sequences: Vec<u16> = actual
            .iter()
            .map(|packet| u16::from_be_bytes([packet[2], packet[3]]))
            .collect();
        let sequence_deltas = |sequences: &[u16]| {
            sequences
                .windows(2)
                .map(|pair| pair[1].wrapping_sub(pair[0]))
                .collect::<Vec<_>>()
        };
        assert!(
            expected_sequences
                .windows(2)
                .all(|pair| pair[1] == pair[0].wrapping_add(1)),
            "expected flow has consecutive sequences"
        );
        assert!(
            actual_sequences
                .windows(2)
                .all(|pair| pair[1] == pair[0].wrapping_add(1)),
            "actual flow has consecutive sequences"
        );
        assert_eq!(
            sequence_deltas(&actual_sequences),
            sequence_deltas(&expected_sequences),
            "same relative sequence progression"
        );

        let expected_ssrc = &expected[0][8..12];
        let actual_ssrc = &actual[0][8..12];
        assert!(
            expected
                .iter()
                .all(|packet| &packet[8..12] == expected_ssrc),
            "expected flow keeps one SSRC"
        );
        assert!(
            actual.iter().all(|packet| &packet[8..12] == actual_ssrc),
            "actual flow keeps one SSRC"
        );
        assert_eq!(
            state.preview_tap().pop().expect("queued").pts_ms,
            999_999,
            "the preview kept its own time base"
        );
    }
}
