// Minimal RTSP/1.0 server for Sehcontrol ScreenCam (Fase 1 MVP).
//
// Supports exactly what a DVR/NVR or VLC needs to pull the single `/live/main`
// stream this module serves: OPTIONS, DESCRIBE, SETUP (both UDP and TCP
// interleaved transport), PLAY, GET_PARAMETER (used by many clients as a
// keep-alive) and TEARDOWN, plus Basic/Digest authentication when the panel
// has issued credentials (see auth.rs). No multiple routes — see
// docs/SCREENCAM_PLAN.md Fase 1/3 for what's intentionally deferred.
//
// Compatibility notes for real DVR/NVR hardware (Dahua, Hikvision and the
// live555-derived stacks most of the market ships), all of which this server
// now accommodates and none of which VLC or ZoneMinder's FFmpeg client ever
// exercised:
//   - RTCP Sender Reports go out every RTCP_INTERVAL on both transports (UDP
//     to the client's RTCP port, TCP on the interleaved channel paired with
//     RTP). Several firmwares tear a session down if the sender never reports.
//   - RTCP Receiver Reports coming *back* over an interleaved connection are
//     recognised and skipped instead of being fed to the request parser. Both
//     Dahua and Hikvision send them, and mistaking one for a request used to
//     desynchronise the connection.
//   - SET_PARAMETER is accepted as a keep-alive (it is the one most NVRs use,
//     more than GET_PARAMETER) and PAUSE is answered rather than refused.
//   - DESCRIBE carries Content-Base, and the SDP uses `trackID=0` control
//     URLs, so a client that resolves the control attribute against the base
//     builds a SETUP URL this server recognises.
//   - PLAY carries RTP-Info (url/seq/rtptime), which live555-based clients
//     want before they will start their decoder.
//
// Known simplifications (tracked in docs/SCREENCAM_PLAN.md, not silently
// hidden): incoming RTCP is skipped, not parsed — this server does not adapt
// its bitrate to the receiver reports it gets. There is still only one stream
// and one track, so there is no sub-stream profile for NVRs that would prefer
// a low-resolution channel for their live wall.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use hbb_common::{anyhow::anyhow, bail, log, ResultType};

use super::auth;
use super::rtp;
use super::SharedState;

const RTSP_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_ACCESS_UNIT_QUEUE_CAPACITY: usize = 2;
const TCP_WRITER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const UDP_WOULD_BLOCK_LIMIT: u8 = 3;
/// Advertised in the SETUP response. NVRs use it to decide how often to send
/// their keep-alive; 60 s is what ONVIF cameras conventionally report and what
/// `GetStreamUri` already advertises as `PT60S`.
const SESSION_TIMEOUT_SECS: u32 = 60;
/// How often each session gets a Sender Report. RFC 3550 wants the RTCP
/// bandwidth held to ~5% of the session bandwidth; for a single video sender
/// with no receiver reports to schedule around, a fixed 5 s is well inside
/// that and is what most camera firmwares emit.
const RTCP_INTERVAL: Duration = Duration::from_secs(5);
/// SDP media-level control attribute, and therefore the suffix an NVR appends
/// to the Content-Base when it issues SETUP.
const TRACK_CONTROL: &str = "trackID=0";

pub(super) struct RtpAccessUnit {
    packets: Vec<Vec<u8>>,
}

impl RtpAccessUnit {
    pub(super) fn new(packets: Vec<Vec<u8>>) -> Self {
        Self { packets }
    }
}

enum WriterMessage {
    AccessUnit {
        epoch: u64,
        access_unit: Arc<RtpAccessUnit>,
    },
    Close,
}

enum Transport {
    Tcp {
        sender: SyncSender<WriterMessage>,
        /// Shared with the interleaved RTP writer and with every RTSP
        /// response, so the mutex is what keeps a Sender Report from landing
        /// in the middle of somebody else's frame.
        stream: Arc<Mutex<TcpStream>>,
        rtcp_channel: u8,
    },
    Udp {
        rtp_socket: UdpSocket,
        // Also where the client's own receiver reports land, instead of
        // getting an ICMP port-unreachable back.
        rtcp_socket: UdpSocket,
    },
}

pub struct Session {
    pub id: String,
    transport: Transport,
    shutdown_stream: Arc<TcpStream>,
    epoch: u64,
    closed: AtomicBool,
    udp_would_block_count: AtomicU8,
}

impl Session {
    pub(super) fn dispatch_access_unit(
        &self,
        epoch: u64,
        access_unit: Arc<RtpAccessUnit>,
    ) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "RTSP session is closed",
            ));
        }
        if epoch != self.epoch {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RTP access unit epoch does not match RTSP session",
            ));
        }

        match &self.transport {
            Transport::Udp { rtp_socket, .. } => {
                self.dispatch_udp_access_unit_with(&access_unit, |packet| rtp_socket.send(packet))
            }
            Transport::Tcp { sender, .. } => sender
                .try_send(WriterMessage::AccessUnit { epoch, access_unit })
                .map_err(map_tcp_queue_error),
        }
    }

    /// Sends one RTCP compound packet out-of-band of the access-unit queue.
    /// Deliberately best-effort: a report that cannot go out says nothing
    /// about whether video still can, so the caller logs and keeps the session.
    fn send_rtcp(&self, packet: &[u8]) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "RTSP session is closed",
            ));
        }
        match &self.transport {
            Transport::Udp { rtcp_socket, .. } => rtcp_socket.send(packet).map(|_| ()),
            Transport::Tcp {
                stream,
                rtcp_channel,
                ..
            } => {
                let len = u16::try_from(packet.len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "RTCP packet exceeds RTSP interleaved frame limit",
                    )
                })?;
                let len = len.to_be_bytes();
                let header = [b'$', *rtcp_channel, len[0], len[1]];
                let mut stream = stream.lock().map_err(|_| {
                    io::Error::new(io::ErrorKind::Other, "RTSP stream lock poisoned")
                })?;
                stream.write_all(&header)?;
                stream.write_all(packet)
            }
        }
    }

    /// Forces the client to reconnect and issue a fresh DESCRIBE/SETUP after
    /// the capture source or its H.264 parameter sets change.
    pub fn close(&self) {
        if self
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if let Transport::Tcp { sender, .. } = &self.transport {
                let _ = sender.try_send(WriterMessage::Close);
            }
            // This clone is independent from the writer's serialization mutex,
            // so shutdown can interrupt a blocked write immediately.
            let _ = self.shutdown_stream.shutdown(Shutdown::Both);
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    fn dispatch_udp_access_unit_with(
        &self,
        access_unit: &RtpAccessUnit,
        send: impl FnMut(&[u8]) -> io::Result<usize>,
    ) -> io::Result<()> {
        match send_udp_access_unit(access_unit, send) {
            Ok(()) => {
                self.udp_would_block_count.store(0, Ordering::Release);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let previous = self.udp_would_block_count.fetch_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |count| Some(count.saturating_add(1)),
                );
                let count = match previous {
                    Ok(previous) | Err(previous) => previous.saturating_add(1),
                };
                if count >= UDP_WOULD_BLOCK_LIMIT {
                    Err(error)
                } else {
                    // UDP cannot resume a partially sent access unit. Drop this
                    // one for this session and let the next successful unit
                    // reset the consecutive-pressure counter.
                    Ok(())
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn map_tcp_queue_error(error: TrySendError<WriterMessage>) -> io::Error {
    match error {
        TrySendError::Full(_) => io::Error::new(
            io::ErrorKind::WouldBlock,
            "RTSP interleaved RTP queue is full",
        ),
        TrySendError::Disconnected(_) => io::Error::new(
            io::ErrorKind::BrokenPipe,
            "RTSP interleaved RTP writer stopped",
        ),
    }
}

fn send_udp_access_unit(
    access_unit: &RtpAccessUnit,
    mut send: impl FnMut(&[u8]) -> io::Result<usize>,
) -> io::Result<()> {
    for packet in &access_unit.packets {
        if send(packet)? != packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partial RTP datagram send",
            ));
        }
    }
    Ok(())
}

struct TcpWriterStart {
    receiver: Receiver<WriterMessage>,
    writer: Box<dyn AccessUnitWriter>,
}

trait AccessUnitWriter: Send {
    fn write_access_unit(
        &mut self,
        access_unit: &RtpAccessUnit,
        should_continue: &mut dyn FnMut() -> bool,
    ) -> io::Result<()>;
}

struct TcpInterleavedWriter {
    stream: Arc<Mutex<TcpStream>>,
    rtp_channel: u8,
}

impl AccessUnitWriter for TcpInterleavedWriter {
    fn write_access_unit(
        &mut self,
        access_unit: &RtpAccessUnit,
        should_continue: &mut dyn FnMut() -> bool,
    ) -> io::Result<()> {
        write_interleaved_access_unit(&self.stream, self.rtp_channel, access_unit, should_continue)
    }
}

fn start_tcp_writer(
    start: TcpWriterStart,
    session: Weak<Session>,
    state: Weak<SharedState>,
) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("screencam-rtsp-writer".to_owned())
        .spawn(move || tcp_writer_loop(start, session, state))
}

fn tcp_writer_loop(mut start: TcpWriterStart, session: Weak<Session>, state: Weak<SharedState>) {
    loop {
        let message = match start.receiver.recv_timeout(TCP_WRITER_POLL_INTERVAL) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => {
                if session
                    .upgrade()
                    .map_or(true, |session| session.closed.load(Ordering::Acquire))
                {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let (epoch, access_unit) = match message {
            WriterMessage::AccessUnit { epoch, access_unit } => (epoch, access_unit),
            WriterMessage::Close => break,
        };
        let Some(current_session) = session.upgrade() else {
            break;
        };
        let Some(current_state) = state.upgrade() else {
            current_session.close();
            break;
        };
        if current_session.closed.load(Ordering::Acquire)
            || epoch != current_session.epoch
            || current_state.stream_epoch() != epoch
        {
            retire_writer_session(&current_state, &current_session, None);
            break;
        }

        if let Err(error) = start.writer.write_access_unit(&access_unit, &mut || {
            !current_session.closed.load(Ordering::Acquire) && current_state.stream_epoch() == epoch
        }) {
            retire_writer_session(&current_state, &current_session, Some(&error));
            break;
        }
    }
}

fn write_interleaved_access_unit(
    stream: &Arc<Mutex<TcpStream>>,
    rtp_channel: u8,
    access_unit: &RtpAccessUnit,
    mut should_continue: impl FnMut() -> bool,
) -> io::Result<()> {
    let mut stream = stream
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "RTSP stream lock poisoned"))?;
    for packet in &access_unit.packets {
        if !should_continue() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "RTSP stream epoch changed",
            ));
        }
        let packet_len = u16::try_from(packet.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "RTP packet exceeds RTSP interleaved frame limit",
            )
        })?;
        let packet_len = packet_len.to_be_bytes();
        let framed_header = [b'$', rtp_channel, packet_len[0], packet_len[1]];
        stream.write_all(&framed_header)?;
        // The packet allocation is shared across all sessions; write_all only
        // borrows it and does not make a per-session payload copy.
        stream.write_all(packet)?;
    }
    Ok(())
}

fn retire_writer_session(state: &SharedState, session: &Arc<Session>, error: Option<&io::Error>) {
    let removed = super::take_matching_arcs(&state.sessions, |registered| {
        registered.id == session.id && Arc::ptr_eq(registered, session)
    });
    if !removed.is_empty() {
        if let Some(error) = error {
            log::warn!("[screencam] removing RTSP session after TCP writer failed: {error}");
        }
        set_rtsp_client_count(state);
    }
    drop(removed);
    // The failing writer owns this exact session even when a newer connection
    // has already replaced it in the registry. Never close the replacement.
    session.close();
}

fn set_rtsp_client_count(state: &SharedState) {
    super::set_rtsp_clients(state.sessions.lock().unwrap().len());
}

fn register_session_replacing_same_id(
    state: &SharedState,
    new_session: Arc<Session>,
) -> Vec<Arc<Session>> {
    let mut replaced = Vec::new();
    let mut sessions = state.sessions.lock().unwrap();
    sessions.retain(|current| {
        if current.id == new_session.id {
            replaced.push(current.clone());
            false
        } else {
            true
        }
    });
    sessions.push(new_session);
    replaced
}

struct RtspRequest {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
}

pub fn start_listener(port: u16, state: Arc<SharedState>) -> ResultType<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    log::info!("[screencam] RTSP listening on 0.0.0.0:{port}");
    start_rtcp_reporter(Arc::downgrade(&state));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let state = state.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle_connection(stream, port, state) {
                            log::debug!("[screencam] rtsp connection ended: {e:?}");
                        }
                    });
                }
                Err(e) => log::warn!("[screencam] rtsp accept error: {e}"),
            }
        }
    });
    Ok(())
}

/// Periodically emits an RTCP Sender Report to every live session. A single
/// thread rather than one per session: the reports are 5 s apart and derived
/// from one shared snapshot, so there is nothing per-session to schedule.
///
/// Failures never retire a session. RTCP is advisory — a report that can't go
/// out (a full UDP buffer, a client that closed only its RTCP port) says
/// nothing about whether video is still flowing, and the RTP path already has
/// its own removal logic for the case where it isn't.
///
/// Reports are emitted in sequence, and an interleaved one has to take the
/// session's write mutex, so a TCP peer that has stopped reading can hold this
/// loop for up to `RTSP_WRITE_TIMEOUT` and delay the other sessions' reports by
/// that much. Acceptable while the interval is an order of magnitude larger
/// than the timeout; if that stops holding, this wants a thread per session
/// rather than a shorter interval.
fn start_rtcp_reporter(state: Weak<SharedState>) {
    std::thread::Builder::new()
        .name("screencam-rtcp".to_owned())
        .spawn(move || loop {
            std::thread::sleep(RTCP_INTERVAL);
            let Some(state) = state.upgrade() else {
                break;
            };
            let sessions = state.sessions.lock().unwrap().clone();
            for session in sessions {
                let Some(snapshot) = state.rtp_stats.snapshot_for_epoch(session.epoch()) else {
                    continue;
                };
                // Nothing has actually been sent yet for this epoch, so there
                // is no clock mapping worth reporting.
                if snapshot.ntp == 0 {
                    continue;
                }
                let packet = rtp::build_sender_report(&snapshot, RTCP_CNAME);
                if let Err(error) = session.send_rtcp(&packet) {
                    log::debug!(
                        "[screencam] RTCP report not sent for session {}: {error}",
                        session.id
                    );
                }
            }
        })
        .ok();
}

/// Canonical name carried in the RTCP SDES item. Constant on purpose: it only
/// has to be stable and unique per source, and this server has exactly one.
const RTCP_CNAME: &str = "screencam@sehcontrol";

fn handle_connection(stream: TcpStream, rtsp_port: u16, state: Arc<SharedState>) -> ResultType<()> {
    let mut session_id = None;
    let mut registered_sessions = Vec::new();
    let result = handle_connection_inner(
        stream,
        rtsp_port,
        state.clone(),
        &mut session_id,
        &mut registered_sessions,
    );
    finish_connection(result, &state, &registered_sessions)
}

fn finish_connection<T, E>(
    result: std::result::Result<T, E>,
    state: &SharedState,
    registered_sessions: &[Arc<Session>],
) -> std::result::Result<T, E> {
    finish_with_cleanup(result, || {
        cleanup_registered_sessions(state, registered_sessions)
    })
}

fn finish_with_cleanup<T, E>(
    result: std::result::Result<T, E>,
    cleanup: impl FnOnce(),
) -> std::result::Result<T, E> {
    cleanup();
    result
}

fn cleanup_registered_sessions(state: &SharedState, registered_sessions: &[Arc<Session>]) {
    let removed = super::take_matching_arcs(&state.sessions, |current| {
        registered_sessions
            .iter()
            .any(|registered| current.id == registered.id && Arc::ptr_eq(current, registered))
    });
    drop(removed);

    // Close every exact session owned by this connection even if stream
    // invalidation or RTP-send cleanup already removed it from the registry.
    // shutdown is idempotent and does not acquire the sessions mutex.
    for session in registered_sessions {
        session.close();
    }
}

fn configure_write_timeout(stream: &TcpStream, timeout: Duration) -> io::Result<()> {
    stream.set_write_timeout(Some(timeout))
}

fn handle_connection_inner(
    stream: TcpStream,
    rtsp_port: u16,
    state: Arc<SharedState>,
    session_id: &mut Option<String>,
    registered_sessions: &mut Vec<Arc<Session>>,
) -> ResultType<()> {
    stream.set_nodelay(true).ok();
    let peer_addr = stream.peer_addr()?;
    let shutdown_stream = Arc::new(stream.try_clone()?);
    let write_stream = stream.try_clone()?;
    configure_write_timeout(&write_stream, RTSP_WRITE_TIMEOUT)?;
    let write_half = Arc::new(Mutex::new(write_stream));
    let mut reader = BufReader::new(stream);

    let mut described_epoch: Option<u64> = None;
    let challenge = auth::Challenge::new();

    loop {
        let req = match read_request(&mut reader)? {
            Some(r) => r,
            None => break, // clean EOF
        };
        let cseq = req.headers.get("cseq").cloned().unwrap_or_default();
        log::debug!("[screencam] {} {} from {}", req.method, req.uri, peer_addr);

        // Auth gate. OPTIONS stays open on purpose: clients and NVR probes use
        // it to discover what this server supports *before* they have been
        // asked for credentials, and it reveals nothing but a method list.
        // GET_PARAMETER/TEARDOWN only act on a session the caller already
        // holds, which it could only have obtained by authenticating.
        if matches!(req.method.as_str(), "DESCRIBE" | "SETUP" | "PLAY" | "PAUSE") {
            let creds = auth::credentials();
            let authorization = req.headers.get("authorization").map(|s| s.as_str());
            if creds.is_set() && !challenge.verify(&creds, &req.method, authorization) {
                // A missing header is just the normal first half of the 401
                // handshake, not a failure worth surfacing — only a header
                // that was actually sent and didn't check out is.
                if authorization.is_some() {
                    log::warn!("[screencam] rejected RTSP credentials from {peer_addr}");
                }
                let headers = challenge.www_authenticate_headers();
                let header_refs: Vec<(&str, &str)> =
                    headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
                write_response(&write_half, "401 Unauthorized", &cseq, &header_refs, None)?;
                continue;
            }
        }

        match req.method.as_str() {
            "OPTIONS" => {
                write_response(
                    &write_half,
                    "200 OK",
                    &cseq,
                    &[(
                        "Public",
                        "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, \
                         GET_PARAMETER, SET_PARAMETER",
                    )],
                    None,
                )?;
            }
            "DESCRIBE" => {
                match build_sdp(&state, peer_addr) {
                    Some((epoch, sdp)) => {
                        // Content-Base is what the client resolves the SDP's
                        // relative `a=control:` attribute against. Without it
                        // a strict client guesses from the request URI and can
                        // produce a SETUP URL that doesn't round-trip.
                        let content_base = content_base_for(&req.uri, peer_addr, rtsp_port);
                        write_response(
                            &write_half,
                            "200 OK",
                            &cseq,
                            &[
                                ("Content-Type", "application/sdp"),
                                ("Content-Base", &content_base),
                            ],
                            Some(sdp.as_bytes()),
                        )?;
                        described_epoch = Some(epoch);
                    }
                    _ => {
                        // This epoch does not yet have dimensions, SPS, PPS
                        // and a confirmed IDR.
                        write_response(&write_half, "503 Service Unavailable", &cseq, &[], None)?;
                    }
                }
            }
            "SETUP" => {
                let transport_hdr = req.headers.get("transport").cloned().unwrap_or_default();
                match setup_transport(&transport_hdr, peer_addr, &write_half) {
                    Ok((transport, resp_transport_hdr, writer_start)) => {
                        let id = session_id
                            .get_or_insert_with(|| {
                                format!("{:016X}", hbb_common::rand::random::<u64>())
                            })
                            .clone();
                        // Keep the descriptor lock until registration is complete.
                        // invalidate_stream takes these locks in the same order, so
                        // it cannot miss a session registered for the old epoch.
                        let descriptor = state.stream_descriptor.lock().unwrap();
                        let current_epoch = descriptor.epoch;
                        if !epoch_allows_setup(described_epoch, current_epoch) {
                            drop(descriptor);
                            write_response(
                                &write_half,
                                "503 Service Unavailable",
                                &cseq,
                                &[],
                                None,
                            )?;
                            continue;
                        }
                        let registered_session = Arc::new(Session {
                            id: id.clone(),
                            transport,
                            shutdown_stream: shutdown_stream.clone(),
                            epoch: current_epoch,
                            closed: AtomicBool::new(false),
                            udp_would_block_count: AtomicU8::new(0),
                        });
                        let replaced =
                            register_session_replacing_same_id(&state, registered_session.clone());
                        drop(descriptor);
                        for replaced_session in replaced {
                            replaced_session.close();
                        }
                        registered_sessions.push(registered_session.clone());
                        // The timeout tells the NVR how often it has to send a
                        // keep-alive; without it firmwares fall back to their
                        // own default, which is sometimes longer than the idle
                        // window an intermediate firewall allows.
                        let session_hdr = format!("{id};timeout={SESSION_TIMEOUT_SECS}");
                        write_response(
                            &write_half,
                            "200 OK",
                            &cseq,
                            &[
                                ("Transport", &resp_transport_hdr),
                                ("Session", &session_hdr),
                            ],
                            None,
                        )?;
                        if let Some(writer_start) = writer_start {
                            // The SETUP response is on the wire before the writer
                            // can emit interleaved RTP on the same connection.
                            start_tcp_writer(
                                writer_start,
                                Arc::downgrade(&registered_session),
                                Arc::downgrade(&state),
                            )?;
                        }
                        set_rtsp_client_count(&state);
                    }
                    Err(e) => {
                        log::warn!("[screencam] SETUP failed: {e:?}");
                        write_response(&write_half, "461 Unsupported Transport", &cseq, &[], None)?;
                    }
                }
            }
            "PLAY" => {
                let id = session_id.clone().unwrap_or_default();
                // live555-derived clients (the bulk of the NVR market) want
                // the first sequence number and RTP timestamp up front so they
                // can prime their jitter buffer instead of waiting to infer
                // both from the stream. Omitted only when the session's epoch
                // has no published stream yet, where any value would be a lie.
                //
                // Declared before `headers`, which borrows it: locals drop in
                // reverse order, so the other way round leaves the header list
                // outliving the string it points at.
                let rtp_info = registered_sessions
                    .last()
                    .and_then(|session| state.rtp_stats.snapshot_for_epoch(session.epoch()))
                    .map(|snapshot| {
                        format!(
                            "url={};seq={};rtptime={}",
                            track_url_for(&req.uri, peer_addr, rtsp_port),
                            snapshot.next_seq,
                            snapshot.timestamp_90k,
                        )
                    });
                let mut headers: Vec<(&str, &str)> =
                    vec![("Session", id.as_str()), ("Range", "npt=0.000-")];
                if let Some(rtp_info) = rtp_info.as_deref() {
                    headers.push(("RTP-Info", rtp_info));
                }
                write_response(&write_half, "200 OK", &cseq, &headers, None)?;
            }
            // Both are keep-alive pings in practice. SET_PARAMETER is the one
            // most NVR firmwares reach for — answering 501 to it, as this
            // server used to, reads as a dead session and triggers a reconnect
            // loop. Neither carries a body we act on; read_request already
            // drained any Content-Length the client sent.
            "GET_PARAMETER" | "SET_PARAMETER" => {
                let id = session_id.clone().unwrap_or_default();
                write_response(&write_half, "200 OK", &cseq, &[("Session", &id)], None)?;
            }
            // Nothing to pause — this is a live source with no seek support,
            // so the stream simply keeps running. Answering 200 rather than
            // 501 keeps clients that PAUSE before TEARDOWN from treating the
            // teardown as a failure.
            "PAUSE" => {
                let id = session_id.clone().unwrap_or_default();
                write_response(&write_half, "200 OK", &cseq, &[("Session", &id)], None)?;
            }
            "TEARDOWN" => {
                let id = session_id.clone().unwrap_or_default();
                write_response(&write_half, "200 OK", &cseq, &[("Session", &id)], None)?;
                break;
            }
            other => {
                log::debug!("[screencam] unsupported method: {other}");
                write_response(&write_half, "501 Not Implemented", &cseq, &[], None)?;
            }
        }
    }

    Ok(())
}

fn epoch_allows_setup(described_epoch: Option<u64>, current_epoch: u64) -> bool {
    described_epoch == Some(current_epoch)
}

fn setup_transport(
    transport_hdr: &str,
    peer_addr: SocketAddr,
    write_half: &Arc<Mutex<TcpStream>>,
) -> ResultType<(Transport, String, Option<TcpWriterStart>)> {
    if transport_hdr.contains("TCP") || transport_hdr.contains("interleaved") {
        // TCP interleaved: RTP and RTCP share this same connection. Echo back
        // the channel pair the client asked for rather than forcing 0-1 —
        // clients are entitled to pick, and one that gets a different pair
        // than it requested will route our RTP to a track it isn't decoding.
        let (rtp_channel, rtcp_channel) = extract_param(transport_hdr, "interleaved=")
            .and_then(|range| parse_channel_range(&range))
            .unwrap_or((0, 1));
        let (sender, receiver) = mpsc::sync_channel(TCP_ACCESS_UNIT_QUEUE_CAPACITY);
        let transport = Transport::Tcp {
            sender,
            stream: write_half.clone(),
            rtcp_channel,
        };
        let writer_start = TcpWriterStart {
            receiver,
            writer: Box::new(TcpInterleavedWriter {
                stream: write_half.clone(),
                rtp_channel,
            }),
        };
        Ok((
            transport,
            format!("RTP/AVP/TCP;unicast;interleaved={rtp_channel}-{rtcp_channel}"),
            Some(writer_start),
        ))
    } else {
        let client_ports = extract_param(transport_hdr, "client_port=")
            .ok_or_else(|| anyhow!("missing client_port in Transport header"))?;
        let (rtp_port, rtcp_port) = parse_port_range(&client_ports)?;

        let rtp_socket = UdpSocket::bind("0.0.0.0:0")?;
        rtp_socket.connect((peer_addr.ip(), rtp_port))?;
        rtp_socket.set_nonblocking(true)?;
        let rtcp_socket = UdpSocket::bind("0.0.0.0:0")?;
        rtcp_socket.connect((peer_addr.ip(), rtcp_port))?;

        let server_rtp_port = rtp_socket.local_addr()?.port();
        let server_rtcp_port = rtcp_socket.local_addr()?.port();

        let resp = format!(
            "RTP/AVP;unicast;client_port={rtp_port}-{rtcp_port};server_port={server_rtp_port}-{server_rtcp_port}"
        );
        Ok((
            Transport::Udp {
                rtp_socket,
                rtcp_socket,
            },
            resp,
            None,
        ))
    }
}

fn parse_channel_range(s: &str) -> Option<(u8, u8)> {
    let mut parts = s.trim().split('-');
    let rtp: u8 = parts.next()?.trim().parse().ok()?;
    // RFC 2326 pairs RTP with the next channel up; a client that only names
    // one is asking for that default pair.
    let rtcp: u8 = match parts.next() {
        Some(value) => value.trim().parse().ok()?,
        None => rtp.checked_add(1)?,
    };
    Some((rtp, rtcp))
}

fn extract_param(header: &str, key: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix(key).map(|v| v.to_owned())
    })
}

fn parse_port_range(s: &str) -> ResultType<(u16, u16)> {
    let mut it = s.split('-');
    let a = it.next().ok_or_else(|| anyhow!("bad port range"))?;
    let b = it.next().unwrap_or(a);
    Ok((a.parse()?, b.parse()?))
}

/// Consumes any `$`-framed interleaved packets sitting in front of the next
/// request (RFC 2326 §10.12). On a TCP-interleaved session these are the
/// client's own RTCP receiver reports — both Dahua and Hikvision send them
/// while playing — and handing one to the line reader below would splice
/// binary data into a request line and desynchronise the connection for good.
///
/// Returns `false` at EOF. The frames themselves are discarded: this server
/// does not act on receiver reports (see module docs).
fn skip_interleaved_frames(reader: &mut BufReader<TcpStream>) -> ResultType<bool> {
    loop {
        if reader.fill_buf()?.first() != Some(&b'$') {
            // Also covers EOF, where fill_buf yields an empty slice; the
            // caller's read_line then observes it and returns cleanly.
            return Ok(true);
        }
        let mut header = [0u8; 4];
        reader.read_exact(&mut header)?;
        let len = u16::from_be_bytes([header[2], header[3]]) as u64;
        if io::copy(&mut reader.by_ref().take(len), &mut io::sink())? != len {
            return Ok(false); // truncated frame, the peer is gone
        }
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> ResultType<Option<RtspRequest>> {
    let mut request_line = String::new();
    loop {
        if !skip_interleaved_frames(reader)? {
            return Ok(None); // EOF mid-frame
        }
        request_line.clear();
        let n = reader.read_line(&mut request_line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        if !request_line.trim().is_empty() {
            break; // skip stray blank lines between requests
        }
    }
    let mut parts = request_line.trim().splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_owned();
    let uri = parts.next().unwrap_or("").to_owned();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_owned());
        }
    }

    // We don't expect/accept a body for any method we support (no ANNOUNCE),
    // but consume Content-Length if a client sends one anyway so the stream
    // stays in sync for the next request.
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
    }

    if method.is_empty() {
        bail!("empty request line");
    }
    Ok(Some(RtspRequest {
        method,
        uri,
        headers,
    }))
}

fn write_response(
    write_half: &Arc<Mutex<TcpStream>>,
    status: &str,
    cseq: &str,
    extra_headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> ResultType<()> {
    let mut resp = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n");
    for (k, v) in extra_headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        resp.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    resp.push_str("\r\n");
    let mut stream = write_half.lock().unwrap();
    stream.write_all(resp.as_bytes())?;
    if let Some(b) = body {
        stream.write_all(b)?;
    }
    Ok(())
}

/// The absolute URL a relative SDP `a=control:` attribute resolves against.
/// Built from the URI the client actually asked for whenever that is already
/// absolute, so an NVR that reached this server through a NAT/port-forward
/// gets its own address back rather than a LAN address it cannot route to.
fn content_base_for(uri: &str, peer_addr: SocketAddr, rtsp_port: u16) -> String {
    let base = if uri.len() > 7 && uri[..7].eq_ignore_ascii_case("rtsp://") {
        uri.to_owned()
    } else {
        let local_ip = local_ip_for_peer(peer_addr).unwrap_or_else(|| "0.0.0.0".to_owned());
        let path = if uri.starts_with('/') {
            uri
        } else {
            "/live/main"
        };
        format!("rtsp://{local_ip}:{rtsp_port}{path}")
    };
    if base.ends_with('/') {
        base
    } else {
        format!("{base}/")
    }
}

fn track_url_for(uri: &str, peer_addr: SocketAddr, rtsp_port: u16) -> String {
    format!(
        "{}{TRACK_CONTROL}",
        content_base_for(uri, peer_addr, rtsp_port)
    )
}

fn build_sdp(state: &SharedState, peer_addr: SocketAddr) -> Option<(u64, String)> {
    let local_ip = local_ip_for_peer(peer_addr).unwrap_or_else(|| "0.0.0.0".to_owned());
    build_sdp_for_ip(state, &local_ip)
}

fn build_sdp_for_ip(state: &SharedState, local_ip: &str) -> Option<(u64, String)> {
    let descriptor = state.stream_descriptor();
    if !descriptor.is_ready() {
        return None;
    }
    let epoch = descriptor.epoch;
    let sps = descriptor.sps?;
    let pps = descriptor.pps?;

    // RFC 6184 §8.1: profile-level-id is profile_idc / constraint flags /
    // level_idc, which start at sps[1] — sps[0] is the NAL header byte (0x67).
    // Reading from sps[0] advertised profile 0x67, which isn't a profile at
    // all; FFmpeg (ZoneMinder) silently reparses the SPS and never noticed,
    // but NVR firmwares that trust the fmtp line reject the DESCRIBE.
    let profile_level_id = if sps.len() >= 4 {
        format!("{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3])
    } else {
        "42E01E".to_owned()
    };
    let sps_b64 = base64_encode(&sps);
    let pps_b64 = base64_encode(&pps);

    Some((
        epoch,
        format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {ip}\r\n\
         s=Sehcontrol ScreenCam\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         a=tool:Sehcontrol ScreenCam\r\n\
         a=type:broadcast\r\n\
         a=range:npt=0-\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\n\
         a=fmtp:96 packetization-mode=1;profile-level-id={plid};sprop-parameter-sets={sps},{pps}\r\n\
         a=control:{track}\r\n",
        ip = local_ip,
        track = TRACK_CONTROL,
        plid = profile_level_id,
        sps = sps_b64,
        pps = pps_b64,
    ),
    ))
}

pub(super) fn local_ip_for_peer(peer_addr: SocketAddr) -> Option<String> {
    // Standard no-extra-dependency trick: a UDP "connect" doesn't send any
    // packet, it just makes the OS pick which local interface/IP would be
    // used to reach that peer — that's the IP the SDP needs to advertise.
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(peer_addr).ok()?;
    socket.local_addr().ok().map(|a| a.ip().to_string())
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    const TEST_WRITE_TIMEOUT: Duration = Duration::from_millis(250);

    fn tcp_session(
        id: &str,
    ) -> (
        Arc<Session>,
        TcpStream,
        Receiver<WriterMessage>,
        Arc<Mutex<TcpStream>>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let write_stream = server.try_clone().unwrap();
        configure_write_timeout(&write_stream, TEST_WRITE_TIMEOUT).unwrap();
        let write_stream = Arc::new(Mutex::new(write_stream));
        let (sender, receiver) = mpsc::sync_channel(TCP_ACCESS_UNIT_QUEUE_CAPACITY);
        let session = Arc::new(Session {
            id: id.to_owned(),
            transport: Transport::Tcp {
                sender,
                stream: write_stream.clone(),
                rtcp_channel: 1,
            },
            shutdown_stream: Arc::new(server.try_clone().unwrap()),
            epoch: 0,
            closed: AtomicBool::new(false),
            udp_would_block_count: AtomicU8::new(0),
        });
        (session, peer, receiver, write_stream)
    }

    fn access_unit(packets: &[&[u8]]) -> Arc<RtpAccessUnit> {
        Arc::new(RtpAccessUnit::new(
            packets.iter().map(|packet| packet.to_vec()).collect(),
        ))
    }

    fn queued_access_unit(message: WriterMessage) -> Arc<RtpAccessUnit> {
        match message {
            WriterMessage::AccessUnit { access_unit, .. } => access_unit,
            WriterMessage::Close => panic!("expected an access unit"),
        }
    }

    fn start_test_writer(
        state: &Arc<SharedState>,
        session: &Arc<Session>,
        receiver: Receiver<WriterMessage>,
        stream: Arc<Mutex<TcpStream>>,
    ) -> thread::JoinHandle<()> {
        start_test_writer_with(
            state,
            session,
            receiver,
            Box::new(TcpInterleavedWriter {
                stream,
                rtp_channel: 0,
            }),
        )
    }

    fn start_test_writer_with(
        state: &Arc<SharedState>,
        session: &Arc<Session>,
        receiver: Receiver<WriterMessage>,
        writer: Box<dyn AccessUnitWriter>,
    ) -> thread::JoinHandle<()> {
        start_tcp_writer(
            TcpWriterStart { receiver, writer },
            Arc::downgrade(session),
            Arc::downgrade(state),
        )
        .unwrap()
    }

    struct ErrorWriter {
        kind: io::ErrorKind,
    }

    impl AccessUnitWriter for ErrorWriter {
        fn write_access_unit(
            &mut self,
            _access_unit: &RtpAccessUnit,
            _should_continue: &mut dyn FnMut() -> bool,
        ) -> io::Result<()> {
            Err(io::Error::new(self.kind, "injected writer failure"))
        }
    }

    struct GatedErrorWriter {
        kind: io::ErrorKind,
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl AccessUnitWriter for GatedErrorWriter {
        fn write_access_unit(
            &mut self,
            _access_unit: &RtpAccessUnit,
            _should_continue: &mut dyn FnMut() -> bool,
        ) -> io::Result<()> {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            Err(io::Error::new(self.kind, "injected writer failure"))
        }
    }

    struct PausingPacketWriter {
        written: mpsc::Sender<Vec<u8>>,
        first_written: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    }

    impl AccessUnitWriter for PausingPacketWriter {
        fn write_access_unit(
            &mut self,
            access_unit: &RtpAccessUnit,
            should_continue: &mut dyn FnMut() -> bool,
        ) -> io::Result<()> {
            for (index, packet) in access_unit.packets.iter().enumerate() {
                if !should_continue() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "injected epoch change",
                    ));
                }
                self.written.send(packet.clone()).unwrap();
                if index == 0 {
                    self.first_written.send(()).unwrap();
                    self.resume.recv().unwrap();
                }
            }
            Ok(())
        }
    }

    struct DropNotifyingWriter {
        calls: Arc<AtomicUsize>,
        dropped: Option<mpsc::Sender<()>>,
    }

    impl AccessUnitWriter for DropNotifyingWriter {
        fn write_access_unit(
            &mut self,
            _access_unit: &RtpAccessUnit,
            _should_continue: &mut dyn FnMut() -> bool,
        ) -> io::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl Drop for DropNotifyingWriter {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    fn early_read_failure() -> std::result::Result<(), &'static str> {
        std::result::Result::<(), _>::Err("read failed")?;
        Ok(())
    }

    #[test]
    fn setup_accepts_only_the_epoch_returned_by_current_describe() {
        assert!(epoch_allows_setup(Some(7), 7));
        assert!(!epoch_allows_setup(Some(6), 7));
        assert!(!epoch_allows_setup(None, 7));
    }

    #[test]
    fn old_describe_is_rejected_after_stream_invalidation() {
        let state = SharedState::new();
        assert!(state.set_stream_dimensions(0, 1920, 1080));
        assert!(state.apply_stream_access_unit(
            0,
            &[&[0x67, 0x64, 0x00, 0x1f], &[0x68, 0xee], &[0x65, 0x88]]
        ));
        let (described_epoch, old_sdp) =
            build_sdp_for_ip(&state, "127.0.0.1").expect("current stream must describe");
        assert!(old_sdp.contains("sprop-parameter-sets="));

        assert_eq!(state.invalidate_stream(), described_epoch + 1);
        assert!(!epoch_allows_setup(
            Some(described_epoch),
            state.stream_epoch()
        ));
        assert!(build_sdp_for_ip(&state, "127.0.0.1").is_none());

        let new_epoch = state.stream_epoch();
        assert!(state.set_stream_dimensions(new_epoch, 2560, 1440));
        assert!(state.apply_stream_access_unit(
            new_epoch,
            &[&[0x67, 0x64, 0x00, 0x20], &[0x68, 0xef], &[0x65, 0x99]]
        ));
        let (new_described_epoch, new_sdp) =
            build_sdp_for_ip(&state, "127.0.0.1").expect("new stream must describe");

        assert_eq!(new_described_epoch, new_epoch);
        assert!(epoch_allows_setup(
            Some(new_described_epoch),
            state.stream_epoch()
        ));
        assert_ne!(old_sdp, new_sdp);
    }

    /// SPS bytes here are `67 64 00 1F`: NAL header, then profile_idc=0x64
    /// (High), constraint flags 0x00, level_idc=0x1F (level 3.1).
    #[test]
    fn profile_level_id_skips_the_nal_header_byte() {
        let state = SharedState::new();
        assert!(state.set_stream_dimensions(0, 1920, 1080));
        assert!(state.apply_stream_access_unit(
            0,
            &[&[0x67, 0x64, 0x00, 0x1f], &[0x68, 0xee], &[0x65, 0x88]]
        ));

        let (_, sdp) = build_sdp_for_ip(&state, "127.0.0.1").expect("describes");

        assert!(
            sdp.contains("profile-level-id=64001F"),
            "must report profile_idc/constraints/level_idc, got: {sdp}"
        );
        assert!(
            !sdp.contains("profile-level-id=676400"),
            "reading from the NAL header advertised a profile that doesn't exist"
        );
    }

    #[test]
    fn a_truncated_sps_falls_back_to_a_valid_baseline_profile() {
        let state = SharedState::new();
        assert!(state.set_stream_dimensions(0, 640, 480));
        // Three bytes: enough for the descriptor, one short of a level_idc.
        assert!(
            state.apply_stream_access_unit(0, &[&[0x67, 0x64, 0x00], &[0x68, 0xee], &[0x65, 0x88]])
        );

        let (_, sdp) = build_sdp_for_ip(&state, "127.0.0.1").expect("describes");

        assert!(sdp.contains("profile-level-id=42E01E"), "got: {sdp}");
    }

    #[test]
    fn sdp_control_attributes_match_what_setup_urls_are_built_from() {
        let state = SharedState::new();
        assert!(state.set_stream_dimensions(0, 1280, 720));
        assert!(state.apply_stream_access_unit(
            0,
            &[&[0x67, 0x42, 0xc0, 0x1e], &[0x68, 0xee], &[0x65, 0x88]]
        ));

        let (_, sdp) = build_sdp_for_ip(&state, "10.0.0.5").expect("describes");

        assert!(sdp.contains("a=control:*\r\n"), "session-level control");
        assert!(
            sdp.contains(&format!("a=control:{TRACK_CONTROL}\r\n")),
            "media-level control must be the track the SETUP URL names"
        );
        assert!(sdp.contains("a=range:npt=0-\r\n"), "live source range");
    }

    #[test]
    fn content_base_echoes_an_absolute_request_uri_so_nat_survives() {
        let peer = "203.0.113.9:5000".parse().unwrap();

        // A client that reached us through a port-forward has to get its own
        // address back, not whichever interface answered.
        assert_eq!(
            content_base_for("rtsp://public.example:8554/live/main", peer, 554),
            "rtsp://public.example:8554/live/main/"
        );
        // Already-terminated bases are not doubled up.
        assert_eq!(
            content_base_for("rtsp://public.example/live/main/", peer, 554),
            "rtsp://public.example/live/main/"
        );
        assert_eq!(
            track_url_for("rtsp://public.example/live/main", peer, 554),
            format!("rtsp://public.example/live/main/{TRACK_CONTROL}")
        );
    }

    #[test]
    fn content_base_reconstructs_an_absolute_url_from_a_bare_path() {
        let peer = "127.0.0.1:5000".parse().unwrap();

        // Dahua appends its own query string; it stays part of the base, which
        // is what the client will echo back on SETUP.
        let base = content_base_for("/cam/realmonitor?channel=1&subtype=0", peer, 554);
        assert!(
            base.starts_with("rtsp://127.0.0.1:554/cam/realmonitor?"),
            "got: {base}"
        );
        assert!(base.ends_with('/'));

        // `OPTIONS *` and other non-path URIs fall back to the served route
        // rather than producing a malformed base.
        let base = content_base_for("*", peer, 554);
        assert_eq!(base, "rtsp://127.0.0.1:554/live/main/");
    }

    #[test]
    fn interleaved_channels_are_echoed_back_as_the_client_asked() {
        assert_eq!(parse_channel_range("0-1"), Some((0, 1)));
        assert_eq!(parse_channel_range("2-3"), Some((2, 3)));
        assert_eq!(parse_channel_range(" 4 - 5 "), Some((4, 5)));
        // RFC 2326 pairs RTP with the next channel up.
        assert_eq!(parse_channel_range("6"), Some((6, 7)));
        assert_eq!(parse_channel_range(""), None);
        assert_eq!(parse_channel_range("a-b"), None);
        assert_eq!(parse_channel_range("300-301"), None);
        // No channel left for RTCP; fall back rather than wrap around to 0.
        assert_eq!(parse_channel_range("255"), None);
    }

    fn reader_over(bytes: &[u8]) -> BufReader<TcpStream> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        peer.write_all(bytes).unwrap();
        peer.shutdown(Shutdown::Write).unwrap();
        BufReader::new(server)
    }

    fn interleaved_frame(channel: u8, payload: &[u8]) -> Vec<u8> {
        let len = (payload.len() as u16).to_be_bytes();
        let mut frame = vec![b'$', channel, len[0], len[1]];
        frame.extend_from_slice(payload);
        frame
    }

    /// The regression this exists for: an NVR sends RTCP receiver reports back
    /// on the same socket it keeps sending RTSP on. Handing one of those to the
    /// line reader spliced binary data into a request line and desynchronised
    /// the connection permanently.
    #[test]
    fn a_receiver_report_before_a_request_does_not_corrupt_it() {
        // A payload containing both a newline and something that looks like a
        // request line — exactly what used to be mistaken for one.
        let mut wire = interleaved_frame(1, b"\x81\xc9\x00\x07GET_PARAMETER * RTSP/1.0\r\n");
        wire.extend_from_slice(b"OPTIONS rtsp://host/live/main RTSP/1.0\r\nCSeq: 4\r\n\r\n");
        let mut reader = reader_over(&wire);

        let request = read_request(&mut reader).unwrap().expect("request");

        assert_eq!(request.method, "OPTIONS");
        assert_eq!(request.uri, "rtsp://host/live/main");
        assert_eq!(request.headers.get("cseq").map(String::as_str), Some("4"));
    }

    #[test]
    fn several_queued_frames_are_all_skipped() {
        let mut wire = interleaved_frame(0, &[0xAA; 1400]);
        wire.extend_from_slice(&interleaved_frame(1, &[0xBB; 60]));
        wire.extend_from_slice(&interleaved_frame(1, &[]));
        wire.extend_from_slice(b"TEARDOWN rtsp://host/live/main RTSP/1.0\r\nCSeq: 9\r\n\r\n");
        let mut reader = reader_over(&wire);

        let request = read_request(&mut reader).unwrap().expect("request");

        assert_eq!(request.method, "TEARDOWN");
        assert_eq!(request.headers.get("cseq").map(String::as_str), Some("9"));
    }

    #[test]
    fn a_truncated_frame_ends_the_connection_instead_of_being_parsed() {
        // Header promises 100 bytes, only 4 arrive before the peer goes away.
        let mut wire = vec![b'$', 1, 0x00, 0x64];
        wire.extend_from_slice(&[0xCC; 4]);
        let mut reader = reader_over(&wire);

        // Reported as a finished connection, not as a request built out of
        // whatever the partial frame happened to contain.
        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn a_clean_eof_is_still_reported_as_a_finished_connection() {
        let mut reader = reader_over(b"");

        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn invalidation_removes_old_sdp_until_the_new_epoch_is_ready() {
        let state = SharedState::new();
        assert!(state.set_stream_dimensions(0, 1920, 1080));
        assert!(state.apply_stream_access_unit(
            0,
            &[&[0x67, 0x64, 0x00, 0x1f], &[0x68, 0xee], &[0x65, 0x88]]
        ));
        assert!(state.stream_descriptor().is_ready());

        state.invalidate_stream();

        let descriptor = state.stream_descriptor();
        assert_eq!(descriptor.epoch, 1);
        assert!(!descriptor.is_ready());
        assert_eq!(descriptor.sps, None);
        assert_eq!(descriptor.pps, None);
        assert!(!descriptor.idr_ready);
    }

    #[test]
    fn cleanup_runs_and_preserves_the_original_early_error() {
        let state = SharedState::new();
        let (session, _peer, _receiver, _stream) = tcp_session("registered");
        state.sessions.lock().unwrap().push(session.clone());

        let result = finish_connection(early_read_failure(), &state, &[session.clone()]);

        assert_eq!(result, Err("read failed"));
        assert!(state.sessions.lock().unwrap().is_empty());
        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[1]]))
            .is_err());
    }

    #[test]
    fn old_connection_cleanup_preserves_a_replacement_with_the_same_id() {
        let state = SharedState::new();
        let (old, _old_peer, _old_receiver, _old_stream) = tcp_session("same-id");
        let (replacement, _replacement_peer, replacement_receiver, _replacement_stream) =
            tcp_session("same-id");
        state
            .sessions
            .lock()
            .unwrap()
            .extend([old.clone(), replacement.clone()]);

        let first = finish_connection(Ok::<_, ()>(()), &state, &[old.clone()]);

        assert_eq!(first, Ok(()));
        let remaining = state.sessions.lock().unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(Arc::ptr_eq(&remaining[0], &replacement));
        drop(remaining);
        assert!(old.dispatch_access_unit(0, access_unit(&[&[1]])).is_err());
        let replacement_access_unit = access_unit(&[&[1]]);
        assert!(replacement
            .dispatch_access_unit(0, replacement_access_unit.clone())
            .is_ok());
        assert!(Arc::ptr_eq(
            &queued_access_unit(replacement_receiver.try_recv().unwrap()),
            &replacement_access_unit
        ));

        let second = finish_connection(Ok::<_, ()>(()), &state, &[old]);
        assert_eq!(second, Ok(()));
        let remaining = state.sessions.lock().unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(Arc::ptr_eq(&remaining[0], &replacement));
        drop(remaining);
        replacement.close();
    }

    #[test]
    fn repeated_setup_replaces_and_closes_the_old_writer_session() {
        let state = Arc::new(SharedState::new());
        let (old, _old_peer, old_receiver, old_stream) = tcp_session("same-id");
        assert!(register_session_replacing_same_id(&state, old.clone()).is_empty());
        let old_writer = start_test_writer(&state, &old, old_receiver, old_stream);
        let (replacement, _new_peer, replacement_receiver, _new_stream) = tcp_session("same-id");

        let replaced = register_session_replacing_same_id(&state, replacement.clone());
        assert_eq!(replaced.len(), 1);
        assert!(Arc::ptr_eq(&replaced[0], &old));
        for session in replaced {
            session.close();
        }
        old_writer.join().unwrap();

        let registered = state.sessions.lock().unwrap();
        assert_eq!(registered.len(), 1);
        assert!(Arc::ptr_eq(&registered[0], &replacement));
        drop(registered);
        assert!(old.dispatch_access_unit(0, access_unit(&[&[1]])).is_err());

        assert_eq!(
            finish_connection(Ok::<_, ()>(()), &state, &[old.clone()]),
            Ok(())
        );
        assert_eq!(finish_connection(Ok::<_, ()>(()), &state, &[old]), Ok(()));
        let registered = state.sessions.lock().unwrap();
        assert_eq!(registered.len(), 1);
        assert!(Arc::ptr_eq(&registered[0], &replacement));
        drop(registered);

        let batch = access_unit(&[&[2]]);
        assert!(replacement.dispatch_access_unit(0, batch.clone()).is_ok());
        assert!(Arc::ptr_eq(
            &queued_access_unit(replacement_receiver.try_recv().unwrap()),
            &batch
        ));
        replacement.close();
    }

    #[test]
    fn tcp_write_timeout_is_configured() {
        let (session, _peer, _receiver, stream) = tcp_session("timeout");
        let timeout = stream.lock().unwrap().write_timeout().unwrap();

        assert_eq!(timeout, Some(TEST_WRITE_TIMEOUT));
        session.close();
    }

    #[test]
    fn close_does_not_wait_for_the_tcp_write_mutex() {
        let (session, _peer, _receiver, write_stream) = tcp_session("interrupt");
        let write_guard = write_stream.lock().unwrap();
        let (closed_tx, closed_rx) = mpsc::channel();
        let closing_session = session.clone();
        let closer = thread::spawn(move || {
            closing_session.close();
            closed_tx.send(()).unwrap();
        });

        closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown must not wait for the write mutex");
        drop(write_guard);
        closer.join().unwrap();
        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[1]]))
            .is_err());
    }

    #[test]
    fn disconnected_tcp_queue_propagates_and_removes_the_failed_session() {
        let state = SharedState::new();
        let (session, _peer, receiver, _stream) = tcp_session("failed");
        state.sessions.lock().unwrap().push(session.clone());
        drop(receiver);
        let access_unit = access_unit(&[&[1, 2, 3]]);

        let failed = super::super::dispatch_access_unit_to_sessions(
            vec![session.clone()],
            access_unit,
            |session, access_unit| session.dispatch_access_unit(0, access_unit),
        );
        let removed =
            super::super::take_failed_session_instances(&state.sessions, &failed, |session| {
                session.id.as_str()
            });

        assert_eq!(failed.len(), 1);
        assert_eq!(removed.len(), 1);
        assert!(Arc::ptr_eq(&removed[0], &session));
        assert!(state.sessions.lock().unwrap().is_empty());
        for session in removed {
            session.close();
        }
    }

    #[test]
    fn sender_drop_terminates_a_waiting_writer_without_processing_messages() {
        let state = Arc::new(SharedState::new());
        let (session, _peer, receiver, _stream) = tcp_session("sender-drop");
        let calls = Arc::new(AtomicUsize::new(0));
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let writer = start_test_writer_with(
            &state,
            &session,
            receiver,
            Box::new(DropNotifyingWriter {
                calls: calls.clone(),
                dropped: Some(dropped_tx),
            }),
        );

        drop(session);
        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("dropping the last sender must terminate the writer");
        writer.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn close_discards_queued_messages_and_terminates_the_writer() {
        let state = Arc::new(SharedState::new());
        let (session, _peer, receiver, _stream) = tcp_session("closed");
        state.sessions.lock().unwrap().push(session.clone());
        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[1, 2, 3]]))
            .is_ok());
        session.close();
        let calls = Arc::new(AtomicUsize::new(0));
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let writer = start_test_writer_with(
            &state,
            &session,
            receiver,
            Box::new(DropNotifyingWriter {
                calls: calls.clone(),
                dropped: Some(dropped_tx),
            }),
        );

        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("close must terminate the writer");
        writer.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[4]]))
            .is_err());
    }

    #[test]
    fn active_writer_client_disconnect_error_removes_and_closes_the_session() {
        let state = Arc::new(SharedState::new());
        let (session, _peer, receiver, _stream) = tcp_session("disconnected-client");
        state.sessions.lock().unwrap().push(session.clone());
        let writer = start_test_writer_with(
            &state,
            &session,
            receiver,
            Box::new(ErrorWriter {
                kind: io::ErrorKind::ConnectionReset,
            }),
        );

        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[1]]))
            .is_ok());
        writer.join().unwrap();

        assert!(state.sessions.lock().unwrap().is_empty());
        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[2]]))
            .is_err());
    }

    #[test]
    fn bounded_queue_is_nonblocking_and_counts_whole_access_units() {
        let (session, _peer, receiver, _stream) = tcp_session("bounded");
        let first = access_unit(&[&[1], &[2]]);
        let second = access_unit(&[&[3]]);
        let third = access_unit(&[&[4]]);

        assert!(session.dispatch_access_unit(0, first.clone()).is_ok());
        assert!(session.dispatch_access_unit(0, second.clone()).is_ok());
        let started = Instant::now();
        let error = session
            .dispatch_access_unit(0, third)
            .expect_err("the third access unit must exceed capacity");

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < Duration::from_millis(50));
        assert!(Arc::ptr_eq(
            &queued_access_unit(receiver.try_recv().unwrap()),
            &first
        ));
        assert!(Arc::ptr_eq(
            &queued_access_unit(receiver.try_recv().unwrap()),
            &second
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn dispatch_shares_one_batch_between_tcp_sessions_without_payload_copies() {
        let (first, _first_peer, first_receiver, _first_stream) = tcp_session("first");
        let (second, _second_peer, second_receiver, _second_stream) = tcp_session("second");
        let batch = access_unit(&[&[1, 2, 3], &[4, 5]]);

        let failed = super::super::dispatch_access_unit_to_sessions(
            vec![first, second],
            batch.clone(),
            |session, access_unit| session.dispatch_access_unit(0, access_unit),
        );

        assert!(failed.is_empty());
        let first_batch = queued_access_unit(first_receiver.try_recv().unwrap());
        let second_batch = queued_access_unit(second_receiver.try_recv().unwrap());
        assert!(Arc::ptr_eq(&first_batch, &batch));
        assert!(Arc::ptr_eq(&second_batch, &batch));
        assert!(Arc::ptr_eq(&first_batch, &second_batch));
    }

    #[test]
    fn full_tcp_session_does_not_delay_healthy_session_or_later_access_units() {
        let state = SharedState::new();
        let (slow, _slow_peer, _slow_receiver, _slow_stream) = tcp_session("slow");
        let (healthy, _healthy_peer, healthy_receiver, _healthy_stream) = tcp_session("healthy");
        state
            .sessions
            .lock()
            .unwrap()
            .extend([slow.clone(), healthy.clone()]);
        assert!(slow.dispatch_access_unit(0, access_unit(&[&[0]])).is_ok());
        assert!(slow.dispatch_access_unit(0, access_unit(&[&[1]])).is_ok());

        let current = access_unit(&[&[2]]);
        let started = Instant::now();
        let failed = super::super::dispatch_access_unit_to_sessions(
            vec![slow.clone(), healthy.clone()],
            current.clone(),
            |session, access_unit| session.dispatch_access_unit(0, access_unit),
        );
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(failed.len(), 1);
        assert!(Arc::ptr_eq(&failed[0].0, &slow));
        let removed =
            super::super::take_failed_session_instances(&state.sessions, &failed, |session| {
                session.id.as_str()
            });
        for session in removed {
            session.close();
        }
        assert!(Arc::ptr_eq(
            &queued_access_unit(healthy_receiver.try_recv().unwrap()),
            &current
        ));

        let next = access_unit(&[&[3]]);
        let failed = super::super::dispatch_access_unit_to_sessions(
            vec![healthy],
            next.clone(),
            |session, access_unit| session.dispatch_access_unit(0, access_unit),
        );
        assert!(failed.is_empty());
        assert!(Arc::ptr_eq(
            &queued_access_unit(healthy_receiver.try_recv().unwrap()),
            &next
        ));
    }

    #[test]
    fn production_access_unit_path_continues_after_a_slow_tcp_session() {
        let state = SharedState::new();
        let (slow, _slow_peer, _slow_receiver, _slow_stream) = tcp_session("slow");
        let (healthy, _healthy_peer, healthy_receiver, _healthy_stream) = tcp_session("healthy");
        state
            .sessions
            .lock()
            .unwrap()
            .extend([slow.clone(), healthy]);
        assert!(slow.dispatch_access_unit(0, access_unit(&[&[0]])).is_ok());
        assert!(slow.dispatch_access_unit(0, access_unit(&[&[1]])).is_ok());
        let mut payloader = super::super::rtp::H264Payloader::new();

        super::super::handle_access_unit(
            &state,
            &mut payloader,
            &[
                0, 0, 0, 1, 0x67, 0x64, 0, 0x1f, 0, 0, 1, 0x68, 0xee, 0, 0, 1, 0x65, 0x88,
            ],
            0,
            0,     // pts_ms: preview no participa en este test
            false, // keyframe
            Duration::from_millis(1),
            1200,
        );
        assert_eq!(state.sessions.lock().unwrap().len(), 1);
        let first = queued_access_unit(healthy_receiver.try_recv().unwrap());
        assert!(!first.packets.is_empty());

        super::super::handle_access_unit(
            &state,
            &mut payloader,
            &[0, 0, 1, 0x61, 0x20],
            0,
            0,     // pts_ms: preview no participa en este test
            false, // keyframe
            Duration::from_millis(2),
            1200,
        );
        let second = queued_access_unit(healthy_receiver.try_recv().unwrap());
        assert!(!second.packets.is_empty());
    }

    #[test]
    fn tcp_writer_preserves_packet_order_within_an_access_unit() {
        let state = Arc::new(SharedState::new());
        let (session, mut peer, receiver, stream) = tcp_session("ordered");
        state.sessions.lock().unwrap().push(session.clone());
        let writer = start_test_writer(&state, &session, receiver, stream);
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[1, 2], &[3, 4, 5]]))
            .is_ok());
        let mut received = [0u8; 13];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(received, [b'$', 0, 0, 2, 1, 2, b'$', 0, 0, 3, 3, 4, 5]);

        session.close();
        writer.join().unwrap();
    }

    #[test]
    fn stale_epoch_is_not_written_and_invalidation_wakes_writer() {
        let state = Arc::new(SharedState::new());
        let (session, mut peer, receiver, stream) = tcp_session("stale");
        state.sessions.lock().unwrap().push(session.clone());
        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[1, 2, 3]]))
            .is_ok());
        assert_eq!(state.invalidate_stream(), 1);

        let started = Instant::now();
        let writer = start_test_writer(&state, &session, receiver, stream);
        writer.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut byte = [0u8; 1];
        match peer.read(&mut byte) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::ConnectionReset
                ) => {}
            other => panic!("stale access unit reached the client: {other:?}"),
        }
    }

    #[test]
    fn invalidation_during_access_unit_stops_remaining_packets() {
        let state = Arc::new(SharedState::new());
        let (session, _peer, receiver, _stream) = tcp_session("mid-epoch");
        state.sessions.lock().unwrap().push(session.clone());
        let (written_tx, written_rx) = mpsc::channel();
        let (first_tx, first_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let writer = start_test_writer_with(
            &state,
            &session,
            receiver,
            Box::new(PausingPacketWriter {
                written: written_tx,
                first_written: first_tx,
                resume: resume_rx,
            }),
        );

        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[1], &[2], &[3]]))
            .is_ok());
        first_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer must process the first packet");
        assert_eq!(state.invalidate_stream(), 1);
        resume_tx.send(()).unwrap();
        writer.join().unwrap();

        let written = written_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(written, vec![vec![1]]);
        assert!(state.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn disabled_cleanup_does_not_wait_for_a_writer_blocked_on_tcp_serialization() {
        let state = Arc::new(SharedState::new());
        let (session, _peer, receiver, stream) = tcp_session("blocked");
        state.sessions.lock().unwrap().push(session.clone());
        let stream_guard = stream.lock().unwrap();
        let writer = start_test_writer(&state, &session, receiver, stream.clone());
        assert!(session
            .dispatch_access_unit(0, access_unit(&[&[1, 2, 3]]))
            .is_ok());
        thread::sleep(Duration::from_millis(10));

        let started = Instant::now();
        assert_eq!(
            super::super::watchdog_disposition(super::super::CaptureExit::Disabled),
            super::super::WatchdogDisposition::RemainDisabled
        );
        assert_eq!(state.invalidate_stream(), 1);
        assert!(started.elapsed() < Duration::from_millis(50));
        drop(stream_guard);
        writer.join().unwrap();
    }

    #[test]
    fn writer_timeout_cleanup_preserves_same_id_replacement_and_is_idempotent() {
        let state = Arc::new(SharedState::new());
        let (old, _old_peer, old_receiver, _old_stream) = tcp_session("same-id");
        assert!(register_session_replacing_same_id(&state, old.clone()).is_empty());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let old_writer = start_test_writer_with(
            &state,
            &old,
            old_receiver,
            Box::new(GatedErrorWriter {
                kind: io::ErrorKind::TimedOut,
                started: started_tx,
                release: release_rx,
            }),
        );
        assert!(old.dispatch_access_unit(0, access_unit(&[&[1]])).is_ok());
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("old writer must enter the injected write");

        let (replacement, _new_peer, replacement_receiver, _new_stream) = tcp_session("same-id");
        let replaced = register_session_replacing_same_id(&state, replacement.clone());
        assert_eq!(replaced.len(), 1);
        assert!(Arc::ptr_eq(&replaced[0], &old));
        for session in replaced {
            session.close();
        }
        release_tx.send(()).unwrap();
        old_writer.join().unwrap();

        let remaining = state.sessions.lock().unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(Arc::ptr_eq(&remaining[0], &replacement));
        drop(remaining);
        assert!(old.dispatch_access_unit(0, access_unit(&[&[1]])).is_err());
        assert_eq!(
            finish_connection(Ok::<_, ()>(()), &state, &[old.clone()]),
            Ok(())
        );
        assert_eq!(finish_connection(Ok::<_, ()>(()), &state, &[old]), Ok(()));
        let replacement_batch = access_unit(&[&[2]]);
        assert!(replacement
            .dispatch_access_unit(0, replacement_batch.clone())
            .is_ok());
        assert!(Arc::ptr_eq(
            &queued_access_unit(replacement_receiver.try_recv().unwrap()),
            &replacement_batch
        ));
        replacement.close();
    }

    #[test]
    fn udp_would_block_and_errors_return_immediately() {
        let batch = RtpAccessUnit::new(vec![vec![1], vec![2]]);
        let started = Instant::now();
        let would_block = send_udp_access_unit(&batch, |_| {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
        })
        .unwrap_err();
        assert_eq!(would_block.kind(), io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < Duration::from_millis(50));

        let mut calls = 0;
        let error = send_udp_access_unit(&batch, |_| {
            calls += 1;
            if calls == 2 {
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "down"))
            } else {
                Ok(1)
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(calls, 2);
    }

    #[test]
    fn udp_would_block_requires_three_consecutive_access_units() {
        let (session, _peer, _receiver, _stream) = tcp_session("udp-pressure");
        let batch = RtpAccessUnit::new(vec![vec![1]]);
        let would_block = || Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"));

        assert!(session
            .dispatch_udp_access_unit_with(&batch, |_| would_block())
            .is_ok());
        assert!(session
            .dispatch_udp_access_unit_with(&batch, |_| would_block())
            .is_ok());
        let error = session
            .dispatch_udp_access_unit_with(&batch, |_| would_block())
            .expect_err("the third consecutive WouldBlock must retire the session");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn successful_udp_access_unit_resets_would_block_counter() {
        let (session, _peer, _receiver, _stream) = tcp_session("udp-reset");
        let batch = RtpAccessUnit::new(vec![vec![1]]);

        for _ in 0..2 {
            assert!(session
                .dispatch_udp_access_unit_with(&batch, |_| {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
                })
                .is_ok());
        }
        assert!(session
            .dispatch_udp_access_unit_with(&batch, |packet| Ok(packet.len()))
            .is_ok());
        for _ in 0..2 {
            assert!(session
                .dispatch_udp_access_unit_with(&batch, |_| {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
                })
                .is_ok());
        }
    }

    #[test]
    fn replacement_does_not_inherit_udp_would_block_counter() {
        let state = SharedState::new();
        let (old, _old_peer, _old_receiver, _old_stream) = tcp_session("same-id");
        let batch = RtpAccessUnit::new(vec![vec![1]]);
        for _ in 0..2 {
            assert!(old
                .dispatch_udp_access_unit_with(&batch, |_| {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
                })
                .is_ok());
        }
        assert!(register_session_replacing_same_id(&state, old.clone()).is_empty());
        let (replacement, _new_peer, _new_receiver, _new_stream) = tcp_session("same-id");
        let replaced = register_session_replacing_same_id(&state, replacement.clone());
        for session in replaced {
            session.close();
        }

        for _ in 0..2 {
            assert!(replacement
                .dispatch_udp_access_unit_with(&batch, |_| {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
                })
                .is_ok());
        }
        assert_eq!(state.sessions.lock().unwrap().len(), 1);
        replacement.close();
    }

    #[test]
    fn third_udp_would_block_removes_only_slow_session_and_healthy_continues() {
        let state = SharedState::new();
        let (slow, _slow_peer, _slow_receiver, _slow_stream) = tcp_session("slow-udp");
        let (healthy, _healthy_peer, _healthy_receiver, _healthy_stream) =
            tcp_session("healthy-udp");
        state
            .sessions
            .lock()
            .unwrap()
            .extend([slow.clone(), healthy.clone()]);
        let batch = access_unit(&[&[1], &[2]]);
        let mut healthy_packets = 0usize;

        for attempt in 1..=3 {
            let failed = super::super::dispatch_access_unit_to_sessions(
                vec![slow.clone(), healthy.clone()],
                batch.clone(),
                |session, access_unit| {
                    if std::ptr::eq(session, slow.as_ref()) {
                        session.dispatch_udp_access_unit_with(&access_unit, |_| {
                            Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
                        })
                    } else {
                        session.dispatch_udp_access_unit_with(&access_unit, |packet| {
                            healthy_packets += 1;
                            Ok(packet.len())
                        })
                    }
                },
            );
            if attempt < UDP_WOULD_BLOCK_LIMIT {
                assert!(failed.is_empty());
            } else {
                assert_eq!(failed.len(), 1);
                assert!(Arc::ptr_eq(&failed[0].0, &slow));
                let removed = super::super::take_failed_session_instances(
                    &state.sessions,
                    &failed,
                    |session| session.id.as_str(),
                );
                assert_eq!(removed.len(), 1);
                assert!(Arc::ptr_eq(&removed[0], &slow));
                for session in removed {
                    session.close();
                }
            }
        }

        assert_eq!(healthy_packets, 6);
        let remaining = state.sessions.lock().unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(Arc::ptr_eq(&remaining[0], &healthy));
        drop(remaining);
        healthy.close();
    }
}
