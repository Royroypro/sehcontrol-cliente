// The panel's realtime channel, owned by the `--server` process.
//
// ScreenCam's capture, encoder and SRT publisher all live in this process
// precisely so they do not depend on the UI being open (see the comment on
// `screen_cam::start` in src/server.rs). The command channel did not: the
// authenticated WebSocket to the panel lived in Dart, so closing the window
// left a service that was capturing perfectly and a panel that could neither
// start a preview nor learn that one had started. Both directions matter:
//
// - panel -> client: `screen_cam.preview.start` / `.stop`.
// - client -> panel: the publisher's lifecycle transitions, which are what
//   move the session to `ready` and make the panel hand the browser a
//   playback URL. Without them a preview publishes into a gateway nobody is
//   ever told to read.
//
// The reconnection rules below are not invented here. They are the ones the
// Dart implementation documents as learned the hard way
// (flutter/lib/common/realtime_channel.dart), and each exists because its
// absence produced a client that went silent until restarted:
//
// - Every failure path arms a retry. "No API server yet" and "no token yet"
//   are transient during start-up and login, not permanent.
// - One socket, one reconnect, no overlap: the loop is strictly sequential.
// - A pong proves liveness; a successful write does not. A half-open socket
//   behind a proxy accepts writes forever and never reports a close.
//
// Nothing here logs the token, or a URL that carries it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hbb_common::{
    config::{Config, LocalConfig},
    log, tokio,
    websocket::WsFramedStream,
};

use super::{apply_preview_start, apply_preview_stop, PreviewStartRequest, PreviewStopRequest};

/// Matches the Dart channel so the panel sees the same cadence from either
/// client.
const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_TIMEOUT: Duration = Duration::from_secs(60);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT_MS: u64 = 20_000;

/// How often the publisher's state is re-read. Fast enough that the panel's
/// status feels immediate, slow enough to be invisible: the answer comes from
/// a mutex read in this same process, with no IPC in between.
const LIFECYCLE_POLL: Duration = Duration::from_millis(400);

/// A frame larger than this cannot be anything this client understands, and
/// decoding it would only hand a stranger a large allocation.
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// This loop retries every 5 s forever, so a reason it cannot run has to be
/// stated once and then stay quiet — otherwise it either floods the log or,
/// as the first version of this did, says nothing at all and leaves "never
/// connected" indistinguishable from "connected and idle".
static MISSING_API_LOGGED: AtomicBool = AtomicBool::new(false);
static MISSING_TOKEN_LOGGED: AtomicBool = AtomicBool::new(false);

fn log_once(flag: &AtomicBool, message: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        log::info!("{message}");
    }
}

pub(crate) fn start() {
    std::thread::Builder::new()
        .name("screencam-panel-link".to_owned())
        .spawn(|| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                log::error!("[screencam] panel link: no runtime, previews will need the window");
                return;
            };
            runtime.block_on(run());
        })
        .map(|_| ())
        .unwrap_or_else(|e| {
            log::error!("[screencam] panel link: thread failed to start: {e}");
        });
}

async fn run() {
    // Survives every session: a reconnect must not re-send transitions the
    // panel already acknowledged, and the publisher's counters do not restart
    // just because the socket did.
    let mut forwarder = LifecycleForwarder::default();
    loop {
        match connect().await {
            Some(mut stream) => {
                log::info!("[screencam] panel link connected");
                session(&mut stream, &mut forwarder).await;
                log::info!("[screencam] panel link disconnected");
            }
            None => {
                // Already logged by `connect` when it is worth logging.
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// `None` for anything that leaves us without a socket. Every such case is a
/// retry, never a stop.
async fn connect() -> Option<WsFramedStream> {
    let api = crate::common::get_api_server(
        Config::get_option("api-server"),
        Config::get_option("custom-rendezvous-server"),
    );
    if api.is_empty() || crate::is_public(&api) {
        // No panel configured, or the public rendezvous: there is no channel
        // to open.
        log_once(
            &MISSING_API_LOGGED,
            "[screencam] panel link idle: no panel configured",
        );
        return None;
    }
    // Deliberately NOT `access_token`: that one is written into the
    // interactive user's profile, while this process runs as SYSTEM and
    // resolves LocalConfig to ServiceProfiles\LocalService — a different file
    // where it is always absent. The UI mirrors it here over IPC instead
    // (see `is_screen_cam_policy` in flutter_ffi.rs).
    let token = LocalConfig::get_option("screencam-panel-token");
    if token.trim().is_empty() {
        // Nobody has logged in since this client was installed. Logged once
        // rather than never: the first version of this returned quietly here,
        // and a channel that never opened looked exactly like one that opened
        // and received nothing.
        log_once(
            &MISSING_TOKEN_LOGGED,
            "[screencam] panel link idle: no panel session yet, open the client once to sign in",
        );
        return None;
    }
    let endpoint = build_endpoint(&api, &token)?;
    match WsFramedStream::new(&endpoint, None, None, CONNECT_TIMEOUT_MS).await {
        Ok(stream) => Some(stream),
        Err(_) => {
            // The error can quote the URL, and the URL carries the token.
            log::warn!("[screencam] panel link connect failed");
            None
        }
    }
}

/// `http`/`ws` -> `ws`, `https`/`wss` -> `wss`, path + `/api/ws`, token in the
/// query. Mirrors `RealtimeChannelController.buildEndpoint` in Dart, because
/// the panel accepts exactly one shape.
fn build_endpoint(api_server: &str, token: &str) -> Option<String> {
    let trimmed = api_server.trim().trim_end_matches('/');
    let (scheme, rest) = match trimmed.split_once("://") {
        Some(("https", rest)) | Some(("wss", rest)) => ("wss", rest),
        Some(("http", rest)) | Some(("ws", rest)) => ("ws", rest),
        _ => return None,
    };
    if rest.is_empty() {
        return None;
    }
    Some(format!(
        "{scheme}://{rest}/api/ws?token={}",
        percent_encode(token)
    ))
}

/// Percent-encodes everything outside the unreserved set. Tokens are
/// base64url in practice, so this is normally a no-op -- but a token that
/// ever contained `&` or `#` would silently truncate the query instead of
/// failing, and that is not a bug worth discovering in production.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// One connected stretch. Returns when the socket is gone for any reason; the
/// caller reconnects.
async fn session(stream: &mut WsFramedStream, forwarder: &mut LifecycleForwarder) {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    // The first tick fires immediately; the connection is fresh, so skip it.
    ping.tick().await;
    let mut lifecycle = tokio::time::interval(LIFECYCLE_POLL);
    let mut last_pong = Instant::now();

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(bytes)) => {
                        if bytes.len() > MAX_FRAME_BYTES {
                            continue;
                        }
                        if handle_frame(&bytes) == FrameKind::Liveness {
                            last_pong = Instant::now();
                        }
                    }
                    // Close frame or a protocol error: both end the session.
                    _ => return,
                }
            }
            _ = ping.tick() => {
                // Checked before writing: a write that succeeds says nothing
                // about whether anyone is still listening.
                if last_pong.elapsed() > PONG_TIMEOUT {
                    log::warn!("[screencam] panel link pong timeout");
                    return;
                }
                if stream.send_text("ping".to_owned()).await.is_err() {
                    return;
                }
            }
            _ = lifecycle.tick() => {
                let status = super::preview_lifecycle_status_json();
                if let Some((identity, payload)) = forwarder.prepare(&status) {
                    if stream.send_text(payload).await.is_err() {
                        // Still owed: the identity is only recorded once it is
                        // actually on the wire, so the next session re-sends it.
                        return;
                    }
                    forwarder.commit(identity);
                }
            }
        }
    }
}

#[derive(PartialEq, Eq)]
enum FrameKind {
    /// A `pong`, or the `connected` greeting some deployments send first --
    /// both prove the peer is alive.
    Liveness,
    Other,
}

fn handle_frame(bytes: &[u8]) -> FrameKind {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return FrameKind::Other;
    };
    let kind = value["type"].as_str().unwrap_or_default();
    match kind {
        "pong" => return FrameKind::Liveness,
        "screen_cam.preview.start" => dispatch_start(&value["data"]),
        "screen_cam.preview.stop" => dispatch_stop(&value["data"]),
        "screen_cam.update" => dispatch_policy_update(&value["data"]),
        _ => {}
    }
    if kind == "connected" {
        FrameKind::Liveness
    } else {
        FrameKind::Other
    }
}

fn dispatch_start(data: &serde_json::Value) {
    let Some(request) = PreviewStartRequest::from_json(&data.to_string()) else {
        log::warn!("[screencam] panel link: invalid preview start");
        return;
    };
    let session = log_ref(&request.session_id);
    let outcome = apply_preview_start(request, Config::get_id().as_str());
    log::info!("[screencam] panel link preview start {session} -> {outcome:?}");
}

/// The panel's display selection, which arrives on the same event as the rest
/// of the ScreenCam policy.
///
/// This has to be handled here for the same reason the preview commands do:
/// with the window closed, the Dart channel that used to carry it does not
/// exist, and the panel sat on "Aplicando cambio de pantalla…" forever because
/// nothing on the device was listening.
///
/// Only the display fields are applied here. The rest of the policy
/// (`licensed`, `desired_state`, `mode`, RTSP credentials) still travels
/// through the UI's channel, so remotely enabling or disabling ScreenCam still
/// needs the window open — the same gap, not yet closed.
fn dispatch_policy_update(data: &serde_json::Value) {
    // Absent and invalid are different: an absent field means "unchanged" and
    // must not be turned into a write, while an invalid one is rejected rather
    // than passed to the applier.
    let selected = match data.get("selected_display_id") {
        Some(serde_json::Value::String(id)) if super::validate_display_policy_id(id) => Some(id.as_str()),
        Some(serde_json::Value::String(_)) => {
            log::warn!("[screencam] panel link: invalid display id in policy update");
            return;
        }
        _ => None,
    };
    let fallback = data.get("fallback_to_primary").and_then(|v| v.as_bool());
    if selected.is_none() && fallback.is_none() {
        // The event carried only fields this path does not own.
        return;
    }
    let outcome = super::persist_and_apply_display_policy_update(selected, fallback);
    log::info!("[screencam] panel link display policy -> {outcome:?}");
}

fn dispatch_stop(data: &serde_json::Value) {
    let Some(request) = PreviewStopRequest::from_json(&data.to_string()) else {
        log::warn!("[screencam] panel link: invalid preview stop");
        return;
    };
    let session = log_ref(&request.session_id);
    let outcome = apply_preview_stop(request, Config::get_id().as_str());
    log::info!("[screencam] panel link preview stop {session} -> {outcome:?}");
}

/// The full id identifies one live session; a prefix is enough to correlate
/// logs without writing the whole thing down.
fn log_ref(session_id: &str) -> String {
    format!("{}…", session_id.chars().take(8).collect::<String>())
}

/// De-duplicates the publisher's transitions and rebuilds each one as the
/// message the panel accepts.
///
/// The identity is `session_id:generation:sequence:event` rather than the
/// sequence alone: the publisher restarts its counters with every new session,
/// so a bare sequence would make a brand new session's first transition look
/// like one already delivered.
#[derive(Default)]
struct LifecycleForwarder {
    last_delivered: Option<String>,
}

/// Only these reach the panel. The payload is rebuilt field by field from a
/// fixed list instead of forwarding what arrived, so a field added later on
/// the publisher side cannot reach the panel by accident -- it sits next to a
/// URL, a token and a stream id.
const ALLOWED_EVENTS: &[&str] = &[
    "screen_cam.preview.connecting",
    "screen_cam.preview.started",
    "screen_cam.preview.failed",
    "screen_cam.preview.stopped",
];

/// Failure codes the publisher may state. Anything else becomes `unknown`, so
/// the panel's vocabulary stays closed even if the two sides drift.
const KNOWN_REASONS: &[&str] = &[
    "connect_failed",
    "send_failed",
    "mux_failed",
    "retries_exhausted",
];

impl LifecycleForwarder {
    /// `(identity, payload)` for a transition worth sending, or `None` for the
    /// idle snapshot, a malformed one, or one already delivered.
    fn prepare(&self, status: &str) -> Option<(String, String)> {
        let value = serde_json::from_str::<serde_json::Value>(status).ok()?;
        // Every field is required: a partial snapshot has no identity to
        // compare, so forwarding it could only produce duplicates.
        let event = value["event"].as_str()?;
        let session_id = value["session_id"].as_str()?;
        let rustdesk_id = value["rustdesk_id"].as_str()?;
        let generation = value["generation"].as_u64()?;
        let sequence = value["sequence"].as_u64()?;
        if session_id.is_empty() || rustdesk_id.is_empty() || !ALLOWED_EVENTS.contains(&event) {
            return None;
        }
        let identity = format!("{session_id}:{generation}:{sequence}:{event}");
        if self.last_delivered.as_deref() == Some(identity.as_str()) {
            return None;
        }
        // The client -> panel direction puts `event` at the root, unlike the
        // `{type, data}` envelope the panel uses in the other direction. That
        // asymmetry is the panel's contract (see src/ws.js there), not an
        // oversight here.
        let mut payload = serde_json::json!({
            "event": event,
            "session_id": session_id,
            "rustdesk_id": rustdesk_id,
        });
        if event == "screen_cam.preview.failed" {
            let reason = value["reason"].as_str().unwrap_or_default();
            payload["reason"] = serde_json::json!(if KNOWN_REASONS.contains(&reason) {
                reason
            } else {
                "unknown"
            });
        }
        Some((identity, payload.to_string()))
    }

    fn commit(&mut self, identity: String) {
        self.last_delivered = Some(identity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_upgrades_scheme_and_keeps_path() {
        assert_eq!(
            build_endpoint("https://panel.example.com", "abc").unwrap(),
            "wss://panel.example.com/api/ws?token=abc"
        );
        assert_eq!(
            build_endpoint("http://10.0.0.1:8899/", "abc").unwrap(),
            "ws://10.0.0.1:8899/api/ws?token=abc"
        );
        assert_eq!(
            build_endpoint("https://example.com/sub", "abc").unwrap(),
            "wss://example.com/sub/api/ws?token=abc"
        );
    }

    #[test]
    fn endpoint_rejects_what_cannot_be_a_panel() {
        assert!(build_endpoint("ftp://example.com", "abc").is_none());
        assert!(build_endpoint("example.com", "abc").is_none());
        assert!(build_endpoint("https://", "abc").is_none());
    }

    #[test]
    fn endpoint_escapes_the_token() {
        // A token is base64url in practice, but the query must survive one
        // that is not rather than silently truncating at an `&`.
        let url = build_endpoint("https://example.com", "a&b=c").unwrap();
        assert!(url.ends_with("/api/ws?token=a%26b%3Dc"), "{url}");
    }

    fn snapshot(event: &str, sequence: u64, extra: serde_json::Value) -> String {
        let mut value = serde_json::json!({
            "event": event,
            "session_id": "pv_abc",
            "rustdesk_id": "485236790",
            "generation": 1,
            "sequence": sequence,
        });
        if let Some(map) = extra.as_object() {
            for (k, v) in map {
                value[k] = v.clone();
            }
        }
        value.to_string()
    }

    #[test]
    fn idle_and_malformed_snapshots_are_not_forwarded() {
        let f = LifecycleForwarder::default();
        assert!(f.prepare(r#"{"state":"idle"}"#).is_none());
        assert!(f.prepare("not json").is_none());
        // Missing the ordering fields: no identity, so no send.
        assert!(f
            .prepare(r#"{"event":"screen_cam.preview.started","session_id":"a","rustdesk_id":"b"}"#)
            .is_none());
    }

    #[test]
    fn unknown_events_are_dropped() {
        let f = LifecycleForwarder::default();
        assert!(f
            .prepare(&snapshot("screen_cam.preview.exploded", 1, serde_json::json!({})))
            .is_none());
    }

    #[test]
    fn the_same_transition_is_sent_once_but_the_next_one_goes() {
        let mut f = LifecycleForwarder::default();
        let first = snapshot("screen_cam.preview.started", 1, serde_json::json!({}));
        let (identity, _) = f.prepare(&first).expect("first send");
        f.commit(identity);
        assert!(f.prepare(&first).is_none(), "already delivered");

        let second = snapshot("screen_cam.preview.stopped", 2, serde_json::json!({}));
        assert!(f.prepare(&second).is_some(), "a new transition still goes");
    }

    #[test]
    fn a_failure_keeps_the_event_pending() {
        // `prepare` alone must not mark anything delivered: only `commit`,
        // which the caller reaches after the write succeeded.
        let f = LifecycleForwarder::default();
        let status = snapshot("screen_cam.preview.started", 1, serde_json::json!({}));
        assert!(f.prepare(&status).is_some());
        assert!(f.prepare(&status).is_some(), "still owed after a failed send");
    }

    #[test]
    fn only_whitelisted_fields_and_reasons_reach_the_panel() {
        let f = LifecycleForwarder::default();
        let status = snapshot(
            "screen_cam.preview.failed",
            1,
            serde_json::json!({
                "reason": "connect_failed",
                "publish_url": "srt://gateway:8890",
                "publish_token": "SECRETO",
            }),
        );
        let (_, payload) = f.prepare(&status).expect("forwarded");
        assert!(!payload.contains("SECRETO"), "{payload}");
        assert!(!payload.contains("publish_url"), "{payload}");
        assert!(payload.contains(r#""reason":"connect_failed""#), "{payload}");

        let unknown = snapshot(
            "screen_cam.preview.failed",
            2,
            serde_json::json!({ "reason": "algo_nuevo" }),
        );
        let (_, payload) = f.prepare(&unknown).expect("forwarded");
        assert!(payload.contains(r#""reason":"unknown""#), "{payload}");
    }

    /// The display fields are read straight off the panel's payload, so the
    /// shape it actually sends is worth pinning down: an absent field means
    /// "unchanged" and must not become a write, while a malformed one must not
    /// reach the applier at all.
    #[test]
    fn policy_update_distinguishes_absent_from_invalid() {
        let selected_of = |data: &serde_json::Value| match data.get("selected_display_id") {
            Some(serde_json::Value::String(id)) if !id.is_empty() && id.starts_with(r"\\.\") => {
                Some(id.clone())
            }
            _ => None,
        };
        let fallback_of =
            |data: &serde_json::Value| data.get("fallback_to_primary").and_then(|v| v.as_bool());

        let full = serde_json::json!({
            "selected_display_id": r"\\.\DISPLAY2",
            "fallback_to_primary": true,
        });
        assert_eq!(selected_of(&full).as_deref(), Some(r"\\.\DISPLAY2"));
        assert_eq!(fallback_of(&full), Some(true));

        // Only the fallback changed: the selection must stay untouched.
        let partial = serde_json::json!({ "fallback_to_primary": false });
        assert_eq!(selected_of(&partial), None);
        assert_eq!(fallback_of(&partial), Some(false));

        // Fields this path does not own must leave both as None, so nothing is
        // written for an event that was about something else entirely.
        let unrelated = serde_json::json!({ "licensed": true, "mode": "managed" });
        assert_eq!(selected_of(&unrelated), None);
        assert_eq!(fallback_of(&unrelated), None);
    }

    #[test]
    fn a_pong_is_liveness_and_a_greeting_too() {
        assert!(handle_frame(br#"{"type":"pong"}"#) == FrameKind::Liveness);
        assert!(handle_frame(br#"{"type":"connected"}"#) == FrameKind::Liveness);
        assert!(handle_frame(br#"{"type":"otra.cosa"}"#) == FrameKind::Other);
        assert!(handle_frame(b"no json") == FrameKind::Other);
    }
}
