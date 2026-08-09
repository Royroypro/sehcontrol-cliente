use std::io::ErrorKind;
use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use log::info;
use ConnectError::*;
use ConnectState::*;
use ConnectionResult::*;

use crate::{
    connection::Connection, packet::*, protocol::handshake::Handshake, settings::ConnInitSettings,
};

use super::{
    hsv5::{start_hsv5_initiation, StartedInitiator},
    ConnectError, ConnectionReject, ConnectionResult,
};

/// **Sehcontrol patch.** How long an unanswered handshake request is left alone
/// before it is sent again.
///
/// Upstream had no such delay: `handle_tick` resent on every tick, so the
/// cadence was whatever timer the driver happened to use — and, worse, a state
/// change triggered by an *incoming packet* did not reset it. srt-tokio ticks
/// every 100 ms, so a conclusion sent on packet arrival could be retransmitted
/// as little as a few milliseconds later. MediaMTX treats each conclusion as a
/// new publisher for the path, so it logged `opened` / `closing existing
/// publisher` in a loop and tore down the connection the caller had just
/// accepted, surfacing as `Started` immediately followed by `SendFailed`.
///
/// The value matches libsrt, which is what MediaMTX's peers are built against.
/// The SRT internet-draft (draft-sharabayko-srt) deliberately leaves
/// retransmission timing to the implementation, and libsrt resends an
/// unanswered request every 250 ms — independently documented by Open Broadcast
/// Systems, who wrote a second implementation for bug-for-bug compatibility:
/// "The outbound handshake could also be lost and a second one sent 250ms
/// later."
///
/// It is also comfortably above the handshake round trip measured against the
/// production server, where the induction response came back well inside
/// 100 ms: a retransmit therefore means a genuinely lost datagram rather than
/// impatience, and recovery still happens many times over inside the connect
/// timeout.
const HANDSHAKE_RETRANSMIT_INTERVAL: Duration = Duration::from_millis(250);

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
enum ConnectState {
    Configured,
    /// keep induction packet around for retransmit
    InductionResponseWait(Packet),
    /// keep conclusion packet around for retransmit
    ConclusionResponseWait(Packet, StartedInitiator),
}

impl Default for ConnectState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectState {
    pub fn new() -> ConnectState {
        Configured
    }
}

pub struct Connect {
    remote: SocketAddr,
    local_addr: IpAddr,
    init_settings: ConnInitSettings,
    state: ConnectState,
    streamid: Option<String>,
    starting_send_seqnum: SeqNumber,
    /// When the request currently held in `state` was last put on the wire.
    ///
    /// Tied to the packet, not to the driver's timer: every path that actually
    /// sends updates it, including the induction -> conclusion transition, which
    /// is driven by an incoming packet rather than by a tick. That is what stops
    /// a fresh conclusion from inheriting the elapsed time of the induction it
    /// replaced.
    last_send: Option<Instant>,
}

impl Connect {
    pub fn new(
        remote: SocketAddr,
        local_addr: IpAddr,
        init_settings: ConnInitSettings,
        streamid: Option<String>,
        starting_send_seqnum: SeqNumber,
    ) -> Self {
        Connect {
            remote,
            local_addr,
            init_settings,
            state: ConnectState::new(),
            streamid,
            starting_send_seqnum,
            last_send: None,
        }
    }

    fn on_start(&mut self, now: Instant) -> ConnectionResult {
        let packet = Packet::Control(ControlPacket {
            dest_sockid: SocketId(0),
            timestamp: TimeStamp::from_micros(0), // TODO: this is not zero in the reference implementation
            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                init_seq_num: self.starting_send_seqnum,
                max_packet_size: self.init_settings.max_packet_size,
                max_flow_size: self.init_settings.max_flow_size,
                socket_id: self.init_settings.local_sockid,
                shake_type: ShakeType::Induction,
                peer_addr: self.local_addr,
                syn_cookie: 0,
                info: HandshakeVsInfo::V4(SocketType::Datagram),
            }),
        });
        self.state = InductionResponseWait(packet.clone());
        self.last_send = Some(now);
        SendPacket((packet, self.remote))
    }

    pub fn wait_for_induction(
        &mut self,
        from: SocketAddr,
        timestamp: TimeStamp,
        info: HandshakeControlInfo,
        now: Instant,
    ) -> ConnectionResult {
        match (info.shake_type, &info.info, from) {
            (ShakeType::Induction, HandshakeVsInfo::V5 { .. }, from) if from == self.remote => {
                let (hsv5, cm) =
                    start_hsv5_initiation(self.init_settings.clone(), self.streamid.clone(), now);

                // send back a packet with the same syn cookie
                let packet = Packet::Control(ControlPacket {
                    timestamp,
                    dest_sockid: SocketId(0),
                    control_type: ControlTypes::Handshake(HandshakeControlInfo {
                        shake_type: ShakeType::Conclusion,
                        socket_id: self.init_settings.local_sockid,
                        info: hsv5,
                        init_seq_num: self.starting_send_seqnum,
                        ..info
                    }),
                });
                self.state = ConclusionResponseWait(packet.clone(), cm);
                // The deadline restarts here, from the instant this conclusion
                // goes out. Without this the next tick could resend within
                // milliseconds, and each resend makes MediaMTX replace its
                // publisher for the path.
                self.last_send = Some(now);
                SendPacket((packet, from))
            }
            (ShakeType::Induction, HandshakeVsInfo::V5 { .. }, from) => {
                NotHandled(UnexpectedHost(self.remote, from))
            }
            (ShakeType::Induction, version, _) => {
                NotHandled(UnsupportedProtocolVersion(version.version()))
            }
            (_, _, _) => NotHandled(InductionExpected(info)),
        }
    }

    fn wait_for_conclusion(
        &mut self,
        from: SocketAddr,
        now: Instant,
        info: HandshakeControlInfo,
        initiator: StartedInitiator,
    ) -> ConnectionResult {
        match (info.shake_type, info.info.version(), from) {
            (ShakeType::Conclusion, 5, from) if from == self.remote => {
                let settings = match initiator.finish_hsv5_initiation(&info, from, now) {
                    Ok(s) => s,
                    Err(rr) => return NotHandled(rr),
                };

                // TODO: no handshake retransmit packet needed? is this right? Needs testing.
                Connected(
                    None,
                    Connection {
                        settings,
                        handshake: Handshake::Connector,
                    },
                )
            }
            (ShakeType::Conclusion, 5, from) => NotHandled(UnexpectedHost(self.remote, from)),
            (ShakeType::Conclusion, version, _) => NotHandled(UnsupportedProtocolVersion(version)),
            (ShakeType::Rejection(rej), _, from) if from == self.remote => {
                Reject(None, ConnectionReject::Rejected(rej))
            }
            (ShakeType::Rejection(_), _, from) => NotHandled(UnexpectedHost(self.remote, from)),
            (ShakeType::Induction, _, _) => NoAction,
            (_, _, _) => NotHandled(ConclusionExpected(info)),
        }
    }

    pub fn handle_packet(&mut self, packet: ReceivePacketResult, now: Instant) -> ConnectionResult {
        use ReceivePacketError::*;
        match packet {
            Ok((packet, from)) => match (self.state.clone(), packet) {
                (InductionResponseWait(_), Packet::Control(control)) => {
                    match control.control_type {
                        ControlTypes::Handshake(shake) => {
                            self.wait_for_induction(from, control.timestamp, shake, now)
                        }
                        control_type => NotHandled(HandshakeExpected(control_type)),
                    }
                }
                (ConclusionResponseWait(_, cm), Packet::Control(control)) => {
                    match control.control_type {
                        ControlTypes::Handshake(shake) => {
                            self.wait_for_conclusion(from, now, shake, cm)
                        }
                        control_type => NotHandled(HandshakeExpected(control_type)),
                    }
                }
                (_, Packet::Data(data)) => NotHandled(ControlExpected(data)),
                (_, _) => NoAction,
            },
            Err(Io(error)) if error.kind() == ErrorKind::ConnectionReset => {
                info!("ConnectionReset received, listener may not have opened the port yet...");
                NoAction
            }
            Err(Io(error)) => Failure(error),
            Err(Parse(PacketParseError::BadConnectionType(c))) => Failure(std::io::Error::new(
                ErrorKind::ConnectionReset,
                Parse(PacketParseError::BadConnectionType(c)),
            )),
            Err(Parse(e)) => NotHandled(ConnectError::ParseFailed(e)),
        }
    }

    /// Whether the request currently in flight may be sent again. `None` means
    /// nothing has gone out yet, which only holds before the induction.
    fn retransmit_due(&self, now: Instant) -> bool {
        match self.last_send {
            None => true,
            Some(last_send) => now.duration_since(last_send) >= HANDSHAKE_RETRANSMIT_INTERVAL,
        }
    }

    pub fn handle_tick(&mut self, now: Instant) -> ConnectionResult {
        let request_packet = match &self.state {
            Configured => return self.on_start(now),
            InductionResponseWait(request_packet) => request_packet.clone(),
            ConclusionResponseWait(request_packet, _) => request_packet.clone(),
        };
        // A tick is an opportunity to retransmit, not an instruction to. The
        // driver's tick rate is deliberately not the retransmission rate: the
        // deadline belongs to the packet, so a state change caused by an
        // incoming packet resets it too.
        if !self.retransmit_due(now) {
            return NoAction;
        }
        // Byte-identical to what went out before — a retransmission, not a new
        // request. Only the deadline moves.
        self.last_send = Some(now);
        SendPacket((request_packet, self.remote))
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use assert_matches::assert_matches;
    use rand::random;

    use crate::{
        options::{self, PacketCount, PacketSize, SrtVersion},
        protocol::pending_connection::ConnectionReject,
    };

    use super::*;

    const TEST_SOCKID: SocketId = SocketId(7655);

    #[test]
    fn reject() {
        let mut c = test_connect(Some("#!::u=test".into()));
        c.handle_tick(Instant::now());

        let first = Packet::Control(ControlPacket {
            timestamp: TimeStamp::from_micros(0),
            dest_sockid: TEST_SOCKID,
            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                syn_cookie: 5554,
                socket_id: SocketId(5678),
                info: HandshakeVsInfo::V5(HsV5Info::default()),
                init_seq_num: random(),
                max_packet_size: PacketSize(8192),
                max_flow_size: PacketCount(1234),
                shake_type: ShakeType::Induction,
                peer_addr: [127, 0, 0, 1].into(),
            }),
        });

        let resp = c.handle_packet(Ok((first, test_remote())), Instant::now());
        assert_matches!(
            resp,
            ConnectionResult::SendPacket((Packet::Control(ControlPacket {
                control_type: ControlTypes::Handshake(HandshakeControlInfo {
                    shake_type: ShakeType::Conclusion,
                    socket_id,
                    syn_cookie: 5554,
                    ..
                }), ..
            }), _)) if socket_id == TEST_SOCKID
        );

        // send rejection
        let rejection = Packet::Control(ControlPacket {
            timestamp: TimeStamp::from_micros(0),
            dest_sockid: TEST_SOCKID,
            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                init_seq_num: random(),
                max_packet_size: PacketSize(8192),
                max_flow_size: PacketCount(1234),
                shake_type: ShakeType::Rejection(RejectReason::Server(ServerRejectReason::BadMode)),
                socket_id: SocketId(5678),
                syn_cookie: 2222,
                peer_addr: [127, 0, 0, 1].into(),
                info: HandshakeVsInfo::V5(HsV5Info::default()),
            }),
        });

        let resp = c.handle_packet(Ok((rejection, test_remote())), Instant::now());
        assert_matches!(
            resp,
            ConnectionResult::Reject(
                _,
                ConnectionReject::Rejected(RejectReason::Server(ServerRejectReason::BadMode)),
            )
        );
    }

    /// The listener's induction response, which is what moves the caller from
    /// `InductionResponseWait` to `ConclusionResponseWait`.
    fn induction_response() -> Packet {
        Packet::Control(ControlPacket {
            timestamp: TimeStamp::from_micros(0),
            dest_sockid: TEST_SOCKID,
            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                syn_cookie: 5554,
                socket_id: SocketId(5678),
                info: HandshakeVsInfo::V5(HsV5Info::default()),
                init_seq_num: random(),
                max_packet_size: PacketSize(8192),
                max_flow_size: PacketCount(1234),
                shake_type: ShakeType::Induction,
                peer_addr: [127, 0, 0, 1].into(),
            }),
        })
    }

    /// The listener's conclusion response, complete with the HSv5 extension
    /// `finish_hsv5_initiation` requires — a bare `HsV5Info::default()` is
    /// rejected as `ExpectedExtFlags`.
    fn conclusion_response() -> Packet {
        Packet::Control(ControlPacket {
            timestamp: TimeStamp::from_micros(0),
            dest_sockid: TEST_SOCKID,
            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                syn_cookie: 5554,
                socket_id: SocketId(5678),
                info: HandshakeVsInfo::V5(HsV5Info {
                    ext_hs: Some(SrtControlPacket::HandshakeResponse(SrtHandshake {
                        version: SrtVersion::CURRENT,
                        flags: SrtShakeFlags::SUPPORTED,
                        send_latency: Duration::from_millis(120),
                        recv_latency: Duration::from_millis(120),
                    })),
                    ..HsV5Info::default()
                }),
                init_seq_num: random(),
                max_packet_size: PacketSize(8192),
                max_flow_size: PacketCount(1234),
                shake_type: ShakeType::Conclusion,
                peer_addr: [127, 0, 0, 1].into(),
            }),
        })
    }

    fn sent_packet(result: &ConnectionResult) -> Packet {
        match result {
            ConnectionResult::SendPacket((packet, _)) => packet.clone(),
            other => panic!("expected a packet to be sent, got {other:?}"),
        }
    }

    fn shake_type_of(packet: &Packet) -> ShakeType {
        match packet {
            Packet::Control(ControlPacket {
                control_type: ControlTypes::Handshake(info),
                ..
            }) => info.shake_type,
            other => panic!("expected a handshake, got {other:?}"),
        }
    }

    // 1
    #[test]
    fn the_first_tick_sends_the_induction() {
        let mut c = test_connect(None);
        let start = Instant::now();

        let sent = sent_packet(&c.handle_tick(start));

        assert_eq!(shake_type_of(&sent), ShakeType::Induction);
    }

    // 2
    #[test]
    fn a_tick_before_the_deadline_does_nothing() {
        let mut c = test_connect(None);
        let start = Instant::now();
        c.handle_tick(start);

        // The driver ticks every 100 ms; none of these may put a packet on the
        // wire, which is the whole point of the patch.
        for elapsed in [1, 45, 100, 200, 249] {
            assert_matches!(
                c.handle_tick(start + Duration::from_millis(elapsed)),
                ConnectionResult::NoAction,
                "resent after only {elapsed} ms"
            );
        }
    }

    // 3
    #[test]
    fn a_tick_after_the_deadline_resends_the_induction() {
        let mut c = test_connect(None);
        let start = Instant::now();
        let first = sent_packet(&c.handle_tick(start));

        let resent = sent_packet(&c.handle_tick(start + HANDSHAKE_RETRANSMIT_INTERVAL));

        assert_eq!(first, resent, "a retransmission is the same packet");
        // ...and the deadline restarts from the retransmission.
        assert_matches!(
            c.handle_tick(start + HANDSHAKE_RETRANSMIT_INTERVAL + Duration::from_millis(100)),
            ConnectionResult::NoAction
        );
    }

    // 4 + 7
    #[test]
    fn the_conclusion_restarts_the_deadline_instead_of_inheriting_it() {
        let mut c = test_connect(Some("#!::u=test".into()));
        let start = Instant::now();
        c.handle_tick(start);

        // The induction response arrives 240 ms in — 10 ms short of the
        // induction's own deadline. Upstream would then have resent the brand
        // new conclusion on the very next tick.
        let response_at = start + Duration::from_millis(240);
        let conclusion = sent_packet(&c.handle_packet(Ok((induction_response(), test_remote())), response_at));
        assert_eq!(shake_type_of(&conclusion), ShakeType::Conclusion);

        // A tick 20 ms later is 260 ms after the induction but only 20 ms after
        // the conclusion: the deadline must have moved with the packet.
        assert_matches!(
            c.handle_tick(response_at + Duration::from_millis(20)),
            ConnectionResult::NoAction,
            "the conclusion inherited the induction's elapsed time"
        );
    }

    // 5
    #[test]
    fn ticks_45_to_100_ms_after_the_conclusion_do_nothing() {
        let mut c = test_connect(Some("#!::u=test".into()));
        let start = Instant::now();
        c.handle_tick(start);
        let sent_at = start + Duration::from_millis(30);
        c.handle_packet(Ok((induction_response(), test_remote())), sent_at);

        // The exact window from the production trace: the second conclusion
        // went out 45.7 ms after the first, and a third 145 ms after that.
        for elapsed in [45, 46, 100, 145, 249] {
            assert_matches!(
                c.handle_tick(sent_at + Duration::from_millis(elapsed)),
                ConnectionResult::NoAction,
                "a second conclusion {elapsed} ms in makes MediaMTX replace its publisher"
            );
        }
    }

    // 6
    #[test]
    fn the_conclusion_is_resent_unchanged_once_the_deadline_passes() {
        let mut c = test_connect(Some("#!::u=test".into()));
        let start = Instant::now();
        c.handle_tick(start);
        let sent_at = start + Duration::from_millis(30);
        let first = sent_packet(&c.handle_packet(Ok((induction_response(), test_remote())), sent_at));

        let resent = sent_packet(&c.handle_tick(sent_at + HANDSHAKE_RETRANSMIT_INTERVAL));

        assert_eq!(first, resent, "byte-identical retransmission");
        assert_eq!(shake_type_of(&resent), ShakeType::Conclusion);
    }

    // 8
    #[test]
    fn a_lost_conclusion_is_still_recovered_and_the_connection_completes() {
        let mut c = test_connect(Some("#!::u=test".into()));
        let start = Instant::now();
        c.handle_tick(start);
        let sent_at = start + Duration::from_millis(30);
        // Pretend this one never reached the listener.
        let lost = sent_packet(&c.handle_packet(Ok((induction_response(), test_remote())), sent_at));

        // Nothing happens for a quarter of a second...
        assert_matches!(
            c.handle_tick(sent_at + Duration::from_millis(200)),
            ConnectionResult::NoAction
        );
        // ...and then the same request goes out again, so a dropped datagram
        // still costs one interval rather than the whole connection.
        let recovered = sent_packet(&c.handle_tick(sent_at + Duration::from_millis(300)));
        assert_eq!(lost, recovered);

        // And the listener's answer to the retransmission still connects.
        let result = c.handle_packet(
            Ok((conclusion_response(), test_remote())),
            sent_at + Duration::from_millis(320),
        );
        assert_matches!(result, ConnectionResult::Connected(..));
    }

    /// The bug as it appeared on the wire: two conclusions 45.7 ms apart.
    #[test]
    fn no_two_conclusions_are_ever_sent_within_the_interval() {
        let mut c = test_connect(Some("#!::u=test".into()));
        let start = Instant::now();
        c.handle_tick(start);
        let sent_at = start + Duration::from_millis(30);
        c.handle_packet(Ok((induction_response(), test_remote())), sent_at);

        // Drive it exactly like srt-tokio does: a tick every 100 ms for 3 s.
        let mut conclusions = vec![sent_at];
        for step in 1..=30u64 {
            let now = start + Duration::from_millis(100 * step);
            if let ConnectionResult::SendPacket((packet, _)) = c.handle_tick(now) {
                assert_eq!(shake_type_of(&packet), ShakeType::Conclusion);
                conclusions.push(now);
            }
        }

        assert!(conclusions.len() > 1, "retransmission must still happen");
        for pair in conclusions.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            assert!(
                gap >= HANDSHAKE_RETRANSMIT_INTERVAL,
                "two conclusions only {gap:?} apart"
            );
        }
    }

    fn test_remote() -> SocketAddr {
        ([127, 0, 0, 1], 6666).into()
    }

    fn test_connect(sid: Option<String>) -> Connect {
        Connect::new(
            test_remote(),
            [127, 0, 0, 1].into(),
            ConnInitSettings {
                local_sockid: TEST_SOCKID,
                key_settings: None,
                key_refresh: Default::default(),
                send_latency: Duration::from_millis(20),
                recv_latency: Duration::from_millis(20),
                bandwidth: Default::default(),
                statistics_interval: Duration::from_secs(1),
                recv_buffer_size: options::PacketCount(8192),
                send_buffer_size: options::PacketCount(8192),
                max_packet_size: options::PacketSize(1500),
                max_flow_size: options::PacketCount(8192),
                peer_idle_timeout: Duration::from_secs(5),
                too_late_packet_drop: true,
            },
            sid,
            random(),
        )
    }
}
