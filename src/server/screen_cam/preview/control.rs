use super::publisher::{
    PreviewDestination, PreviewOwnership, PreviewPublishRequest, PreviewPublisherHandle,
    PreviewPublisherSpawner, PreviewStreamId, JOIN_BUDGET,
};
use super::tap::PreviewTap;
use serde_derive::{Deserialize, Serialize};
use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use url::Url;

const MAX_PREVIEW_DURATION_SECS: u32 = 300;

#[derive(Deserialize, Serialize)]
pub(crate) struct PreviewStartRequest {
    pub(crate) session_id: String,
    pub(crate) rustdesk_id: String,
    pub(crate) publish_url: String,
    pub(crate) publish_token: String,
    pub(crate) stream_name: String,
    pub(crate) expires_in: u32,
}

impl PreviewStartRequest {
    pub(crate) fn from_json(value: &str) -> Option<Self> {
        let mut request = serde_json::from_str::<Self>(value).ok()?;
        request.session_id = trimmed_non_empty(&request.session_id)?;
        request.rustdesk_id = trimmed_non_empty(&request.rustdesk_id)?;
        request.publish_url = trimmed_non_empty(&request.publish_url)?;
        if request.publish_token.trim().is_empty() {
            return None;
        }
        request.stream_name = trimmed_non_empty(&request.stream_name)?;
        if !(1..=MAX_PREVIEW_DURATION_SECS).contains(&request.expires_in)
            || !valid_srt_publish_url(&request.publish_url)
        {
            return None;
        }
        Some(request)
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct PreviewStopRequest {
    pub(crate) session_id: String,
    pub(crate) rustdesk_id: String,
}

impl PreviewStopRequest {
    pub(crate) fn from_json(value: &str) -> Option<Self> {
        let mut request = serde_json::from_str::<Self>(value).ok()?;
        request.session_id = trimmed_non_empty(&request.session_id)?;
        request.rustdesk_id = trimmed_non_empty(&request.rustdesk_id)?;
        Some(request)
    }
}

struct PreviewSecret(String);

impl PreviewSecret {
    fn matches(&self, candidate: &str) -> bool {
        self.0 == candidate
    }
}

impl fmt::Debug for PreviewSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

struct PreviewSession {
    session_id: String,
    rustdesk_id: String,
    publish_url: String,
    publish_token: PreviewSecret,
    stream_name: String,
    expires_in: u32,
    expires_at: Instant,
    generation: u64,
}

impl PreviewSession {
    fn from_request(request: PreviewStartRequest, generation: u64) -> Self {
        Self {
            session_id: request.session_id,
            rustdesk_id: request.rustdesk_id,
            publish_url: request.publish_url,
            publish_token: PreviewSecret(request.publish_token),
            stream_name: request.stream_name,
            expires_in: request.expires_in,
            expires_at: Instant::now() + Duration::from_secs(u64::from(request.expires_in)),
            generation,
        }
    }

    fn matches(&self, request: &PreviewStartRequest) -> bool {
        self.session_id == request.session_id
            && self.rustdesk_id == request.rustdesk_id
            && self.publish_url == request.publish_url
            && self.publish_token.matches(&request.publish_token)
            && self.stream_name == request.stream_name
            && self.expires_in == request.expires_in
    }
}

impl fmt::Debug for PreviewSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewSession")
            .field("session_id", &self.session_id)
            .field("rustdesk_id", &self.rustdesk_id)
            .field("publish_url", &"<redacted>")
            .field("publish_token", &self.publish_token)
            .field("stream_name", &self.stream_name)
            .field("expires_in", &self.expires_in)
            .field("expires_at", &self.expires_at)
            .field("generation", &self.generation)
            .finish()
    }
}

#[derive(Debug)]
enum PreviewControlState {
    Inactive,
    /// This control owns the session and the tap is open. `publisher` is `None`
    /// only in the window between claiming the session and the worker being
    /// installed, and permanently when the spawner declines to start one.
    Starting {
        session: PreviewSession,
        publisher: Option<PreviewPublisherHandle>,
    },
}

#[derive(Debug)]
struct PreviewControlInner {
    state: PreviewControlState,
    generation: u64,
}

impl PreviewControlInner {
    /// Removes the worker from whatever session currently holds one, so it can
    /// be stopped **after** the lock is released.
    fn take_publisher(&mut self) -> Option<PreviewPublisherHandle> {
        match &mut self.state {
            PreviewControlState::Starting { publisher, .. } => publisher.take(),
            PreviewControlState::Inactive => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewControlRejection {
    DeviceIdMismatch,
    SessionConflict,
    PreviewUnavailable,
}

impl PreviewControlRejection {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::DeviceIdMismatch => "device id mismatch",
            Self::SessionConflict => "preview session conflict",
            Self::PreviewUnavailable => "preview unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewControlOutcome {
    pub(crate) applied: bool,
    pub(crate) changed: bool,
    pub(crate) session_id: String,
    pub(crate) rejection: Option<PreviewControlRejection>,
}

impl PreviewControlOutcome {
    fn applied(session_id: String, changed: bool) -> Self {
        Self {
            applied: true,
            changed,
            session_id,
            rejection: None,
        }
    }

    fn rejected(session_id: String, rejection: PreviewControlRejection) -> Self {
        Self {
            applied: false,
            changed: false,
            session_id,
            rejection: Some(rejection),
        }
    }

    pub(crate) fn unavailable(session_id: String) -> Self {
        Self::rejected(session_id, PreviewControlRejection::PreviewUnavailable)
    }
}

/// What [`PreviewControl::start`] decided while holding the lock, so the slow
/// part can happen after releasing it.
enum StartDecision {
    /// Same session, same parameters: nothing to do, and above all no restart.
    Idempotent,
    Conflict,
    /// This call claimed the session; the previous worker (if any) still has to
    /// be stopped and a new one started, both outside the lock.
    Launch {
        previous: Option<PreviewPublisherHandle>,
        generation: u64,
        expires_at: Instant,
    },
}

pub(crate) struct PreviewControl {
    tap: Arc<PreviewTap>,
    inner: Mutex<PreviewControlInner>,
    spawner: Arc<dyn PreviewPublisherSpawner>,
    /// Mirror of `inner.generation`, readable without the lock. A worker checks
    /// it to answer "am I still the current session?", which is what lets this
    /// control wait for a worker while holding nothing — see the module note in
    /// `publisher`.
    current_generation: Arc<AtomicU64>,
}

impl PreviewControl {
    pub(crate) fn new(tap: Arc<PreviewTap>, spawner: Arc<dyn PreviewPublisherSpawner>) -> Self {
        Self {
            tap,
            inner: Mutex::new(PreviewControlInner {
                state: PreviewControlState::Inactive,
                generation: 0,
            }),
            spawner,
            current_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Starts, replaces or no-ops a preview session.
    ///
    /// # Locking
    ///
    /// The mutex is held for three short, allocation-only stretches and **never**
    /// across a join, a socket close, a timeout or any network work. Stopping
    /// the previous worker and spawning the new one both happen with the lock
    /// released; the third stretch re-checks the generation before installing
    /// the new worker, so a START that raced ahead of this one wins and this
    /// one's worker is stopped instead of overwriting it.
    pub(crate) fn start(
        &self,
        request: PreviewStartRequest,
        local_rustdesk_id: &str,
    ) -> PreviewControlOutcome {
        let session_id = request.session_id.clone();
        if local_rustdesk_id.trim() != request.rustdesk_id.trim() {
            return PreviewControlOutcome::rejected(
                session_id,
                PreviewControlRejection::DeviceIdMismatch,
            );
        }

        // Built before the lock and before the token disappears into
        // `PreviewSecret`: both are pure, both can fail, and neither should
        // happen with the mutex held. A rejection here reuses
        // `PreviewUnavailable` rather than widening the enum, so the ACK the
        // panel already understands does not change shape.
        let Ok(stream_id) = PreviewStreamId::build(&request.stream_name, &request.publish_token)
        else {
            return PreviewControlOutcome::rejected(
                session_id,
                PreviewControlRejection::PreviewUnavailable,
            );
        };
        let Ok(destination) = PreviewDestination::from_publish_url(&request.publish_url) else {
            return PreviewControlOutcome::rejected(
                session_id,
                PreviewControlRejection::PreviewUnavailable,
            );
        };

        // --- locked stretch 1: decide and claim ---
        let decision = {
            let mut inner = self.inner.lock().unwrap();
            match &inner.state {
                PreviewControlState::Starting { session, .. }
                    if session.session_id == request.session_id =>
                {
                    if session.matches(&request) {
                        StartDecision::Idempotent
                    } else {
                        StartDecision::Conflict
                    }
                }
                _ => {
                    let Some(generation) = inner.generation.checked_add(1) else {
                        return PreviewControlOutcome::rejected(
                            session_id,
                            PreviewControlRejection::PreviewUnavailable,
                        );
                    };
                    // Taken here, stopped later: holding the lock while joining
                    // is exactly the deadlock this split exists to avoid.
                    let previous = inner.take_publisher();
                    // A replacement must never inherit frames from the previous
                    // session. Both operations are bounded queue/atomic work and
                    // perform no I/O.
                    self.tap.deactivate();
                    self.tap.activate();
                    inner.generation = generation;
                    // Published before the old worker is even told to stop, so
                    // from this instant it can no longer claim to be current.
                    self.current_generation.store(generation, Ordering::Release);
                    let session = PreviewSession::from_request(request, generation);
                    let expires_at = session.expires_at;
                    inner.state = PreviewControlState::Starting {
                        session,
                        publisher: None,
                    };
                    StartDecision::Launch {
                        previous,
                        generation,
                        expires_at,
                    }
                }
            }
        };

        match decision {
            StartDecision::Idempotent => PreviewControlOutcome::applied(session_id, false),
            StartDecision::Conflict => PreviewControlOutcome::rejected(
                session_id,
                PreviewControlRejection::SessionConflict,
            ),
            StartDecision::Launch {
                previous,
                generation,
                expires_at,
            } => {
                // --- unlocked: the only part that can wait ---
                if let Some(previous) = previous {
                    previous.stop_and_join(JOIN_BUDGET);
                }
                let publisher = self.spawner.spawn(PreviewPublishRequest {
                    session_id: session_id.clone(),
                    // Already checked against the local device id at the top of
                    // this function, so the worker reports under an identity the
                    // panel can trust.
                    rustdesk_id: local_rustdesk_id.trim().to_owned(),
                    generation,
                    destination,
                    stream_id,
                    expires_at,
                    tap: Arc::clone(&self.tap),
                    ownership: PreviewOwnership::new(
                        Arc::clone(&self.current_generation),
                        generation,
                    ),
                });

                // --- locked stretch 2: install, unless we were overtaken ---
                let stale = {
                    let mut inner = self.inner.lock().unwrap();
                    match &mut inner.state {
                        PreviewControlState::Starting { session, publisher: slot }
                            if session.generation == generation =>
                        {
                            *slot = publisher;
                            None
                        }
                        // A newer START (or a STOP) already moved on. Its own
                        // call sequence owns the tap now; this worker must go.
                        _ => publisher,
                    }
                };
                if let Some(stale) = stale {
                    stale.stop_and_join(JOIN_BUDGET);
                }
                PreviewControlOutcome::applied(session_id, true)
            }
        }
    }

    /// Stops the current session. Idempotent for anything else, including a
    /// STOP for a session that a newer START already replaced.
    ///
    /// # Locking
    ///
    /// Same rule as [`Self::start`]: the tap is closed and the state cleared
    /// under the lock — both non-blocking — and the worker is signalled and
    /// joined only after the lock is released.
    pub(crate) fn stop(
        &self,
        request: PreviewStopRequest,
        local_rustdesk_id: &str,
    ) -> PreviewControlOutcome {
        let session_id = request.session_id;
        if local_rustdesk_id.trim() != request.rustdesk_id.trim() {
            return PreviewControlOutcome::rejected(
                session_id,
                PreviewControlRejection::DeviceIdMismatch,
            );
        }

        // --- locked stretch: close the session ---
        let publisher = {
            let mut inner = self.inner.lock().unwrap();
            let is_current = matches!(
                &inner.state,
                PreviewControlState::Starting { session, .. }
                    if session.session_id == session_id
                        && session.rustdesk_id == request.rustdesk_id
            );
            if !is_current {
                return PreviewControlOutcome::applied(session_id, false);
            }
            let publisher = inner.take_publisher();
            self.tap.deactivate();
            inner.state = PreviewControlState::Inactive;
            // Retiring the generation keeps a worker that is still unwinding
            // from closing a tap that a later START may already have reopened.
            if let Some(generation) = inner.generation.checked_add(1) {
                inner.generation = generation;
                self.current_generation.store(generation, Ordering::Release);
            }
            publisher
        };

        // --- unlocked: signal and wait, bounded ---
        if let Some(publisher) = publisher {
            publisher.stop_and_join(JOIN_BUDGET);
        }
        PreviewControlOutcome::applied(session_id, true)
    }
}

impl fmt::Debug for PreviewControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.lock().unwrap();
        f.debug_struct("PreviewControl")
            .field("tap", &self.tap)
            .field("inner", &*inner)
            .finish()
    }
}

fn trimmed_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn valid_srt_publish_url(value: &str) -> bool {
    if !value.starts_with("srt://") {
        return false;
    }
    let authority = value["srt://".len()..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    url.scheme() == "srt"
        && url.host_str().map(|host| !host.is_empty()).unwrap_or(false)
        && url.port().map(|port| port > 0).unwrap_or(false)
        && !authority.contains('@')
        && url.fragment().is_none()
        && !url
            .query_pairs()
            .any(|(key, _)| key.eq_ignore_ascii_case("streamid"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::screen_cam::preview::publisher::{
        testing::{FakePublisherSpawner, FakeStep},
        NoopPublisherSpawner, PreviewPublisherEvent, PreviewStopCause, TransportError,
        MAX_STREAM_ID_BYTES,
    };
    use crate::server::screen_cam::preview::tap::{EncodedAccessUnit, TapPushResult};

    const LOCAL_ID: &str = "485236790";
    const TOKEN: &str = "test-token-redacted";

    fn start_request(session_id: &str) -> PreviewStartRequest {
        PreviewStartRequest {
            session_id: session_id.to_owned(),
            rustdesk_id: LOCAL_ID.to_owned(),
            publish_url: "srt://sehcontrol.sehuacho.com:8890".to_owned(),
            publish_token: TOKEN.to_owned(),
            stream_name: session_id.to_owned(),
            expires_in: 300,
        }
    }

    fn stop_request(session_id: &str) -> PreviewStopRequest {
        PreviewStopRequest {
            session_id: session_id.to_owned(),
            rustdesk_id: LOCAL_ID.to_owned(),
        }
    }

    fn unit(marker: u8) -> EncodedAccessUnit {
        EncodedAccessUnit::from_vec(1, 0, true, true, true, vec![0, 0, 0, 1, 0x65, marker])
            .expect("valid fixture")
    }

    fn session_snapshot(control: &PreviewControl) -> Option<(String, u64, Instant)> {
        let inner = control.inner.lock().unwrap();
        match &inner.state {
            PreviewControlState::Inactive => None,
            PreviewControlState::Starting { session, .. } => Some((
                session.session_id.clone(),
                session.generation,
                session.expires_at,
            )),
        }
    }

    #[test]
    fn preview_control_initial_state_is_inactive_and_uses_the_same_tap() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        assert!(session_snapshot(&control).is_none());
        assert!(!tap.is_active());
        assert!(Arc::ptr_eq(&tap, &control.tap));
    }

    #[test]
    fn preview_control_first_start_activates_tap_and_stores_session() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        let outcome = control.start(start_request("pv_first"), LOCAL_ID);
        assert!(outcome.applied);
        assert!(outcome.changed);
        assert!(tap.is_active());
        let (session_id, generation, _) = session_snapshot(&control).expect("active session");
        assert_eq!(session_id, "pv_first");
        assert_eq!(generation, 1);
        assert_eq!(tap.stats().queued, 0);
    }

    #[test]
    fn preview_control_first_start_clears_unexpected_residue() {
        let tap = Arc::new(PreviewTap::new());
        tap.activate();
        assert_eq!(tap.push(unit(1)), TapPushResult::Queued);
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        assert!(control.start(start_request("pv_first"), LOCAL_ID).changed);
        assert_eq!(tap.stats().queued, 0);
    }

    #[test]
    fn preview_control_duplicate_is_idempotent_without_draining() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        assert!(control.start(start_request("pv_same"), LOCAL_ID).changed);
        assert_eq!(tap.push(unit(1)), TapPushResult::Queued);

        let outcome = control.start(start_request("pv_same"), LOCAL_ID);
        assert!(outcome.applied);
        assert!(!outcome.changed);
        assert_eq!(session_snapshot(&control).expect("session").1, 1);
        assert_eq!(tap.stats().queued, 1);
    }

    #[test]
    fn preview_control_rejects_same_session_with_different_secret_or_url() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        assert!(control.start(start_request("pv_same"), LOCAL_ID).changed);

        let mut different_secret = start_request("pv_same");
        different_secret.publish_token = "different-secret".to_owned();
        let secret_outcome = control.start(different_secret, LOCAL_ID);
        assert_eq!(
            secret_outcome.rejection,
            Some(PreviewControlRejection::SessionConflict)
        );

        let mut different_url = start_request("pv_same");
        different_url.publish_url = "srt://preview.example.invalid:9000".to_owned();
        let url_outcome = control.start(different_url, LOCAL_ID);
        assert_eq!(
            url_outcome.rejection,
            Some(PreviewControlRejection::SessionConflict)
        );
        assert!(tap.is_active());
        assert_eq!(session_snapshot(&control).expect("session").1, 1);
    }

    #[test]
    fn preview_control_new_start_replaces_session_and_clears_frames() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        assert!(control.start(start_request("pv_old"), LOCAL_ID).changed);
        assert_eq!(tap.push(unit(1)), TapPushResult::Queued);

        let outcome = control.start(start_request("pv_new"), LOCAL_ID);
        assert!(outcome.applied);
        assert!(outcome.changed);
        assert!(tap.is_active());
        assert_eq!(tap.stats().queued, 0);
        let (session_id, generation, _) = session_snapshot(&control).expect("session");
        assert_eq!(session_id, "pv_new");
        assert_eq!(generation, 2);
    }

    #[test]
    fn preview_control_current_stop_deactivates_clears_and_removes_session() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        control.start(start_request("pv_stop"), LOCAL_ID);
        assert_eq!(tap.push(unit(1)), TapPushResult::Queued);

        let outcome = control.stop(stop_request("pv_stop"), LOCAL_ID);
        assert!(outcome.applied);
        assert!(outcome.changed);
        assert!(!tap.is_active());
        assert_eq!(tap.stats().queued, 0);
        assert!(session_snapshot(&control).is_none());
    }

    #[test]
    fn preview_control_stop_without_session_is_idempotent() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        let outcome = control.stop(stop_request("pv_absent"), LOCAL_ID);
        assert!(outcome.applied);
        assert!(!outcome.changed);
        assert!(!tap.is_active());
    }

    #[test]
    fn preview_control_old_stop_does_not_stop_new_session() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        control.start(start_request("pv_old"), LOCAL_ID);
        control.start(start_request("pv_new"), LOCAL_ID);

        let outcome = control.stop(stop_request("pv_old"), LOCAL_ID);
        assert!(outcome.applied);
        assert!(!outcome.changed);
        assert!(tap.is_active());
        assert_eq!(session_snapshot(&control).expect("session").0, "pv_new");
    }

    #[test]
    fn preview_control_wrong_device_never_changes_tap() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        let outcome = control.start(start_request("pv_wrong"), "different-id");
        assert_eq!(
            outcome.rejection,
            Some(PreviewControlRejection::DeviceIdMismatch)
        );
        assert!(!tap.is_active());
        assert!(session_snapshot(&control).is_none());
    }

    #[test]
    fn preview_control_expiration_is_calculated_with_tolerance() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(tap, Arc::new(NoopPublisherSpawner));
        let before = Instant::now() + Duration::from_secs(299);
        control.start(start_request("pv_expiry"), LOCAL_ID);
        let expires_at = session_snapshot(&control).expect("session").2;
        let after = Instant::now() + Duration::from_secs(301);
        assert!(expires_at >= before);
        assert!(expires_at <= after);
    }

    #[test]
    fn preview_control_debug_redacts_secret_and_url() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(tap, Arc::new(NoopPublisherSpawner));
        control.start(start_request("pv_debug"), LOCAL_ID);
        let diagnostic = format!("{control:?}");
        assert!(!diagnostic.contains(TOKEN));
        assert!(!diagnostic.contains("sehcontrol.sehuacho.com"));
        assert!(diagnostic.contains("<redacted>"));
    }

    #[test]
    fn preview_control_invalidation_preserves_session() {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), Arc::new(NoopPublisherSpawner));
        control.start(start_request("pv_invalidation"), LOCAL_ID);
        tap.invalidate_stream(9);
        assert_eq!(
            session_snapshot(&control).expect("session").0,
            "pv_invalidation"
        );
        assert!(tap.is_active());
    }

    #[test]
    fn preview_requests_validate_types_ranges_and_urls() {
        assert!(PreviewStartRequest::from_json(
            r#"{"session_id":"pv","rustdesk_id":"485236790","publish_url":"srt://host:8890","publish_token":"secret","stream_name":"pv","expires_in":300}"#
        )
        .is_some());
        for invalid in [
            "{}",
            r#"{"session_id":1,"rustdesk_id":"485236790","publish_url":"srt://host:8890","publish_token":"secret","stream_name":"pv","expires_in":300}"#,
            r#"{"session_id":"pv","rustdesk_id":"485236790","publish_url":"https://host:8890","publish_token":"secret","stream_name":"pv","expires_in":300}"#,
            r#"{"session_id":"pv","rustdesk_id":"485236790","publish_url":"srt://host","publish_token":"secret","stream_name":"pv","expires_in":300}"#,
            r#"{"session_id":"pv","rustdesk_id":"485236790","publish_url":"srt://user@host:8890","publish_token":"secret","stream_name":"pv","expires_in":300}"#,
            r#"{"session_id":"pv","rustdesk_id":"485236790","publish_url":"srt://host:8890#fragment","publish_token":"secret","stream_name":"pv","expires_in":300}"#,
            r#"{"session_id":"pv","rustdesk_id":"485236790","publish_url":"srt://host:8890?streamid=existing","publish_token":"secret","stream_name":"pv","expires_in":300}"#,
            r#"{"session_id":"pv","rustdesk_id":"485236790","publish_url":"srt://host:8890","publish_token":"secret","stream_name":"pv","expires_in":0}"#,
            r#"{"session_id":"pv","rustdesk_id":"485236790","publish_url":"srt://host:8890","publish_token":"secret","stream_name":"pv","expires_in":301}"#,
        ] {
            assert!(PreviewStartRequest::from_json(invalid).is_none());
        }
        assert!(
            PreviewStopRequest::from_json(r#"{"session_id":"pv","rustdesk_id":"485236790"}"#)
                .is_some()
        );
        assert!(
            PreviewStopRequest::from_json(r#"{"session_id":1,"rustdesk_id":"485236790"}"#)
                .is_none()
        );
    }

    /// A control wired to a spawner that runs the real publisher state machine
    /// over a fake transport, which is the only way to assert things like "this
    /// START did not restart the worker".
    fn control_with_publisher(spawner: Arc<FakePublisherSpawner>) -> (Arc<PreviewTap>, PreviewControl) {
        let tap = Arc::new(PreviewTap::new());
        let control = PreviewControl::new(Arc::clone(&tap), spawner);
        (tap, control)
    }

    fn wait_until(budget: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        condition()
    }

    // 16
    #[test]
    fn an_identical_start_does_not_restart_the_publisher() {
        let spawner = Arc::new(FakePublisherSpawner::new());
        let (tap, control) = control_with_publisher(Arc::clone(&spawner));

        let first = control.start(start_request("pv_1"), LOCAL_ID);
        assert!(first.applied && first.changed);
        assert!(wait_until(Duration::from_secs(5), || spawner
            .shared()
            .snapshot(|state| !state.log.connects.is_empty())));
        assert_eq!(spawner.spawned(), 1);

        // Byte-identical START: applied, but nothing may move — no second
        // worker, no second handshake, and the tap keeps its frames.
        tap.push(unit(1));
        let repeat = control.start(start_request("pv_1"), LOCAL_ID);
        assert!(repeat.applied);
        assert!(!repeat.changed);
        assert_eq!(spawner.spawned(), 1, "the publisher must not be restarted");
        assert_eq!(
            spawner.shared().snapshot(|state| state.log.connects.len()),
            1,
            "no second connection was opened"
        );

        control.stop(stop_request("pv_1"), LOCAL_ID);
    }

    // 17
    #[test]
    fn a_conflicting_start_leaves_the_active_publisher_alone() {
        let spawner = Arc::new(FakePublisherSpawner::new());
        let (tap, control) = control_with_publisher(Arc::clone(&spawner));
        assert!(control.start(start_request("pv_1"), LOCAL_ID).applied);
        assert!(wait_until(Duration::from_secs(5), || spawner
            .shared()
            .snapshot(|state| !state.log.connects.is_empty())));

        // Same session id, different parameters.
        let mut conflicting = start_request("pv_1");
        conflicting.publish_token = "a-different-secret".to_owned();
        let outcome = control.start(conflicting, LOCAL_ID);

        assert!(!outcome.applied);
        assert_eq!(
            outcome.rejection,
            Some(PreviewControlRejection::SessionConflict)
        );
        assert_eq!(spawner.spawned(), 1, "no worker was started for the conflict");
        assert_eq!(spawner.shared().snapshot(|state| state.log.closes), 0);
        assert!(tap.is_active(), "the live session keeps its tap");

        control.stop(stop_request("pv_1"), LOCAL_ID);
    }

    // 18
    #[test]
    fn a_replaced_worker_cannot_close_the_tap_of_the_session_that_replaced_it() {
        // The old worker is wedged inside its handshake, so it is still running
        // when the replacement takes over and can only unwind afterwards.
        let spawner = Arc::new(FakePublisherSpawner::new());
        let (tap, control) = control_with_publisher(Arc::clone(&spawner));
        assert!(control.start(start_request("pv_old"), LOCAL_ID).applied);
        assert!(wait_until(Duration::from_secs(5), || spawner
            .shared()
            .snapshot(|state| !state.log.connects.is_empty())));

        let replaced = control.start(start_request("pv_new"), LOCAL_ID);
        assert!(replaced.applied && replaced.changed);
        assert_eq!(session_snapshot(&control).expect("session").0, "pv_new");
        assert_eq!(spawner.spawned(), 2);

        // Whatever the old worker does as it finishes, the new session's tap has
        // to stay open: its ownership check fails, so it leaves it alone.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            tap.is_active(),
            "an outgoing worker must not close the incoming session's tap"
        );
        let (_, generation, _) = session_snapshot(&control).expect("session");
        assert_eq!(generation, 2);

        control.stop(stop_request("pv_new"), LOCAL_ID);
        assert!(!tap.is_active());
    }

    // 15
    #[test]
    fn a_hundred_start_stop_cycles_finish_without_deadlocking() {
        // Sends fail so every cycle also exercises the reconnect path, which is
        // where a lock held across a wait would show up as a hang.
        let spawner = Arc::new(
            FakePublisherSpawner::new().with_send_plan(vec![FakeStep::Fail(
                TransportError::SendFailed,
            )]),
        );
        let (tap, control) = control_with_publisher(Arc::clone(&spawner));

        let started = Instant::now();
        for round in 0..100 {
            let session = format!("pv_{round}");
            let started_outcome = control.start(start_request(&session), LOCAL_ID);
            assert!(started_outcome.applied, "round {round} failed to start");
            // Feed a publishable frame so the worker gets past the keyframe gate
            // and into the failing send.
            tap.push(unit(round as u8));
            let stopped = control.stop(stop_request(&session), LOCAL_ID);
            assert!(stopped.applied && stopped.changed, "round {round} failed to stop");
            assert!(!tap.is_active(), "round {round} left the tap open");
        }
        // Not a performance assertion: each cycle joins a worker, and the join
        // budget alone is 3 s, so anything remotely close to that means a cycle
        // was blocking rather than finishing.
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "100 cycles took {:?}",
            started.elapsed()
        );
        assert_eq!(spawner.spawned(), 100);
    }

    #[test]
    fn a_start_whose_stream_id_cannot_fit_is_rejected_without_touching_the_tap() {
        let spawner = Arc::new(FakePublisherSpawner::new());
        let (tap, control) = control_with_publisher(Arc::clone(&spawner));

        let mut request = start_request("pv_1");
        request.publish_token = "t".repeat(MAX_STREAM_ID_BYTES);
        let outcome = control.start(request, LOCAL_ID);

        assert!(!outcome.applied);
        assert_eq!(
            outcome.rejection,
            Some(PreviewControlRejection::PreviewUnavailable)
        );
        assert!(!tap.is_active(), "a rejected START must not open the tap");
        assert_eq!(spawner.spawned(), 0);
    }

    #[test]
    fn stopping_a_session_reports_the_expected_publisher_events() {
        let spawner = Arc::new(FakePublisherSpawner::new());
        let (tap, control) = control_with_publisher(Arc::clone(&spawner));
        assert!(control.start(start_request("pv_1"), LOCAL_ID).applied);
        assert!(wait_until(Duration::from_secs(5), || spawner
            .sink()
            .events()
            .iter()
            .any(|event| matches!(event, PreviewPublisherEvent::Started { .. }))));
        tap.push(unit(1));

        assert!(control.stop(stop_request("pv_1"), LOCAL_ID).changed);

        let events = spawner.sink().events();
        assert!(events.iter().any(|event| matches!(
            event,
            PreviewPublisherEvent::Connecting { generation: 1, attempt: 0 }
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, PreviewPublisherEvent::Started { generation: 1 })));
        assert!(
            events.iter().any(|event| matches!(
                event,
                PreviewPublisherEvent::Stopped {
                    generation: 1,
                    cause: PreviewStopCause::Stopped | PreviewStopCause::TapClosed,
                }
            )),
            "{events:?}"
        );
    }
}
