// The preview publisher: what finally joins the three pieces next door.
//
// It drains [`PreviewTap`], muxes each access unit with [`MpegTsMuxer`] and
// pushes the transport stream to MediaMTX over SRT in caller mode. Nothing in
// here touches the capture pipeline: the tap is a parallel outlet, and every
// failure mode below ends in "this publisher stops", never in "capture stops".
// RTSP and UDP consumers cannot observe any of it.
//
// Three properties are structural rather than best-effort:
//
// - **The capture thread is never blocked.** The publisher only ever calls
//   `pop`, `is_active`, `register_waker` and the snapshot readers, all of which
//   are non-blocking. The socket lives on this side of the tap.
// - **The token never leaves.** The stream id embeds `publish_token`, so it is
//   wrapped in [`PreviewStreamId`], whose `Debug` is redacted and which has no
//   `Display`. Transport errors are mapped to a closed enum *here*, discarding
//   the library's message, because an `io::Error` from a connect attempt can
//   quote the address and the stream id.
// - **Cancellation is prompt.** Connect, send and backoff all run inside a
//   `select!` against the stop token and the session's expiry, so a STOP never
//   waits for a network timeout to elapse.
//
// Ownership of the session lifecycle stays with `PreviewControl`. This module
// deliberately holds no lock that `PreviewControl` also takes: the only shared
// state is the tap and an `AtomicU64` generation, so a worker that outlives its
// session can never mutate a newer one's state, and `PreviewControl` can wait
// for a worker to finish without holding its own mutex.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hbb_common::futures::future::poll_fn;
use hbb_common::log;
use hbb_common::tokio::{self, sync::Notify};

use super::mpegts::{MpegTsError, MpegTsMuxer};
use super::tap::{EncodedAccessUnit, PreviewTap};

/// How long a single SRT handshake may take before it is called a timeout. The
/// panel's START already carries a short `expires_in`, so a caller that cannot
/// reach MediaMTX is more useful failing fast and retrying than hanging.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Backoff between reconnection attempts. Its length is also the attempt cap:
/// five tries, then the session is given up.
const RECONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

/// Budget for closing the socket on the way out. The spike measured a clean
/// close in microseconds; this only exists so a wedged socket cannot keep the
/// worker thread alive.
const CLOSE_BUDGET: Duration = Duration::from_secs(2);

/// Budget `PreviewControl` gives a worker to finish after being told to stop.
/// Deliberately short: it is spent outside the control mutex, but it is still
/// on the IPC thread's path.
pub(crate) const JOIN_BUDGET: Duration = Duration::from_secs(3);

/// The publisher's clock, injectable so the state machine can be exercised in
/// milliseconds instead of the ~15 s the production backoff ladder would take.
/// Only the numbers change; every path through the worker is the same one
/// production runs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PreviewTimings {
    pub(crate) connect_timeout: Duration,
    /// One entry per retry, in order. Its length is the attempt cap.
    pub(crate) backoff: &'static [Duration],
    pub(crate) close_budget: Duration,
}

impl PreviewTimings {
    pub(crate) const PRODUCTION: Self = Self {
        connect_timeout: CONNECT_TIMEOUT,
        backoff: &RECONNECT_BACKOFF,
        close_budget: CLOSE_BUDGET,
    };
}

/// SRT's stream id field is capped at 512 bytes by the protocol, and
/// `srt-protocol` rejects anything longer at connect time. Checked before the
/// socket is opened so an over-long token becomes a categorized rejection
/// rather than a library error carrying the id.
pub(crate) const MAX_STREAM_ID_BYTES: usize = 512;

/// Why a publisher gave up. Closed on purpose: a free-form `String` is exactly
/// how a token ends up in a log, an event or a panic message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewFailureReason {
    /// `publish_url` had no usable host/port pair.
    InvalidDestination,
    /// `publish:<stream>:token=<token>` exceeded the protocol's 512 bytes.
    StreamIdTooLong,
    ConnectTimeout,
    ConnectFailed,
    /// The peer went away after a successful handshake.
    TransportLost,
    SendFailed,
    /// The muxer refused an access unit in a way that is not recoverable by
    /// waiting for the next keyframe.
    MuxFailed,
    /// Every reconnection attempt was used up.
    RetriesExhausted,
}

impl PreviewFailureReason {
    /// Safe to log: every variant is a fixed string with no session data.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvalidDestination => "invalid destination",
            Self::StreamIdTooLong => "stream id too long",
            Self::ConnectTimeout => "connect timeout",
            Self::ConnectFailed => "connect failed",
            Self::TransportLost => "transport lost",
            Self::SendFailed => "send failed",
            Self::MuxFailed => "mux failed",
            Self::RetriesExhausted => "retries exhausted",
        }
    }
}

impl PreviewFailureReason {
    /// Stable code for the panel. Deliberately coarse: the exact transport
    /// error is a diagnostic for our logs, not something an operator's browser
    /// needs, and a narrower vocabulary is one less way to leak detail.
    pub(crate) fn safe_code(self) -> &'static str {
        match self {
            Self::InvalidDestination
            | Self::StreamIdTooLong
            | Self::ConnectTimeout
            | Self::ConnectFailed => "connect_failed",
            Self::TransportLost | Self::SendFailed => "send_failed",
            Self::MuxFailed => "mux_failed",
            Self::RetriesExhausted => "retries_exhausted",
        }
    }
}

impl PreviewStopCause {
    pub(crate) fn safe_code(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Expired => "expired",
            Self::TapClosed => "tap_closed",
        }
    }
}

impl fmt::Display for PreviewFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a publisher stopped without failing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewStopCause {
    /// STOP arrived, or a newer session replaced this one.
    Stopped,
    /// `expires_at` was reached without a STOP.
    Expired,
    /// The tap was closed underneath the publisher.
    TapClosed,
}

/// What a publisher reports about itself. Carries `generation` so a consumer
/// can tell an old worker's dying words from a live one's — and carries no URL,
/// no stream id and no token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewPublisherEvent {
    Connecting { generation: u64, attempt: u32 },
    Started { generation: u64 },
    Failed { generation: u64, reason: PreviewFailureReason },
    Stopped { generation: u64, cause: PreviewStopCause },
}

/// Where events go.
pub(crate) trait PreviewEventSink: Send + Sync + 'static {
    fn emit(&self, event: PreviewPublisherEvent);
}

/// The publisher's state as the panel is allowed to see it.
///
/// Everything here is either an identifier the panel already sent us
/// (`session_id`, `rustdesk_id`) or a fixed code. There is deliberately no
/// field that could hold `publish_url`, `publish_token`, `stream_name` or the
/// SRT stream id: the type is the guarantee, not a call-site convention.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewLifecycleSnapshot {
    /// Monotonic across the process. Lets the UI notice a change without
    /// comparing every field, and lets a consumer drop what it already sent.
    pub(crate) sequence: u64,
    pub(crate) generation: u64,
    pub(crate) session_id: String,
    pub(crate) rustdesk_id: String,
    /// `connecting` | `started` | `failed` | `stopped`.
    pub(crate) state: &'static str,
    /// Only meaningful for `connecting`.
    pub(crate) attempt: Option<u32>,
    /// Fixed code, only for `failed`.
    pub(crate) reason: Option<&'static str>,
    /// Fixed code, only for `stopped`.
    pub(crate) cause: Option<&'static str>,
}

impl PreviewLifecycleSnapshot {
    /// The wire shape the UI process forwards. `state` is expanded to the full
    /// event name the panel's contract uses.
    pub(crate) fn to_json(&self) -> String {
        let mut value = serde_json::json!({
            "sequence": self.sequence,
            "generation": self.generation,
            "session_id": self.session_id,
            "rustdesk_id": self.rustdesk_id,
            "event": format!("screen_cam.preview.{}", self.state),
        });
        if let Some(attempt) = self.attempt {
            value["attempt"] = serde_json::json!(attempt);
        }
        if let Some(reason) = self.reason {
            value["reason"] = serde_json::json!(reason);
        }
        if let Some(cause) = self.cause {
            value["cause"] = serde_json::json!(cause);
        }
        value.to_string()
    }
}

/// The last snapshot, readable by the IPC handler.
///
/// A process-wide `Mutex<Option<..>>` rather than a field of `SharedState`: the
/// capture state is rebuilt by the watchdog on a display change, and a preview
/// session outlives that. Reading it is not destructive, so the UI can re-send
/// the current state after a WebSocket reconnect.
static PREVIEW_LIFECYCLE: Mutex<Option<PreviewLifecycleSnapshot>> = Mutex::new(None);
static PREVIEW_LIFECYCLE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// `None` when no preview session has reported anything yet.
pub(crate) fn lifecycle_snapshot() -> Option<PreviewLifecycleSnapshot> {
    PREVIEW_LIFECYCLE.lock().unwrap().clone()
}

#[cfg(test)]
pub(crate) fn reset_lifecycle_for_test() {
    *PREVIEW_LIFECYCLE.lock().unwrap() = None;
    PREVIEW_LIFECYCLE_SEQUENCE.store(0, Ordering::SeqCst);
}

/// Records the worker's transitions into the shared snapshot.
///
/// The session identity lives here rather than in the events because a
/// `PreviewPublisherEvent` deliberately carries no session data — the sink is
/// built per worker and already knows which session it speaks for.
pub(crate) struct PreviewLifecycleSink {
    session_id: String,
    rustdesk_id: String,
    generation: u64,
    ownership: PreviewOwnership,
}

impl PreviewLifecycleSink {
    pub(crate) fn new(
        session_id: String,
        rustdesk_id: String,
        generation: u64,
        ownership: PreviewOwnership,
    ) -> Self {
        Self {
            session_id,
            rustdesk_id,
            generation,
            ownership,
        }
    }
}

impl PreviewEventSink for PreviewLifecycleSink {
    fn emit(&self, event: PreviewPublisherEvent) {
        // A worker that has been replaced must not repaint the panel for the
        // session that replaced it. Checked without any control lock — see the
        // module note — so this can never deadlock against a STOP that is
        // waiting for this very worker to finish.
        if !self.ownership.is_current() {
            return;
        }
        let (state, attempt, reason, cause) = match event {
            PreviewPublisherEvent::Connecting { attempt, .. } => {
                ("connecting", Some(attempt), None, None)
            }
            PreviewPublisherEvent::Started { .. } => ("started", None, None, None),
            PreviewPublisherEvent::Failed { reason, .. } => {
                ("failed", None, Some(reason.safe_code()), None)
            }
            PreviewPublisherEvent::Stopped { cause, .. } => {
                ("stopped", None, None, Some(cause.safe_code()))
            }
        };
        let sequence = PREVIEW_LIFECYCLE_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
        let mut slot = PREVIEW_LIFECYCLE.lock().unwrap();
        // Second guard, this time against a snapshot a newer generation already
        // published: `is_current` can pass and then lose the race for the lock.
        if slot
            .as_ref()
            .is_some_and(|current| current.generation > self.generation)
        {
            return;
        }
        *slot = Some(PreviewLifecycleSnapshot {
            sequence,
            generation: self.generation,
            session_id: self.session_id.clone(),
            rustdesk_id: self.rustdesk_id.clone(),
            state,
            attempt,
            reason,
            cause,
        });
    }
}

/// The SRT stream id, which embeds `publish_token`.
///
/// Not `Display`, and `Debug` prints nothing but the length, so no format
/// string anywhere can leak it. [`Self::as_str`] is the single way out and only
/// the transport calls it.
pub(crate) struct PreviewStreamId(String);

impl PreviewStreamId {
    /// Builds `publish:<stream_name>:token=<publish_token>` — the exact shape
    /// MediaMTX is configured to parse. It is *not* appended to the URL as a
    /// query parameter; it travels in the SRT handshake.
    pub(crate) fn build(stream_name: &str, publish_token: &str) -> Result<Self, PreviewFailureReason> {
        let value = format!("publish:{stream_name}:token={publish_token}");
        if value.len() > MAX_STREAM_ID_BYTES {
            return Err(PreviewFailureReason::StreamIdTooLong);
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PreviewStreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewStreamId")
            .field("value", &"<redacted>")
            .field("len", &self.0.len())
            .finish()
    }
}

/// `host:port` extracted from `publish_url`. Safe to log as-is: the URL's query
/// — which is where a credential would hide — is dropped here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewDestination(String);

impl PreviewDestination {
    /// `PreviewStartRequest` has already established that the URL is
    /// `srt://host:port` with no userinfo, no fragment and no `streamid`
    /// parameter, so this only has to pull the authority back out.
    pub(crate) fn from_publish_url(publish_url: &str) -> Result<Self, PreviewFailureReason> {
        let url = url::Url::parse(publish_url).map_err(|_| PreviewFailureReason::InvalidDestination)?;
        let host = url.host_str().ok_or(PreviewFailureReason::InvalidDestination)?;
        let port = url.port().ok_or(PreviewFailureReason::InvalidDestination)?;
        if host.is_empty() || port == 0 {
            return Err(PreviewFailureReason::InvalidDestination);
        }
        Ok(Self(format!("{host}:{port}")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Categorized transport failure. The library's own message is never carried:
/// an SRT connect error can quote both the destination and the stream id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransportError {
    ConnectTimeout,
    ConnectFailed,
    SendFailed,
    Lost,
}

/// Why a handshake did not complete, in enough detail to act on.
///
/// This exists because "connect failed" is indistinguishable between a server
/// that refuses our protocol version, a token the server rejects, and a host
/// that does not resolve — three problems with three different owners. Every
/// variant renders to a fixed string: the underlying `io::Error` is never
/// logged, because SRT quotes both the destination and the stream id (which
/// embeds the publish token) in its own messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SrtConnectDiagnosis {
    /// The peer refused the SRT version we announce. MediaMTX 1.9.3 answers
    /// this to anything below 1.4.1; see the `srt-protocol` patch note in
    /// Cargo.toml.
    VersionRejected,
    /// The peer refused the credential: a bad or expired publish token, or a
    /// stream id it will not authorize.
    AuthRejected,
    /// Refused during the handshake for some other stated reason.
    HandshakeRejected,
    /// No answer at all within the budget.
    Timeout,
    /// The host could not be resolved.
    DnsFailed,
    /// Local socket trouble: bind, permissions, no route.
    SocketFailed,
}

impl SrtConnectDiagnosis {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::VersionRejected => "peer rejected our SRT version",
            Self::AuthRejected => "peer rejected the credential",
            Self::HandshakeRejected => "peer rejected the handshake",
            Self::Timeout => "no handshake response",
            Self::DnsFailed => "destination did not resolve",
            Self::SocketFailed => "local socket error",
        }
    }

    /// Reads the typed reject reason srt-tokio boxes into the `io::Error`
    /// rather than parsing its text, so this cannot drift with a wording change
    /// and cannot accidentally surface the message it is inspecting.
    fn classify(error: &std::io::Error) -> Self {
        use srt_protocol::packet::{CoreRejectReason, RejectReason, ServerRejectReason};
        use srt_protocol::protocol::pending_connection::ConnectionReject;
        use std::io::ErrorKind;

        if error.kind() == ErrorKind::TimedOut {
            return Self::Timeout;
        }
        let reject = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ConnectionReject>());
        if let Some(reject) = reject {
            let reason = match reject {
                ConnectionReject::Rejecting(reason) | ConnectionReject::Rejected(reason) => reason,
            };
            return match reason {
                RejectReason::Core(CoreRejectReason::Version) => Self::VersionRejected,
                RejectReason::Core(CoreRejectReason::BadSecret)
                | RejectReason::Core(CoreRejectReason::Unsecure)
                | RejectReason::Server(ServerRejectReason::Unauthorized)
                | RejectReason::Server(ServerRejectReason::Forbidden) => Self::AuthRejected,
                RejectReason::Core(CoreRejectReason::Timeout) => Self::Timeout,
                _ => Self::HandshakeRejected,
            };
        }
        match error.kind() {
            ErrorKind::NotFound | ErrorKind::AddrNotAvailable => Self::DnsFailed,
            ErrorKind::ConnectionRefused => Self::HandshakeRejected,
            _ => Self::SocketFailed,
        }
    }

    fn to_transport_error(self) -> TransportError {
        match self {
            Self::Timeout => TransportError::ConnectTimeout,
            _ => TransportError::ConnectFailed,
        }
    }
}

impl From<TransportError> for PreviewFailureReason {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::ConnectTimeout => Self::ConnectTimeout,
            TransportError::ConnectFailed => Self::ConnectFailed,
            TransportError::SendFailed => Self::SendFailed,
            TransportError::Lost => Self::TransportLost,
        }
    }
}

/// The transport seam. Keeping SRT behind it is what lets the whole state
/// machine — keyframe gating, epoch resets, backoff, cancellation — be tested
/// without a socket, and what would let the SRT implementation be replaced
/// without touching any of that logic.
pub(crate) trait PreviewTransport {
    /// Opens the session. Implementations must apply their own timeout: the
    /// caller races this against STOP and expiry, but a transport that could
    /// hang forever would still pin the worker thread after those fire.
    async fn connect(
        &mut self,
        destination: &PreviewDestination,
        stream_id: &PreviewStreamId,
        timeout: Duration,
    ) -> Result<(), TransportError>;

    /// Sends one complete muxer output. Ownership is taken so the transport can
    /// hand the buffer straight to the socket without another copy.
    async fn send(&mut self, packets: Vec<u8>) -> Result<(), TransportError>;

    async fn close(&mut self);
}

/// Cooperative cancellation. `stop` is idempotent and callable from any thread,
/// including while the worker is inside a connect or a backoff sleep.
pub(crate) struct StopToken {
    stopped: AtomicBool,
    notify: Notify,
}

impl StopToken {
    pub(crate) fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // `notify_waiters` rather than `notify_one`: every branch waiting on
        // this token has to wake, and a stored permit for a future waiter would
        // be pointless once the flag is already set.
        self.notify.notify_waiters();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Resolves once stopped. The future is created before the flag is
    /// re-checked, which is what keeps a `stop()` landing in between from being
    /// missed.
    async fn cancelled(&self) {
        loop {
            if self.is_stopped() {
                return;
            }
            let notified = self.notify.notified();
            if self.is_stopped() {
                return;
            }
            notified.await;
        }
    }
}

impl fmt::Debug for StopToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StopToken")
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

/// Lets a worker ask "am I still the current session?" without taking the
/// control mutex — which is what makes it safe for `PreviewControl` to wait for
/// a worker while holding nothing.
#[derive(Clone, Debug)]
pub(crate) struct PreviewOwnership {
    current: Arc<AtomicU64>,
    mine: u64,
}

impl PreviewOwnership {
    pub(crate) fn new(current: Arc<AtomicU64>, mine: u64) -> Self {
        Self { current, mine }
    }

    pub(crate) fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire) == self.mine
    }
}

/// Everything a worker needs, all of it already validated by `PreviewControl`.
pub(crate) struct PreviewPublishRequest {
    pub(crate) session_id: String,
    /// The device id `PreviewControl::start` already checked against the local
    /// one, carried so the lifecycle the panel sees is attributed to the same
    /// device it addressed.
    pub(crate) rustdesk_id: String,
    pub(crate) generation: u64,
    pub(crate) destination: PreviewDestination,
    pub(crate) stream_id: PreviewStreamId,
    pub(crate) expires_at: Instant,
    pub(crate) tap: Arc<PreviewTap>,
    pub(crate) ownership: PreviewOwnership,
}

impl fmt::Debug for PreviewPublishRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewPublishRequest")
            .field("session_id", &self.session_id)
            .field("rustdesk_id", &self.rustdesk_id)
            .field("generation", &self.generation)
            .field("destination", &self.destination)
            .field("stream_id", &self.stream_id)
            .finish()
    }
}

/// What `PreviewControl` keeps so it can stop the worker later.
pub(crate) struct PreviewPublisherHandle {
    stop_token: Arc<StopToken>,
    join_handle: Option<JoinHandle<()>>,
    generation: u64,
    session_id: String,
}

impl PreviewPublisherHandle {
    /// Signals and waits, bounded. **Must be called with no `PreviewControl`
    /// lock held**: the worker takes no such lock, but the wait itself is what
    /// the concurrency rule is about.
    ///
    /// Returns false when the budget ran out, in which case the thread is
    /// abandoned with its stop flag set — it holds no lock and owns only its
    /// own socket, so it can finish unobserved rather than block a STOP.
    pub(crate) fn stop_and_join(mut self, budget: Duration) -> bool {
        self.stop_token.stop();
        let Some(join_handle) = self.join_handle.take() else {
            return true;
        };
        let deadline = Instant::now() + budget;
        while !join_handle.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        join_handle.join().is_ok()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.join_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(true)
    }
}

impl fmt::Debug for PreviewPublisherHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewPublisherHandle")
            .field("generation", &self.generation)
            .field("session_id", &self.session_id)
            .field("stop_token", &self.stop_token)
            .field("finished", &self.is_finished())
            .finish()
    }
}

/// How `PreviewControl` obtains a worker. Injectable so the control tests drive
/// the real state machine over a fake transport instead of a socket.
pub(crate) trait PreviewPublisherSpawner: Send + Sync {
    /// `None` means "no publisher was started"; the session still owns the tap,
    /// which keeps the START/STOP contract identical to before C1.
    fn spawn(&self, request: PreviewPublishRequest) -> Option<PreviewPublisherHandle>;
}

/// Spawns a worker on its own thread with its own single-threaded runtime.
///
/// A dedicated thread rather than a task on some ambient runtime: the capture
/// side is plain synchronous code with no runtime in scope, and at most one
/// preview session exists at a time, so one thread is both cheaper to reason
/// about and impossible to starve by unrelated work.
pub(crate) fn spawn_worker<T, S>(
    request: PreviewPublishRequest,
    transport: T,
    sink: S,
    timings: PreviewTimings,
) -> Option<PreviewPublisherHandle>
where
    T: PreviewTransport + Send + 'static,
    S: PreviewEventSink,
{
    let stop_token = Arc::new(StopToken::new());
    let generation = request.generation;
    let session_id = request.session_id.clone();
    let worker_token = Arc::clone(&stop_token);
    let thread = std::thread::Builder::new()
        .name("screencam-preview-publisher".to_owned())
        .spawn(move || {
            // A runtime that cannot be built is a startup failure, not a reason
            // to take the capture process down.
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    let ownership = request.ownership.clone();
                    sink.emit(PreviewPublisherEvent::Failed {
                        generation,
                        reason: PreviewFailureReason::ConnectFailed,
                    });
                    if ownership.is_current() {
                        request.tap.deactivate();
                    }
                    return;
                }
            };
            runtime.block_on(run_publisher(request, transport, worker_token, sink, timings));
        });
    match thread {
        Ok(join_handle) => Some(PreviewPublisherHandle {
            stop_token,
            join_handle: Some(join_handle),
            generation,
            session_id,
        }),
        Err(_) => None,
    }
}

/// Outcome of one connected streaming stretch.
enum StreamOutcome {
    /// Terminal: no reconnection.
    Finished(PreviewStopCause),
    /// Recoverable: try to reconnect.
    Interrupted(PreviewFailureReason),
    /// Terminal failure.
    Fatal(PreviewFailureReason),
}

/// The whole publisher, transport-agnostic.
async fn run_publisher<T, S>(
    request: PreviewPublishRequest,
    mut transport: T,
    stop: Arc<StopToken>,
    sink: S,
    timings: PreviewTimings,
) where
    T: PreviewTransport,
    S: PreviewEventSink,
{
    let PreviewPublishRequest {
        generation,
        destination,
        stream_id,
        expires_at,
        tap,
        ownership,
        ..
    } = request;
    let expiry = tokio::time::Instant::from_std(expires_at);

    let mut attempt: u32 = 0;
    let terminal = loop {
        if stop.is_stopped() {
            break Ok(PreviewStopCause::Stopped);
        }
        if Instant::now() >= expires_at {
            break Ok(PreviewStopCause::Expired);
        }

        sink.emit(PreviewPublisherEvent::Connecting { generation, attempt });
        let connected = tokio::select! {
            biased;
            _ = stop.cancelled() => None,
            _ = tokio::time::sleep_until(expiry) => Some(Err(PreviewStopCause::Expired)),
            result = transport.connect(&destination, &stream_id, timings.connect_timeout) => Some(Ok(result)),
        };
        match connected {
            // STOP won the race, possibly before the handshake resolved.
            None => break Ok(PreviewStopCause::Stopped),
            Some(Err(cause)) => break Ok(cause),
            Some(Ok(Err(error))) => {
                // Nothing to close: the socket never came up.
                let reason = PreviewFailureReason::from(error);
                match next_backoff(timings.backoff, attempt) {
                    Some(delay) => {
                        sink.emit(PreviewPublisherEvent::Failed { generation, reason });
                        match wait_before_retry(delay, &stop, expiry).await {
                            RetryWait::Proceed => {
                                attempt += 1;
                                continue;
                            }
                            RetryWait::Stopped => break Ok(PreviewStopCause::Stopped),
                            RetryWait::Expired => break Ok(PreviewStopCause::Expired),
                        }
                    }
                    None => break Err(PreviewFailureReason::RetriesExhausted),
                }
            }
            Some(Ok(Ok(()))) => {}
        }

        sink.emit(PreviewPublisherEvent::Started { generation });
        // A fresh muxer per connection: MediaMTX sees a stream that starts with
        // PAT/PMT and a keyframe, never mid-GOP continuity counters.
        let mut muxer = MpegTsMuxer::new();
        match stream_until_done(&mut transport, &tap, &mut muxer, &stop, expiry).await {
            StreamOutcome::Finished(cause) => break Ok(cause),
            StreamOutcome::Fatal(reason) => break Err(reason),
            StreamOutcome::Interrupted(reason) => {
                transport.close().await;
                match next_backoff(timings.backoff, attempt) {
                    Some(delay) => {
                        sink.emit(PreviewPublisherEvent::Failed { generation, reason });
                        match wait_before_retry(delay, &stop, expiry).await {
                            RetryWait::Proceed => {
                                attempt += 1;
                                continue;
                            }
                            RetryWait::Stopped => break Ok(PreviewStopCause::Stopped),
                            RetryWait::Expired => break Ok(PreviewStopCause::Expired),
                        }
                    }
                    None => break Err(PreviewFailureReason::RetriesExhausted),
                }
            }
        }
    };

    // Closing is bounded so a wedged socket cannot keep this thread alive.
    let _ = tokio::time::timeout(timings.close_budget, transport.close()).await;

    // Only the current session may close the tap. An expired or failed worker
    // that has already been replaced must leave the newer session's tap alone.
    if ownership.is_current() {
        tap.deactivate();
    }

    match terminal {
        Ok(cause) => sink.emit(PreviewPublisherEvent::Stopped { generation, cause }),
        Err(reason) => {
            sink.emit(PreviewPublisherEvent::Failed { generation, reason });
            sink.emit(PreviewPublisherEvent::Stopped {
                generation,
                cause: PreviewStopCause::Stopped,
            });
        }
    }
}

enum RetryWait {
    Proceed,
    Stopped,
    Expired,
}

/// The backoff sleep, cancelable. This is the branch a STOP during reconnection
/// has to win, and `biased` makes it win deterministically.
async fn wait_before_retry(
    delay: Duration,
    stop: &StopToken,
    expiry: tokio::time::Instant,
) -> RetryWait {
    tokio::select! {
        biased;
        _ = stop.cancelled() => RetryWait::Stopped,
        _ = tokio::time::sleep_until(expiry) => RetryWait::Expired,
        _ = tokio::time::sleep(delay) => RetryWait::Proceed,
    }
}

fn next_backoff(backoff: &[Duration], attempt: u32) -> Option<Duration> {
    backoff.get(attempt as usize).copied()
}

/// One connected stretch: drain, gate on a keyframe, mux, send.
async fn stream_until_done<T: PreviewTransport>(
    transport: &mut T,
    tap: &PreviewTap,
    muxer: &mut MpegTsMuxer,
    stop: &StopToken,
    expiry: tokio::time::Instant,
) -> StreamOutcome {
    // Follow the tap's consumer protocol from the start: remember what we have
    // seen so an invalidation or a saturation drop cannot slip past unnoticed.
    let mut last_invalidation = tap.invalidation_snapshot();
    let mut last_dropped = tap.dropped_total();
    // Nothing is published until the stream is decodable from scratch: a
    // keyframe that carries its own SPS and PPS.
    let mut awaiting_keyframe = true;

    loop {
        let unit = tokio::select! {
            biased;
            _ = stop.cancelled() => return StreamOutcome::Finished(PreviewStopCause::Stopped),
            _ = tokio::time::sleep_until(expiry) => {
                return StreamOutcome::Finished(PreviewStopCause::Expired)
            }
            unit = next_access_unit(tap) => unit,
        };
        // `None` means the tap closed: the session is over, not a transport
        // problem, so this must not trigger a reconnection.
        let Some(unit) = unit else {
            return StreamOutcome::Finished(PreviewStopCause::TapClosed);
        };

        // Snapshot *after* the pop, generation before epoch — the order the tap
        // documents. Either check failing means this unit predates the current
        // stream and the muxer has to start over.
        let snapshot = tap.invalidation_snapshot();
        if snapshot.generation != last_invalidation.generation {
            last_invalidation = snapshot;
            muxer.reset();
            awaiting_keyframe = true;
        }
        if unit.epoch != last_invalidation.epoch {
            continue;
        }

        // A saturation drop means part of a GOP is gone; delta frames after it
        // would decode into garbage, so wait for the next keyframe.
        let dropped = tap.dropped_total();
        if dropped != last_dropped {
            last_dropped = dropped;
            muxer.reset();
            awaiting_keyframe = true;
        }

        if awaiting_keyframe {
            if !(unit.keyframe && unit.has_sps && unit.has_pps) {
                continue;
            }
            awaiting_keyframe = false;
        }

        let packets = match muxer.mux_access_unit(unit.pts_ms, unit.keyframe, &unit.annexb) {
            Ok(packets) => packets,
            // The muxer is transactional, so its state is untouched. These are
            // per-frame sanitary rejections (a negative PTS, a payload that is
            // not Annex-B); skipping the frame and resynchronizing is right,
            // and taking the session down for one bad frame is not.
            Err(MpegTsError::NegativePts)
            | Err(MpegTsError::EmptyAccessUnit)
            | Err(MpegTsError::InvalidAnnexB) => {
                awaiting_keyframe = true;
                continue;
            }
            // Not a property of one frame: the muxer refuses to emit a
            // malformed stream, so retrying with the next access unit would
            // only repeat it. Give the session up instead of publishing
            // something a player cannot decode.
            Err(MpegTsError::InternalPacketization) => {
                return StreamOutcome::Fatal(PreviewFailureReason::MuxFailed)
            }
        };
        if packets.is_empty() {
            continue;
        }

        // Whole muxer output, unfragmented: the buffer is already a whole
        // number of 188-byte packets and splitting it here would be inventing a
        // framing the muxer deliberately owns.
        if let Err(error) = transport.send(packets).await {
            return StreamOutcome::Interrupted(PreviewFailureReason::from(error));
        }
    }
}

/// Awaits the next access unit without spinning, following the tap's protocol:
/// drain, register, drain again, and only then suspend. Resolves to `None` when
/// the tap is closed.
async fn next_access_unit(tap: &PreviewTap) -> Option<EncodedAccessUnit> {
    poll_fn(|cx| {
        if let Some(unit) = tap.pop() {
            return Poll::Ready(Some(unit));
        }
        if !tap.is_active() {
            return Poll::Ready(None);
        }
        tap.register_waker(cx.waker());
        // Re-check after registering: a push that landed in between would
        // otherwise have woken nobody.
        if let Some(unit) = tap.pop() {
            return Poll::Ready(Some(unit));
        }
        if !tap.is_active() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

/// The real transport: SRT caller, MPEG-TS payload, MediaMTX on the other end.
pub(crate) struct SrtTransport {
    socket: Option<srt_tokio::SrtSocket>,
}

impl SrtTransport {
    pub(crate) fn new() -> Self {
        Self { socket: None }
    }
}

impl PreviewTransport for SrtTransport {
    async fn connect(
        &mut self,
        destination: &PreviewDestination,
        stream_id: &PreviewStreamId,
        timeout: Duration,
    ) -> Result<(), TransportError> {
        use hbb_common::futures::SinkExt;

        // srt-tokio has no connect timeout of its own, so one is imposed here.
        let attempt = srt_tokio::SrtSocket::builder()
            .call(destination.as_str(), Some(stream_id.as_str()));
        match tokio::time::timeout(timeout, attempt).await {
            Err(_elapsed) => {
                log::warn!(
                    "[screencam] preview SRT connect to {}: {}",
                    destination.as_str(),
                    SrtConnectDiagnosis::Timeout.as_str()
                );
                Err(TransportError::ConnectTimeout)
            }
            // The error itself is never logged: an `io::Error` from a failed
            // handshake can quote the destination *and* the stream id, and the
            // stream id contains the token. Only the classification and the
            // host:port — which carry no secret — come out.
            Ok(Err(error)) => {
                let diagnosis = SrtConnectDiagnosis::classify(&error);
                log::warn!(
                    "[screencam] preview SRT connect to {}: {}",
                    destination.as_str(),
                    diagnosis.as_str()
                );
                Err(diagnosis.to_transport_error())
            }
            Ok(Ok(mut socket)) => {
                // Flush any handshake-time buffering before the caller is told
                // the session is up.
                let _ = socket.flush().await;
                self.socket = Some(socket);
                Ok(())
            }
        }
    }

    async fn send(&mut self, packets: Vec<u8>) -> Result<(), TransportError> {
        use hbb_common::futures::SinkExt;

        let socket = self.socket.as_mut().ok_or(TransportError::Lost)?;
        // `Bytes::from` takes the Vec's allocation, so the muxer's output
        // reaches the socket without being copied again.
        //
        // The instant is the send clock srt-tokio stamps its packets with; the
        // presentation timestamps the player needs are already inside the
        // transport stream, written by the muxer.
        socket
            .send((Instant::now(), bytes::Bytes::from(packets)))
            .await
            .map_err(|_error| TransportError::SendFailed)
    }

    async fn close(&mut self) {
        use hbb_common::futures::SinkExt;

        if let Some(mut socket) = self.socket.take() {
            let _ = socket.close().await;
        }
    }
}

/// Production spawner: real SRT, silent sink.
pub(crate) struct SrtPublisherSpawner;

impl PreviewPublisherSpawner for SrtPublisherSpawner {
    fn spawn(&self, request: PreviewPublishRequest) -> Option<PreviewPublisherHandle> {
        let sink = PreviewLifecycleSink::new(
            request.session_id.clone(),
            request.rustdesk_id.clone(),
            request.generation,
            request.ownership.clone(),
        );
        spawn_worker(request, SrtTransport::new(), sink, PreviewTimings::PRODUCTION)
    }
}

/// Spawner that starts nothing, for the control tests that only care about the
/// session bookkeeping. Test-only: production always wants a real publisher, and
/// a silently publisher-less START is not a state the panel should be able to
/// reach.
#[cfg(test)]
pub(crate) struct NoopPublisherSpawner;

#[cfg(test)]

impl PreviewPublisherSpawner for NoopPublisherSpawner {
    fn spawn(&self, _request: PreviewPublishRequest) -> Option<PreviewPublisherHandle> {
        None
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A transport and a sink that let the state machine above be driven
    //! deterministically, with no socket and no clock of its own.

    use super::*;
    use std::sync::Mutex;

    /// The production ladder compressed from ~15 s to ~75 ms. Same number of
    /// attempts, same ordering, same cancellation points.
    pub(crate) const FAST_BACKOFF: [Duration; 5] = [
        Duration::from_millis(5),
        Duration::from_millis(10),
        Duration::from_millis(15),
        Duration::from_millis(20),
        Duration::from_millis(25),
    ];

    pub(crate) const FAST_TIMINGS: PreviewTimings = PreviewTimings {
        connect_timeout: Duration::from_millis(200),
        backoff: &FAST_BACKOFF,
        close_budget: Duration::from_millis(200),
    };

    /// What a fake connection attempt should do.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum FakeStep {
        Ok,
        Fail(TransportError),
        /// Never resolves, so the caller's race against STOP/expiry is what
        /// decides. Models a handshake to a host that silently drops packets.
        Hang,
    }

    #[derive(Debug, Default)]
    pub(crate) struct FakeLog {
        pub(crate) connects: Vec<(String, String)>,
        pub(crate) sent: Vec<Vec<u8>>,
        pub(crate) closes: usize,
    }

    impl FakeLog {
        pub(crate) fn total_bytes(&self) -> usize {
            self.sent.iter().map(|packets| packets.len()).sum()
        }
    }

    #[derive(Debug, Default)]
    pub(crate) struct FakeTransportState {
        pub(crate) log: FakeLog,
        connect_plan: Vec<FakeStep>,
        send_plan: Vec<FakeStep>,
    }

    /// Shared so a test can inspect what the worker did after it exits.
    #[derive(Clone, Default)]
    pub(crate) struct FakeTransportShared(Arc<Mutex<FakeTransportState>>);

    impl FakeTransportShared {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Steps are consumed in order and the last one repeats — see
        /// [`FakeTransport::step`].
        pub(crate) fn with_connect_plan(self, plan: Vec<FakeStep>) -> Self {
            self.0.lock().unwrap().connect_plan = plan;
            self
        }

        pub(crate) fn with_send_plan(self, plan: Vec<FakeStep>) -> Self {
            self.0.lock().unwrap().send_plan = plan;
            self
        }

        pub(crate) fn snapshot<R>(&self, read: impl FnOnce(&FakeTransportState) -> R) -> R {
            read(&self.0.lock().unwrap())
        }

        pub(crate) fn transport(&self) -> FakeTransport {
            FakeTransport {
                state: Arc::clone(&self.0),
                connects: 0,
                sends: 0,
            }
        }
    }

    pub(crate) struct FakeTransport {
        state: Arc<Mutex<FakeTransportState>>,
        connects: usize,
        sends: usize,
    }

    impl FakeTransport {
        /// The last step repeats forever, so `vec![Fail(..)]` reads as "always
        /// fails" and `vec![Fail(..), Ok]` as "fails once, then works". An empty
        /// plan always succeeds.
        fn step(plan: &[FakeStep], index: usize) -> FakeStep {
            match plan.get(index) {
                Some(step) => *step,
                None => plan.last().copied().unwrap_or(FakeStep::Ok),
            }
        }
    }

    impl PreviewTransport for FakeTransport {
        async fn connect(
            &mut self,
            destination: &PreviewDestination,
            stream_id: &PreviewStreamId,
            _timeout: Duration,
        ) -> Result<(), TransportError> {
            let step = {
                let mut state = self.state.lock().unwrap();
                state
                    .log
                    .connects
                    .push((destination.as_str().to_owned(), stream_id.as_str().to_owned()));
                let step = Self::step(&state.connect_plan, self.connects);
                self.connects += 1;
                step
            };
            match step {
                FakeStep::Ok => Ok(()),
                FakeStep::Fail(error) => Err(error),
                // Yields forever instead of sleeping, so a paused test clock
                // cannot be advanced past this by accident.
                FakeStep::Hang => {
                    loop {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }

        async fn send(&mut self, packets: Vec<u8>) -> Result<(), TransportError> {
            let step = {
                let mut state = self.state.lock().unwrap();
                let step = Self::step(&state.send_plan, self.sends);
                self.sends += 1;
                if step == FakeStep::Ok {
                    state.log.sent.push(packets);
                }
                step
            };
            match step {
                FakeStep::Ok => Ok(()),
                FakeStep::Fail(error) => Err(error),
                FakeStep::Hang => loop {
                    tokio::task::yield_now().await;
                },
            }
        }

        async fn close(&mut self) {
            self.state.lock().unwrap().log.closes += 1;
        }
    }

    /// Records events so a test can assert the sequence, including that no
    /// event ever carries session data.
    #[derive(Clone, Default)]
    pub(crate) struct RecordingEventSink(Arc<Mutex<Vec<PreviewPublisherEvent>>>);

    impl RecordingEventSink {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        pub(crate) fn events(&self) -> Vec<PreviewPublisherEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    impl PreviewEventSink for RecordingEventSink {
        fn emit(&self, event: PreviewPublisherEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// Spawner that runs the real state machine over a fake transport.
    pub(crate) struct FakePublisherSpawner {
        shared: FakeTransportShared,
        sink: RecordingEventSink,
        spawned: Arc<AtomicU64>,
        timings: PreviewTimings,
    }

    impl FakePublisherSpawner {
        pub(crate) fn new() -> Self {
            Self {
                shared: FakeTransportShared::new(),
                sink: RecordingEventSink::new(),
                spawned: Arc::new(AtomicU64::new(0)),
                timings: FAST_TIMINGS,
            }
        }

        pub(crate) fn with_send_plan(mut self, plan: Vec<FakeStep>) -> Self {
            self.shared = self.shared.with_send_plan(plan);
            self
        }

        pub(crate) fn shared(&self) -> FakeTransportShared {
            self.shared.clone()
        }

        pub(crate) fn sink(&self) -> RecordingEventSink {
            self.sink.clone()
        }

        /// How many workers were started — the way a test proves an idempotent
        /// START did *not* restart the publisher.
        pub(crate) fn spawned(&self) -> u64 {
            self.spawned.load(Ordering::SeqCst)
        }
    }

    impl PreviewPublisherSpawner for FakePublisherSpawner {
        fn spawn(&self, request: PreviewPublishRequest) -> Option<PreviewPublisherHandle> {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            spawn_worker(
                request,
                self.shared.transport(),
                self.sink.clone(),
                self.timings,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const STREAM: &str = "pv_8f12ab34c5";
    /// Not a credential: a literal that only exists so a test can assert it
    /// never appears anywhere it should not.
    const TOKEN: &str = "test-token-must-not-leak";
    const TS_PACKET_SIZE: usize = 188;

    fn destination() -> PreviewDestination {
        PreviewDestination::from_publish_url("srt://sehcontrol.sehuacho.com:8890")
            .expect("fixture url is valid")
    }

    fn stream_id() -> PreviewStreamId {
        PreviewStreamId::build(STREAM, TOKEN).expect("fixture stream id fits")
    }

    /// A keyframe carrying its own parameter sets — the only thing the publisher
    /// is allowed to start a stream on.
    fn keyframe(epoch: u64, pts_ms: i64) -> EncodedAccessUnit {
        EncodedAccessUnit::from_vec(
            epoch,
            pts_ms,
            true,
            true,
            true,
            vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0xAA],
        )
        .expect("valid fixture")
    }

    fn delta(epoch: u64, pts_ms: i64) -> EncodedAccessUnit {
        EncodedAccessUnit::from_vec(epoch, pts_ms, false, false, false, vec![0, 0, 0, 1, 0x41, 0xBB])
            .expect("valid fixture")
    }

    /// A keyframe without parameter sets: decodable only for someone who
    /// already has them, so the publisher must not open a stream with it.
    fn bare_keyframe(epoch: u64, pts_ms: i64) -> EncodedAccessUnit {
        EncodedAccessUnit::from_vec(epoch, pts_ms, true, false, false, vec![0, 0, 0, 1, 0x65, 0xCC])
            .expect("valid fixture")
    }

    struct Fixture {
        tap: Arc<PreviewTap>,
        shared: FakeTransportShared,
        sink: RecordingEventSink,
        handle: PreviewPublisherHandle,
        generation: Arc<AtomicU64>,
    }

    impl Fixture {
        /// An active tap plus a worker running the real state machine over a
        /// fake transport, exactly as `PreviewControl` would wire it.
        fn start(shared: FakeTransportShared, ttl: Duration) -> Self {
            Self::start_with(shared, ttl, FAST_TIMINGS)
        }

        fn start_with(
            shared: FakeTransportShared,
            ttl: Duration,
            timings: PreviewTimings,
        ) -> Self {
            let tap = Arc::new(PreviewTap::new());
            assert!(tap.activate().activated);
            let generation = Arc::new(AtomicU64::new(1));
            let sink = RecordingEventSink::new();
            let handle = spawn_worker(
                PreviewPublishRequest {
                    session_id: STREAM.to_owned(),
                    rustdesk_id: "485236790".to_owned(),
                    generation: 1,
                    destination: destination(),
                    stream_id: stream_id(),
                    expires_at: Instant::now() + ttl,
                    tap: Arc::clone(&tap),
                    ownership: PreviewOwnership::new(Arc::clone(&generation), 1),
                },
                shared.transport(),
                sink.clone(),
                timings,
            )
            .expect("worker thread must start");
            Self {
                tap,
                shared,
                sink,
                handle,
                generation,
            }
        }

        fn sent_buffers(&self) -> Vec<Vec<u8>> {
            self.shared.snapshot(|state| state.log.sent.clone())
        }

        fn stop(self) -> bool {
            self.handle.stop_and_join(Duration::from_secs(5))
        }

        fn join_only(self) -> bool {
            self.handle.stop_and_join(Duration::from_secs(5))
        }
    }

    /// Polls a condition instead of sleeping a fixed amount: the worker runs on
    /// a real thread, so the test has to wait for it, but never longer than it
    /// takes.
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

    fn pid_of(packet: &[u8]) -> u16 {
        (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2])
    }

    fn pids(buffer: &[u8]) -> Vec<u16> {
        buffer.chunks(TS_PACKET_SIZE).map(pid_of).collect()
    }

    // 1 + 2
    #[test]
    fn the_publisher_drains_the_tap_without_letting_it_saturate() {
        let fixture = Fixture::start(FakeTransportShared::new(), Duration::from_secs(30));
        assert!(
            wait_until(Duration::from_secs(5), || fixture
                .shared
                .snapshot(|state| !state.log.connects.is_empty())),
            "the worker must connect before it can publish"
        );

        fixture.tap.push(keyframe(0, 0));
        for pts in 1..=20 {
            fixture.tap.push(delta(0, pts));
            // Give the consumer room to keep up; the tap only holds 8.
            assert!(wait_until(Duration::from_secs(2), || fixture.tap.len() < 8));
        }

        assert!(
            wait_until(Duration::from_secs(5), || fixture.sent_buffers().len() >= 21),
            "every queued access unit should have been published"
        );
        assert!(
            wait_until(Duration::from_secs(2), || fixture.tap.is_empty()),
            "the queue must not be left holding frames"
        );
        assert_eq!(
            fixture.tap.dropped_total(),
            0,
            "an active consumer means nothing had to be evicted"
        );
        assert!(fixture.stop());
    }

    // 3
    #[test]
    fn an_idle_publisher_sends_nothing_and_still_reacts_to_the_first_frame() {
        let fixture = Fixture::start(FakeTransportShared::new(), Duration::from_secs(30));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .shared
            .snapshot(|state| !state.log.connects.is_empty())));

        // Connected but with nothing to do: the worker is parked on the tap's
        // waker. If it were polling, it would still send nothing — but the
        // second half of this test is what shows it was actually asleep and got
        // woken, rather than having spun past an empty queue and given up.
        std::thread::sleep(Duration::from_millis(50));
        assert!(fixture.sent_buffers().is_empty());

        fixture.tap.push(keyframe(0, 0));
        assert!(
            wait_until(Duration::from_secs(5), || fixture.sent_buffers().len() == 1),
            "a single push must wake the parked worker exactly once"
        );
        assert!(fixture.stop());
    }

    // 4
    #[test]
    fn nothing_is_published_before_a_keyframe_with_parameter_sets() {
        let fixture = Fixture::start(FakeTransportShared::new(), Duration::from_secs(30));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .shared
            .snapshot(|state| !state.log.connects.is_empty())));

        // Delta frames and a keyframe with no SPS/PPS: a player joining here
        // could not decode any of it.
        fixture.tap.push(delta(0, 1));
        fixture.tap.push(bare_keyframe(0, 2));
        fixture.tap.push(delta(0, 3));
        assert!(wait_until(Duration::from_secs(2), || fixture.tap.is_empty()));
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            fixture.sent_buffers().is_empty(),
            "the publisher must wait, not publish an undecodable stream"
        );

        fixture.tap.push(keyframe(0, 4));
        assert!(wait_until(Duration::from_secs(5), || !fixture
            .sent_buffers()
            .is_empty()));
        assert!(fixture.stop());
    }

    // 5
    #[test]
    fn the_first_buffer_carries_pat_and_pmt_before_the_video_pes() {
        let fixture = Fixture::start(FakeTransportShared::new(), Duration::from_secs(30));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .shared
            .snapshot(|state| !state.log.connects.is_empty())));
        fixture.tap.push(keyframe(0, 0));
        assert!(wait_until(Duration::from_secs(5), || !fixture
            .sent_buffers()
            .is_empty()));

        let first = fixture.sent_buffers().remove(0);
        assert_eq!(
            first.len() % TS_PACKET_SIZE,
            0,
            "only whole transport packets are ever sent"
        );
        let pids = pids(&first);
        let pat = pids.iter().position(|pid| *pid == 0x0000).expect("PAT");
        let pmt = pids.iter().position(|pid| *pid == 0x1000).expect("PMT");
        let video = pids.iter().position(|pid| *pid == 0x0100).expect("video PES");
        assert!(pat < video && pmt < video, "PSI must precede the payload");
        assert!(first.iter().step_by(TS_PACKET_SIZE).all(|byte| *byte == 0x47));
        assert!(fixture.stop());
    }

    // 6
    #[test]
    fn an_epoch_change_resets_the_muxer_and_waits_for_a_new_keyframe() {
        let fixture = Fixture::start(FakeTransportShared::new(), Duration::from_secs(30));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .shared
            .snapshot(|state| !state.log.connects.is_empty())));
        fixture.tap.push(keyframe(0, 0));
        fixture.tap.push(delta(0, 1));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .sent_buffers()
            .len()
            == 2));

        // The capture pipeline rebuilt its stream: a display change, say. Note
        // the tap stays active — the session outlives the stream.
        fixture.tap.invalidate_stream(1);
        assert!(fixture.tap.is_active());
        let before = fixture.sent_buffers().len();

        // Frames of the *old* epoch, and a delta of the new one: none may be
        // published, and the continuity counters must not carry over.
        fixture.tap.push(delta(0, 2));
        fixture.tap.push(delta(1, 3));
        assert!(wait_until(Duration::from_secs(2), || fixture.tap.is_empty()));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(fixture.sent_buffers().len(), before, "nothing across the seam");

        fixture.tap.push(keyframe(1, 4));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .sent_buffers()
            .len()
            == before + 1));
        // A reset muxer starts its PSI schedule over, so the first buffer of the
        // new epoch carries PAT/PMT again.
        let resumed = fixture.sent_buffers().remove(before);
        let pids = pids(&resumed);
        assert!(pids.contains(&0x0000) && pids.contains(&0x1000));
        assert!(fixture.stop());
    }

    // 7
    #[test]
    fn stop_before_the_handshake_resolves_returns_promptly() {
        let shared = FakeTransportShared::new().with_connect_plan(vec![FakeStep::Hang]);
        // A generous connect timeout, so what ends this is the STOP and not the
        // timeout firing behind it.
        let timings = PreviewTimings {
            connect_timeout: Duration::from_secs(30),
            ..FAST_TIMINGS
        };
        let fixture = Fixture::start_with(shared, Duration::from_secs(30), timings);
        assert!(wait_until(Duration::from_secs(5), || fixture
            .shared
            .snapshot(|state| !state.log.connects.is_empty())));

        let started = Instant::now();
        let tap = Arc::clone(&fixture.tap);
        assert!(fixture.stop(), "the worker must finish inside the budget");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "STOP cannot wait for a hung handshake: took {:?}",
            started.elapsed()
        );
        assert!(!tap.is_active(), "the owning worker closes its tap on the way out");
    }

    // 8
    #[test]
    fn stop_on_an_established_session_closes_the_transport() {
        let fixture = Fixture::start(FakeTransportShared::new(), Duration::from_secs(30));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .shared
            .snapshot(|state| !state.log.connects.is_empty())));
        fixture.tap.push(keyframe(0, 0));
        assert!(wait_until(Duration::from_secs(5), || !fixture
            .sent_buffers()
            .is_empty()));

        let shared = fixture.shared.clone();
        let tap = Arc::clone(&fixture.tap);
        let sink = fixture.sink.clone();
        let started = Instant::now();
        assert!(fixture.stop());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(shared.snapshot(|state| state.log.closes) >= 1, "socket closed");
        assert!(!tap.is_active());
        assert!(sink.events().iter().any(|event| matches!(
            event,
            PreviewPublisherEvent::Stopped {
                cause: PreviewStopCause::Stopped,
                ..
            }
        )));
    }

    // 9
    #[test]
    fn stop_during_backoff_does_not_wait_for_the_delay() {
        // Fails to connect forever, with a backoff long enough that a STOP that
        // merely "happened to arrive late" could not explain a fast exit.
        const SLOW: [Duration; 5] = [Duration::from_secs(30); 5];
        let shared = FakeTransportShared::new().with_connect_plan(vec![FakeStep::Fail(
            TransportError::ConnectFailed,
        )]);
        let timings = PreviewTimings {
            backoff: &SLOW,
            ..FAST_TIMINGS
        };
        let fixture = Fixture::start_with(shared, Duration::from_secs(300), timings);
        // Wait until the first attempt has failed and the worker is sleeping.
        assert!(wait_until(Duration::from_secs(5), || fixture
            .sink
            .events()
            .iter()
            .any(|event| matches!(event, PreviewPublisherEvent::Failed { .. }))));

        let started = Instant::now();
        assert!(fixture.stop());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the backoff sleep must be cancelable: took {:?}",
            started.elapsed()
        );
    }

    // 10
    #[test]
    fn a_session_expires_on_its_own_without_a_stop() {
        let fixture = Fixture::start(FakeTransportShared::new(), Duration::from_millis(80));
        let sink = fixture.sink.clone();
        let tap = Arc::clone(&fixture.tap);

        assert!(
            wait_until(Duration::from_secs(5), || sink.events().iter().any(|event| {
                matches!(
                    event,
                    PreviewPublisherEvent::Stopped {
                        cause: PreviewStopCause::Expired,
                        ..
                    }
                )
            })),
            "expiry must end the session with no STOP involved: {:?}",
            sink.events()
        );
        assert!(
            wait_until(Duration::from_secs(2), || !tap.is_active()),
            "an expired session closes its own tap"
        );
        // And a STOP arriving afterwards still has to be harmless.
        assert!(fixture.join_only());
    }

    // 11
    #[test]
    fn reconnection_stops_after_the_configured_number_of_attempts() {
        let shared = FakeTransportShared::new()
            .with_connect_plan(vec![FakeStep::Fail(TransportError::ConnectFailed)]);
        let fixture = Fixture::start(shared, Duration::from_secs(300));
        let sink = fixture.sink.clone();

        assert!(
            wait_until(Duration::from_secs(10), || sink.events().iter().any(|event| {
                matches!(
                    event,
                    PreviewPublisherEvent::Failed {
                        reason: PreviewFailureReason::RetriesExhausted,
                        ..
                    }
                )
            })),
            "the attempt cap must be reached: {:?}",
            sink.events()
        );
        // One initial attempt plus one per backoff step, and not one more.
        let attempts = fixture.shared.snapshot(|state| state.log.connects.len());
        assert_eq!(attempts, FAST_BACKOFF.len() + 1);
        assert!(fixture.join_only());
    }

    // 12
    #[test]
    fn the_stream_id_is_exactly_the_contract_the_server_parses() {
        let built = PreviewStreamId::build("pv_8f12ab34c5", "s3cr3t").expect("fits");
        assert_eq!(built.as_str(), "publish:pv_8f12ab34c5:token=s3cr3t");

        // And it reaches the transport verbatim, not as a URL parameter.
        let fixture = Fixture::start(FakeTransportShared::new(), Duration::from_secs(30));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .shared
            .snapshot(|state| !state.log.connects.is_empty())));
        let (destination, seen) = fixture.shared.snapshot(|state| state.log.connects[0].clone());
        assert_eq!(destination, "sehcontrol.sehuacho.com:8890");
        assert_eq!(seen, format!("publish:{STREAM}:token={TOKEN}"));
        assert!(!destination.contains("streamid"));
        assert!(fixture.stop());
    }

    // 13
    #[test]
    fn an_over_long_stream_id_is_rejected_before_any_socket_is_opened() {
        let long_token = "t".repeat(MAX_STREAM_ID_BYTES);
        assert_eq!(
            PreviewStreamId::build(STREAM, &long_token).err(),
            Some(PreviewFailureReason::StreamIdTooLong)
        );

        // Exactly at the limit is still accepted: the check is a limit, not an
        // off-by-one margin.
        let prefix = format!("publish:{STREAM}:token=");
        let exact = "t".repeat(MAX_STREAM_ID_BYTES - prefix.len());
        let built = PreviewStreamId::build(STREAM, &exact).expect("512 bytes fits");
        assert_eq!(built.as_str().len(), MAX_STREAM_ID_BYTES);
    }

    // 14
    #[test]
    fn debug_output_and_failures_never_carry_the_token_or_the_stream_id() {
        let stream_id = stream_id();
        let rendered = format!("{stream_id:?}");
        assert!(!rendered.contains(TOKEN), "Debug leaked the token: {rendered}");
        assert!(!rendered.contains("publish:"), "Debug leaked the stream id");
        assert!(rendered.contains("<redacted>"));

        let request = PreviewPublishRequest {
            session_id: STREAM.to_owned(),
            rustdesk_id: "485236790".to_owned(),
            generation: 7,
            destination: destination(),
            stream_id,
            expires_at: Instant::now(),
            tap: Arc::new(PreviewTap::new()),
            ownership: PreviewOwnership::new(Arc::new(AtomicU64::new(7)), 7),
        };
        let rendered = format!("{request:?}");
        assert!(!rendered.contains(TOKEN));
        assert!(!rendered.contains("publish:"));
        // The destination is deliberately present: host:port carries no secret
        // and is what makes a failure diagnosable at all.
        assert!(rendered.contains("sehcontrol.sehuacho.com:8890"));

        // Every failure reason is a fixed string, so no code path can format a
        // connection parameter into one.
        for reason in [
            PreviewFailureReason::InvalidDestination,
            PreviewFailureReason::StreamIdTooLong,
            PreviewFailureReason::ConnectTimeout,
            PreviewFailureReason::ConnectFailed,
            PreviewFailureReason::TransportLost,
            PreviewFailureReason::SendFailed,
            PreviewFailureReason::MuxFailed,
            PreviewFailureReason::RetriesExhausted,
        ] {
            let rendered = format!("{reason} {reason:?}");
            assert!(!rendered.contains(TOKEN));
            assert!(!rendered.contains("publish:"));
        }
    }

    /// Builds the error srt-tokio produces for a refused handshake: it boxes a
    /// `ConnectionReject` into the `io::Error`, which is exactly what the
    /// classifier reads.
    fn rejected(reason: srt_protocol::packet::RejectReason) -> std::io::Error {
        use srt_protocol::protocol::pending_connection::ConnectionReject;
        std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            ConnectionReject::Rejected(reason),
        )
    }

    #[test]
    fn a_version_rejection_is_told_apart_from_every_other_failure() {
        use srt_protocol::packet::{CoreRejectReason, RejectReason, ServerRejectReason};

        // The one this whole patch exists for: MediaMTX answering 1008.
        assert_eq!(
            SrtConnectDiagnosis::classify(&rejected(RejectReason::Core(
                CoreRejectReason::Version
            ))),
            SrtConnectDiagnosis::VersionRejected
        );

        for reason in [
            RejectReason::Core(CoreRejectReason::BadSecret),
            RejectReason::Core(CoreRejectReason::Unsecure),
            RejectReason::Server(ServerRejectReason::Unauthorized),
            RejectReason::Server(ServerRejectReason::Forbidden),
        ] {
            assert_eq!(
                SrtConnectDiagnosis::classify(&rejected(reason)),
                SrtConnectDiagnosis::AuthRejected,
                "{reason:?} is a credential problem, not a version one"
            );
        }

        assert_eq!(
            SrtConnectDiagnosis::classify(&rejected(RejectReason::Core(
                CoreRejectReason::Rogue
            ))),
            SrtConnectDiagnosis::HandshakeRejected
        );
        assert_eq!(
            SrtConnectDiagnosis::classify(&rejected(RejectReason::Core(
                CoreRejectReason::Timeout
            ))),
            SrtConnectDiagnosis::Timeout
        );
    }

    #[test]
    fn transport_level_failures_are_classified_without_a_reject_reason() {
        use std::io::{Error, ErrorKind};

        assert_eq!(
            SrtConnectDiagnosis::classify(&Error::new(ErrorKind::TimedOut, "")),
            SrtConnectDiagnosis::Timeout
        );
        assert_eq!(
            SrtConnectDiagnosis::classify(&Error::new(ErrorKind::NotFound, "no such host")),
            SrtConnectDiagnosis::DnsFailed
        );
        assert_eq!(
            SrtConnectDiagnosis::classify(&Error::new(ErrorKind::PermissionDenied, "bind")),
            SrtConnectDiagnosis::SocketFailed
        );
        // A refusal with no typed reason still reads as a handshake refusal
        // rather than being mislabelled a local socket problem.
        assert_eq!(
            SrtConnectDiagnosis::classify(&Error::new(ErrorKind::ConnectionRefused, "")),
            SrtConnectDiagnosis::HandshakeRejected
        );
    }

    #[test]
    fn a_diagnosis_never_carries_session_data() {
        use srt_protocol::packet::{CoreRejectReason, RejectReason};

        for diagnosis in [
            SrtConnectDiagnosis::VersionRejected,
            SrtConnectDiagnosis::AuthRejected,
            SrtConnectDiagnosis::HandshakeRejected,
            SrtConnectDiagnosis::Timeout,
            SrtConnectDiagnosis::DnsFailed,
            SrtConnectDiagnosis::SocketFailed,
        ] {
            let rendered = format!("{} {diagnosis:?}", diagnosis.as_str());
            assert!(!rendered.contains(TOKEN));
            assert!(!rendered.contains("publish:"));
            assert!(!rendered.contains("token"));
        }

        // Only the timeout maps to a distinct transport error; everything else
        // stays a retryable connect failure, so the state machine is unchanged.
        assert_eq!(
            SrtConnectDiagnosis::Timeout.to_transport_error(),
            TransportError::ConnectTimeout
        );
        assert_eq!(
            SrtConnectDiagnosis::classify(&rejected(RejectReason::Core(
                CoreRejectReason::Version
            )))
            .to_transport_error(),
            TransportError::ConnectFailed
        );
    }

    /// The lifecycle snapshot is process-wide, so these run one at a time.
    static LIFECYCLE_TESTS: Mutex<()> = Mutex::new(());

    fn lifecycle_sink(generation: u64, current: &Arc<AtomicU64>) -> PreviewLifecycleSink {
        PreviewLifecycleSink::new(
            STREAM.to_owned(),
            "485236790".to_owned(),
            generation,
            PreviewOwnership::new(Arc::clone(current), generation),
        )
    }

    #[test]
    fn the_snapshot_keeps_the_session_identity_the_panel_addressed() {
        let _guard = LIFECYCLE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        reset_lifecycle_for_test();
        let current = Arc::new(AtomicU64::new(4));
        let sink = lifecycle_sink(4, &current);

        sink.emit(PreviewPublisherEvent::Connecting {
            generation: 4,
            attempt: 0,
        });
        let snapshot = lifecycle_snapshot().expect("a snapshot was published");
        assert_eq!(snapshot.session_id, STREAM);
        assert_eq!(snapshot.rustdesk_id, "485236790");
        assert_eq!(snapshot.generation, 4);
        assert_eq!(snapshot.state, "connecting");
        assert_eq!(snapshot.attempt, Some(0));

        sink.emit(PreviewPublisherEvent::Started { generation: 4 });
        let started = lifecycle_snapshot().expect("started");
        assert_eq!(started.state, "started");
        assert!(
            started.sequence > snapshot.sequence,
            "the sequence must advance so the UI notices"
        );
    }

    #[test]
    fn the_snapshot_json_carries_no_session_secret() {
        let _guard = LIFECYCLE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        reset_lifecycle_for_test();
        let current = Arc::new(AtomicU64::new(1));
        let sink = lifecycle_sink(1, &current);

        sink.emit(PreviewPublisherEvent::Failed {
            generation: 1,
            reason: PreviewFailureReason::ConnectFailed,
        });

        let json = lifecycle_snapshot().expect("failed").to_json();
        assert!(json.contains(r#""event":"screen_cam.preview.failed""#));
        assert!(json.contains(r#""reason":"connect_failed""#));
        // The three things that must never travel, checked on the rendered
        // string rather than field by field.
        assert!(!json.contains(TOKEN));
        assert!(!json.contains("publish:"));
        assert!(!json.contains("srt://"));
        assert!(!json.contains("sehcontrol.sehuacho.com"));
    }

    #[test]
    fn a_replaced_worker_cannot_repaint_the_session_that_replaced_it() {
        let _guard = LIFECYCLE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        reset_lifecycle_for_test();
        let current = Arc::new(AtomicU64::new(1));
        let old = lifecycle_sink(1, &current);
        old.emit(PreviewPublisherEvent::Started { generation: 1 });

        // A newer START takes over.
        current.store(2, Ordering::SeqCst);
        let new = lifecycle_sink(2, &current);
        new.emit(PreviewPublisherEvent::Started { generation: 2 });
        let after_replacement = lifecycle_snapshot().expect("new session");

        // The outgoing worker now unwinds and reports its own end.
        old.emit(PreviewPublisherEvent::Stopped {
            generation: 1,
            cause: PreviewStopCause::Stopped,
        });

        let latest = lifecycle_snapshot().expect("still the new session");
        assert_eq!(
            latest, after_replacement,
            "an old worker overwrote a newer one"
        );
        assert_eq!(latest.generation, 2);
        assert_eq!(latest.state, "started");
    }

    #[test]
    fn a_retry_cycle_is_reported_in_order() {
        let _guard = LIFECYCLE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        reset_lifecycle_for_test();
        let current = Arc::new(AtomicU64::new(9));
        let sink = lifecycle_sink(9, &current);

        let mut states = Vec::new();
        for event in [
            PreviewPublisherEvent::Connecting {
                generation: 9,
                attempt: 0,
            },
            PreviewPublisherEvent::Failed {
                generation: 9,
                reason: PreviewFailureReason::ConnectTimeout,
            },
            PreviewPublisherEvent::Connecting {
                generation: 9,
                attempt: 1,
            },
            PreviewPublisherEvent::Started { generation: 9 },
        ] {
            sink.emit(event);
            let snapshot = lifecycle_snapshot().expect("snapshot");
            states.push((snapshot.state, snapshot.sequence));
        }

        assert_eq!(
            states.iter().map(|(state, _)| *state).collect::<Vec<_>>(),
            vec!["connecting", "failed", "connecting", "started"]
        );
        // Strictly increasing, so a consumer can drop what it already sent.
        for pair in states.windows(2) {
            assert!(pair[1].1 > pair[0].1);
        }
    }

    #[test]
    fn every_failure_reason_maps_to_a_closed_safe_code() {
        for (reason, expected) in [
            (PreviewFailureReason::InvalidDestination, "connect_failed"),
            (PreviewFailureReason::StreamIdTooLong, "connect_failed"),
            (PreviewFailureReason::ConnectTimeout, "connect_failed"),
            (PreviewFailureReason::ConnectFailed, "connect_failed"),
            (PreviewFailureReason::TransportLost, "send_failed"),
            (PreviewFailureReason::SendFailed, "send_failed"),
            (PreviewFailureReason::MuxFailed, "mux_failed"),
            (PreviewFailureReason::RetriesExhausted, "retries_exhausted"),
        ] {
            assert_eq!(reason.safe_code(), expected);
        }
        for (cause, expected) in [
            (PreviewStopCause::Stopped, "stopped"),
            (PreviewStopCause::Expired, "expired"),
            (PreviewStopCause::TapClosed, "tap_closed"),
        ] {
            assert_eq!(cause.safe_code(), expected);
        }
    }

    // 19
    #[test]
    fn a_dead_publisher_leaves_the_shared_capture_outlet_usable() {
        let shared = FakeTransportShared::new()
            .with_connect_plan(vec![FakeStep::Fail(TransportError::ConnectFailed)]);
        let fixture = Fixture::start(shared, Duration::from_secs(300));
        let tap = Arc::clone(&fixture.tap);
        let sink = fixture.sink.clone();
        assert!(wait_until(Duration::from_secs(10), || sink
            .events()
            .iter()
            .any(|event| matches!(
                event,
                PreviewPublisherEvent::Stopped { .. }
            ))));
        assert!(fixture.join_only());

        // What the capture thread does next must still be a cheap, infallible
        // no-op — not a panic, not an error, not a block.
        for pts in 0..50 {
            assert_eq!(
                tap.push_annexb_copy_if_active(0, pts, true, true, true, &[0, 0, 0, 1, 0x65, 0x01]),
                Ok(TapPushResult::IgnoredInactive)
            );
        }
        // And the stream bookkeeping the capture loop owns is untouched.
        let invalidation = tap.invalidate_stream(9);
        assert_eq!(invalidation.epoch, 9);
        assert_eq!(tap.dropped_total(), 0);
    }

    // 20
    #[test]
    fn a_transport_that_always_fails_never_blocks_or_starves_the_producer() {
        let shared = FakeTransportShared::new()
            .with_send_plan(vec![FakeStep::Fail(TransportError::SendFailed)]);
        let fixture = Fixture::start(shared, Duration::from_secs(300));
        assert!(wait_until(Duration::from_secs(5), || fixture
            .shared
            .snapshot(|state| !state.log.connects.is_empty())));

        // The producer keeps handing frames over at full rate while every send
        // fails and the worker reconnects underneath. None of this may make a
        // push block or fail: the capture thread's contract is unconditional.
        let started = Instant::now();
        for pts in 0..200 {
            let result = fixture.tap.push_annexb_copy_if_active(
                0,
                pts,
                true,
                true,
                true,
                &[0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0xAA],
            );
            assert!(result.is_ok(), "push must never fail because SRT is failing");
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "200 pushes took {:?} — the producer was being blocked",
            started.elapsed()
        );
        assert!(fixture.stop());
    }

    use super::super::tap::TapPushResult;

    /// End-to-end over a **real** SRT socket, with the production
    /// [`SrtTransport`], the production muxer and a real [`PreviewTap`]: only the
    /// peer is local. It proves the three claims the fake transport cannot —
    /// that a caller-mode session actually opens, that the stream id survives a
    /// real HSv5 handshake, and that real MPEG-TS bytes arrive intact.
    ///
    /// `#[ignore]` because it binds a UDP port, which has no place in the suite
    /// every build runs. Run it explicitly:
    ///
    /// ```text
    /// cargo test --features screencam --lib publisher::tests::a_real_srt_session -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "binds a real UDP socket on loopback"]
    fn a_real_srt_session_delivers_mpegts_to_a_listening_peer() {
        use hbb_common::futures::StreamExt;

        const EXPECTED_BUFFERS: usize = 6;

        let listener_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        // Bind the UDP socket here so the ephemeral port is known before the
        // caller is pointed at it.
        let (port, udp) = listener_runtime.block_on(async {
            let udp = tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind loopback");
            let port = udp.local_addr().expect("local addr").port();
            (port, udp)
        });
        println!("[real-srt] listener bound on 127.0.0.1:{port}");

        let received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let seen_stream_id = Arc::new(Mutex::new(Option::<String>::None));
        let listener_received = Arc::clone(&received);
        let listener_stream_id = Arc::clone(&seen_stream_id);
        let listener = std::thread::spawn(move || {
            listener_runtime.block_on(async move {
                let (_binding, mut incoming) = srt_tokio::SrtListener::builder()
                    .socket(udp)
                    .bind(port)
                    .await
                    .expect("listener");
                let request = incoming
                    .incoming()
                    .next()
                    .await
                    .expect("a caller must arrive");
                *listener_stream_id.lock().unwrap() =
                    request.stream_id().map(|id| id.to_string());
                let mut socket = request.accept(None).await.expect("accept");
                while let Some(Ok((_instant, payload))) = socket.next().await {
                    listener_received.lock().unwrap().push(payload.to_vec());
                    if listener_received.lock().unwrap().len() >= EXPECTED_BUFFERS {
                        break;
                    }
                }
            });
        });

        let tap = Arc::new(PreviewTap::new());
        assert!(tap.activate().activated);
        let generation = Arc::new(AtomicU64::new(1));
        let sink = RecordingEventSink::new();
        let handle = spawn_worker(
            PreviewPublishRequest {
                session_id: STREAM.to_owned(),
                rustdesk_id: "485236790".to_owned(),
                generation: 1,
                destination: PreviewDestination::from_publish_url(&format!(
                    "srt://127.0.0.1:{port}"
                ))
                .expect("loopback url"),
                stream_id: stream_id(),
                expires_at: Instant::now() + Duration::from_secs(30),
                tap: Arc::clone(&tap),
                ownership: PreviewOwnership::new(Arc::clone(&generation), 1),
            },
            SrtTransport::new(),
            sink.clone(),
            PreviewTimings::PRODUCTION,
        )
        .expect("worker thread");

        // Real capture shape: a keyframe with parameter sets, then inter frames.
        assert!(wait_until(Duration::from_secs(10), || sink
            .events()
            .iter()
            .any(|event| matches!(event, PreviewPublisherEvent::Started { .. }))));
        tap.push(keyframe(0, 0));
        for pts in 1..EXPECTED_BUFFERS as i64 {
            tap.push(delta(0, pts * 33));
            assert!(wait_until(Duration::from_secs(2), || tap.len() < 8));
        }

        assert!(
            wait_until(Duration::from_secs(15), || received.lock().unwrap().len()
                >= EXPECTED_BUFFERS),
            "the peer received {} of {EXPECTED_BUFFERS} buffers; events: {:?}",
            received.lock().unwrap().len(),
            sink.events()
        );
        assert!(handle.stop_and_join(Duration::from_secs(5)));
        listener.join().expect("listener thread");

        let buffers = received.lock().unwrap().clone();
        let total: usize = buffers.iter().map(|buffer| buffer.len()).sum();
        println!(
            "[real-srt] streamid seen by peer: publish:{STREAM}:token=<redacted>  buffers={} bytes={total}",
            buffers.len()
        );
        assert_eq!(
            seen_stream_id.lock().unwrap().as_deref(),
            Some(format!("publish:{STREAM}:token={TOKEN}").as_str()),
            "the stream id must cross a real handshake verbatim"
        );
        for buffer in &buffers {
            assert_eq!(buffer.len() % TS_PACKET_SIZE, 0, "whole TS packets only");
            assert!(
                buffer.iter().step_by(TS_PACKET_SIZE).all(|byte| *byte == 0x47),
                "every packet must start with the TS sync byte"
            );
        }
        let first = pids(&buffers[0]);
        assert!(first.contains(&0x0000) && first.contains(&0x1000) && first.contains(&0x0100));
        assert!(total > 0);
    }

    use std::sync::Mutex;
}
