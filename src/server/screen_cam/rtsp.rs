// Minimal RTSP/1.0 server for Sehcontrol ScreenCam (Fase 1 MVP).
//
// Supports exactly what a DVR/NVR or VLC needs to pull the single `/live/main`
// stream this module serves: OPTIONS, DESCRIBE, SETUP (both UDP and TCP
// interleaved transport), PLAY, GET_PARAMETER (used by many clients as a
// keep-alive) and TEARDOWN, plus Basic/Digest authentication when the panel
// has issued credentials (see auth.rs). No multiple routes — see
// docs/SCREENCAM_PLAN.md Fase 1/3 for what's intentionally deferred.
//
// Known simplifications (tracked in docs/SCREENCAM_PLAN.md, not silently
// hidden): no RTCP Sender Reports are sent on the UDP path (some strict NVRs
// may eventually want them); a TCP-interleaved session that receives
// unexpected non-RTSP bytes after PLAY (e.g. a client sending RTCP back over
// the same socket) will simply have that request parse fail and the
// connection close, it will not corrupt other sessions.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};

use hbb_common::{anyhow::anyhow, bail, log, ResultType};

use super::auth;
use super::SharedState;

pub enum Transport {
    Tcp {
        stream: Arc<Mutex<TcpStream>>,
        rtp_channel: u8,
    },
    Udp {
        rtp_socket: UdpSocket,
        // Kept open (and never written to in this MVP) purely so the client's
        // RTCP receiver reports land somewhere sane instead of getting an
        // ICMP port-unreachable back. See module docs.
        _rtcp_socket: UdpSocket,
    },
}

pub struct Session {
    pub id: String,
    pub transport: Transport,
}

impl Session {
    /// Best-effort send; a broken pipe just means the client went away and
    /// will be pruned the next time its connection thread notices EOF.
    pub fn send_rtp(&self, packet: &[u8]) {
        match &self.transport {
            Transport::Udp { rtp_socket, .. } => {
                let _ = rtp_socket.send(packet);
            }
            Transport::Tcp { stream, rtp_channel } => {
                let mut framed = Vec::with_capacity(4 + packet.len());
                framed.push(b'$');
                framed.push(*rtp_channel);
                framed.extend_from_slice(&(packet.len() as u16).to_be_bytes());
                framed.extend_from_slice(packet);
                if let Ok(mut s) = stream.lock() {
                    let _ = s.write_all(&framed);
                }
            }
        }
    }
}

struct RtspRequest {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
}

pub fn start_listener(port: u16, state: Arc<SharedState>) -> ResultType<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    log::info!("[screencam] RTSP listening on 0.0.0.0:{port}");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let state = state.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle_connection(stream, state) {
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

fn handle_connection(stream: TcpStream, state: Arc<SharedState>) -> ResultType<()> {
    stream.set_nodelay(true).ok();
    let peer_addr = stream.peer_addr()?;
    let write_half = Arc::new(Mutex::new(stream.try_clone()?));
    let mut reader = BufReader::new(stream);

    let mut session_id: Option<String> = None;
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
        if matches!(req.method.as_str(), "DESCRIBE" | "SETUP" | "PLAY") {
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
                        "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER",
                    )],
                    None,
                )?;
            }
            "DESCRIBE" => match build_sdp(&state, peer_addr) {
                Some(sdp) => {
                    write_response(
                        &write_half,
                        "200 OK",
                        &cseq,
                        &[("Content-Type", "application/sdp")],
                        Some(sdp.as_bytes()),
                    )?;
                }
                None => {
                    // Encoder hasn't produced a keyframe (SPS/PPS) yet.
                    write_response(&write_half, "503 Service Unavailable", &cseq, &[], None)?;
                }
            },
            "SETUP" => {
                let transport_hdr = req.headers.get("transport").cloned().unwrap_or_default();
                match setup_transport(&transport_hdr, peer_addr, &write_half) {
                    Ok((transport, resp_transport_hdr)) => {
                        let id = session_id
                            .get_or_insert_with(|| format!("{:016X}", hbb_common::rand::random::<u64>()))
                            .clone();
                        state.sessions.lock().unwrap().push(Session {
                            id: id.clone(),
                            transport,
                        });
                        write_response(
                            &write_half,
                            "200 OK",
                            &cseq,
                            &[("Transport", &resp_transport_hdr), ("Session", &id)],
                            None,
                        )?;
                    }
                    Err(e) => {
                        log::warn!("[screencam] SETUP failed: {e:?}");
                        write_response(&write_half, "461 Unsupported Transport", &cseq, &[], None)?;
                    }
                }
            }
            "PLAY" => {
                let id = session_id.clone().unwrap_or_default();
                write_response(
                    &write_half,
                    "200 OK",
                    &cseq,
                    &[("Session", &id), ("Range", "npt=0.000-")],
                    None,
                )?;
            }
            "GET_PARAMETER" => {
                // Used by many clients/NVRs purely as a keep-alive ping.
                write_response(&write_half, "200 OK", &cseq, &[], None)?;
            }
            "TEARDOWN" => {
                if let Some(id) = &session_id {
                    state.sessions.lock().unwrap().retain(|s| &s.id != id);
                }
                write_response(&write_half, "200 OK", &cseq, &[], None)?;
                break;
            }
            other => {
                log::debug!("[screencam] unsupported method: {other}");
                write_response(&write_half, "501 Not Implemented", &cseq, &[], None)?;
            }
        }
    }

    if let Some(id) = &session_id {
        state.sessions.lock().unwrap().retain(|s| &s.id != id);
    }
    Ok(())
}

fn setup_transport(
    transport_hdr: &str,
    peer_addr: SocketAddr,
    write_half: &Arc<Mutex<TcpStream>>,
) -> ResultType<(Transport, String)> {
    if transport_hdr.contains("TCP") || transport_hdr.contains("interleaved") {
        // TCP interleaved: RTP shares this same connection, channel 0 (RTCP
        // would be channel 1, unused here — see module docs).
        let transport = Transport::Tcp {
            stream: write_half.clone(),
            rtp_channel: 0,
        };
        Ok((transport, "RTP/AVP/TCP;unicast;interleaved=0-1".to_owned()))
    } else {
        let client_ports = extract_param(transport_hdr, "client_port=")
            .ok_or_else(|| anyhow!("missing client_port in Transport header"))?;
        let (rtp_port, rtcp_port) = parse_port_range(&client_ports)?;

        let rtp_socket = UdpSocket::bind("0.0.0.0:0")?;
        rtp_socket.connect((peer_addr.ip(), rtp_port))?;
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
                _rtcp_socket: rtcp_socket,
            },
            resp,
        ))
    }
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

fn read_request(reader: &mut BufReader<TcpStream>) -> ResultType<Option<RtspRequest>> {
    let mut request_line = String::new();
    loop {
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
    if let Some(len) = headers.get("content-length").and_then(|v| v.parse::<usize>().ok()) {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
    }

    if method.is_empty() {
        bail!("empty request line");
    }
    Ok(Some(RtspRequest { method, uri, headers }))
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

fn build_sdp(state: &SharedState, peer_addr: SocketAddr) -> Option<String> {
    let sps = state.sps.lock().unwrap().clone()?;
    let pps = state.pps.lock().unwrap().clone()?;
    let local_ip = local_ip_for_peer(peer_addr).unwrap_or_else(|| "0.0.0.0".to_owned());

    let profile_level_id = if sps.len() >= 3 {
        format!("{:02X}{:02X}{:02X}", sps[0], sps[1], sps[2])
    } else {
        "42E01E".to_owned()
    };
    let sps_b64 = base64_encode(&sps);
    let pps_b64 = base64_encode(&pps);

    Some(format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {ip}\r\n\
         s=Sehcontrol ScreenCam\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         a=tool:Sehcontrol ScreenCam\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\n\
         a=fmtp:96 packetization-mode=1;profile-level-id={plid};sprop-parameter-sets={sps},{pps}\r\n\
         a=control:track1\r\n",
        ip = local_ip,
        plid = profile_level_id,
        sps = sps_b64,
        pps = pps_b64,
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
