// Minimal ONVIF support for Sehcontrol ScreenCam (docs/SCREENCAM_PLAN.md, Fase 6).
//
// Scope, deliberately: WS-Discovery (so an NVR's "search for cameras" finds
// this device on its own) + the smallest ONVIF device/media SOAP surface
// that lets an NVR go from "found it" to "here's the RTSP URL" without a
// human typing anything in by hand:
//   - device_service: GetSystemDateAndTime, GetDeviceInformation,
//     GetCapabilities, GetServices, GetServiceCapabilities, GetScopes,
//     GetNetworkInterfaces
//   - media_service: GetProfiles/GetProfile, GetStreamUri, GetVideoSources,
//     GetVideoSourceConfiguration(s), GetVideoEncoderConfiguration(s),
//     GetServiceCapabilities
//
// The list past GetProfiles/GetStreamUri exists because real NVRs walk more of
// the device than VLC ever does. `GetServices` in particular is what an
// ONVIF 2.x client (Hikvision's, notably) calls before anything else, and a
// fault there made it abandon the device entirely — the rest are what Dahua's
// gSOAP client queries while building its channel configuration.
//
// Explicitly NOT implemented yet (see docs/SCREENCAM_PLAN.md for what's
// deferred and why): WS-Security/auth on these SOAP calls (RTSP authentication
// is handled separately), GetSnapshotUri (this pipeline has no JPEG path, so
// it returns a fault rather than a URL that would 404), a second low-resolution
// profile for NVRs that would prefer a sub-stream on their live wall,
// PTZ/Events/Imaging/Analytics services, WS-Discovery
// "Hello" announcements on startup (only Probes are answered — sufficient
// for any NVR that actively searches, which is the common case; a purely
// passive listener wouldn't notice this device until it probes).
//
// No SOAP/XML crate pulled in: requests are routed by a crude substring
// search for the action name in the body (e.g. `body.contains("GetProfiles")`)
// and responses are plain format!() templates — same "hand-roll it, no new
// dependency" approach as rtsp.rs takes for RTSP/SDP.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;

#[cfg(windows)]
use std::os::windows::io::{FromRawSocket, RawSocket};

#[cfg(windows)]
use windows::Win32::{
    Foundation::ERROR_BUFFER_OVERFLOW,
    NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    },
    Networking::WinSock::{
        bind, closesocket, setsockopt, WSASocketW, AF_INET, IN_ADDR, IN_ADDR_0, IPPROTO_UDP,
        SOCKADDR, SOCKADDR_IN, SOCK_DGRAM, SOL_SOCKET, SO_REUSEADDR,
    },
};

use hbb_common::{log, ResultType};

use super::rtsp::local_ip_for_peer;
use super::SharedState;

const WS_DISCOVERY_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const WS_DISCOVERY_PORT: u16 = 3702;

pub fn start(rtsp_port: u16, onvif_port: u16, device_uuid: String, state: Arc<SharedState>) {
    start_discovery_responder(onvif_port, device_uuid.clone());
    start_soap_server(rtsp_port, onvif_port, device_uuid, state);
}

// ---------------------------------------------------------------------------
// WS-Discovery: answers Probe messages on the standard ONVIF multicast group.
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn bind_discovery_socket() -> io::Result<UdpSocket> {
    // Rust ya inicializa WinSock al usar std::net. Creamos el socket manualmente
    // para poder activar SO_REUSEADDR antes del bind y convivir con FDResPub.
    unsafe {
        let socket = WSASocketW(AF_INET.0 as i32, SOCK_DGRAM.0, IPPROTO_UDP.0, None, 0, 0)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let reuse = 1i32.to_ne_bytes();
        if setsockopt(socket, SOL_SOCKET, SO_REUSEADDR, Some(&reuse)) != 0 {
            closesocket(socket);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "setsockopt(SO_REUSEADDR) failed",
            ));
        }

        let address = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: WS_DISCOVERY_PORT.to_be(),
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 { S_addr: 0 },
            },
            sin_zero: [0; 8],
        };

        if bind(
            socket,
            &address as *const SOCKADDR_IN as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        ) != 0
        {
            closesocket(socket);
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("couldn't share/bind UDP {WS_DISCOVERY_PORT}"),
            ));
        }

        Ok(UdpSocket::from_raw_socket(socket.0 as RawSocket))
    }
}

#[cfg(not(windows))]
fn bind_discovery_socket() -> io::Result<UdpSocket> {
    UdpSocket::bind(("0.0.0.0", WS_DISCOVERY_PORT))
}

#[cfg(windows)]
fn discovery_ipv4_interfaces() -> Vec<Ipv4Addr> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 0u32;

    let first = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, None, &mut size) };
    if first != ERROR_BUFFER_OVERFLOW.0 || size == 0 {
        log::warn!("[screencam][onvif] GetAdaptersAddresses size query failed: {first}");
        return Vec::new();
    }

    // Vec<usize> garantiza alineación suficiente para IP_ADAPTER_ADDRESSES_LH.
    let word = std::mem::size_of::<usize>();
    let mut storage = vec![0usize; (size as usize + word - 1) / word];
    let head = storage.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;

    let result =
        unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(head), &mut size) };
    if result != 0 {
        log::warn!("[screencam][onvif] GetAdaptersAddresses failed: {result}");
        return Vec::new();
    }

    let mut addresses = Vec::new();
    let mut adapter = head;

    unsafe {
        while !adapter.is_null() {
            let mut unicast = (*adapter).FirstUnicastAddress;

            while !unicast.is_null() {
                let sockaddr = (*unicast).Address.lpSockaddr;

                if !sockaddr.is_null() && (*sockaddr).sa_family == AF_INET {
                    let sockaddr_in = &*(sockaddr as *const SOCKADDR_IN);
                    let raw = sockaddr_in.sin_addr.S_un.S_addr;
                    let ip = Ipv4Addr::from(raw.to_ne_bytes());

                    if !ip.is_unspecified()
                        && !ip.is_loopback()
                        && !ip.is_multicast()
                        && !ip.is_link_local()
                        && !addresses.contains(&ip)
                    {
                        addresses.push(ip);
                    }
                }

                unicast = (*unicast).Next;
            }

            adapter = (*adapter).Next;
        }
    }

    addresses
}

#[cfg(not(windows))]
fn discovery_ipv4_interfaces() -> Vec<Ipv4Addr> {
    vec![Ipv4Addr::UNSPECIFIED]
}

fn join_discovery_interfaces(socket: &UdpSocket) -> usize {
    let interfaces = discovery_ipv4_interfaces();
    let mut joined = 0usize;

    for ip in interfaces {
        match socket.join_multicast_v4(&WS_DISCOVERY_MULTICAST_ADDR, &ip) {
            Ok(()) => {
                joined += 1;
                log::info!("[screencam][onvif] joined WS-Discovery multicast on {ip}");
            }
            Err(e) => {
                log::debug!("[screencam][onvif] couldn't join WS-Discovery on {ip}: {e}");
            }
        }
    }

    joined
}

fn start_discovery_responder(onvif_port: u16, device_uuid: String) {
    std::thread::spawn(move || {
        let socket = match bind_discovery_socket() {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "[screencam][onvif] WS-Discovery disabled, couldn't open/share UDP {WS_DISCOVERY_PORT}: {e}"
                );
                return;
            }
        };

        let joined = join_discovery_interfaces(&socket);
        if joined == 0 {
            log::warn!(
                "[screencam][onvif] WS-Discovery disabled: no IPv4 interface accepted multicast membership"
            );
            return;
        }

        log::info!(
            "[screencam][onvif] WS-Discovery listening on {WS_DISCOVERY_MULTICAST_ADDR}:{WS_DISCOVERY_PORT} across {joined} IPv4 interface(s)"
        );

        let mut buf = [0u8; 8192];
        loop {
            let (len, src) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("[screencam][onvif] WS-Discovery recv error: {e}");
                    continue;
                }
            };

            let msg = String::from_utf8_lossy(&buf[..len]);
            // The multicast group also carries other devices' Hello/Bye/
            // ProbeMatches traffic — only ever answer an actual Probe.
            if !msg.contains("Probe") || msg.contains("ProbeMatch") {
                continue;
            }

            let relates_to = extract_tag(&msg, "MessageID")
                .unwrap_or_else(|| "urn:uuid:00000000-0000-0000-0000-000000000000".to_owned());

            let Some(local_ip) = local_ip_for_peer(src) else {
                continue;
            };

            let response = build_probe_match(&relates_to, &device_uuid, &local_ip, onvif_port);

            if let Err(e) = socket.send_to(response.as_bytes(), src) {
                log::debug!("[screencam][onvif] failed to send ProbeMatch to {src}: {e}");
            }
        }
    });
}

/// Finds `<...MessageID>value</...>` regardless of which namespace prefix the
/// sender used (`w:`, `a:`, `wsa:`, none, ...) by searching for the substring
/// right after the tag name instead of parsing XML properly.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let marker = format!("{tag}>");
    let start = xml.find(&marker)? + marker.len();
    let rest = &xml[start..];
    let end = rest.find('<')?;
    Some(rest[..end].trim().to_owned())
}

fn build_probe_match(
    relates_to: &str,
    device_uuid: &str,
    local_ip: &str,
    onvif_port: u16,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<e:Header>
<w:MessageID>urn:uuid:{msg_id}</w:MessageID>
<w:RelatesTo>{relates_to}</w:RelatesTo>
<w:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</w:To>
<w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</w:Action>
</e:Header>
<e:Body>
<d:ProbeMatches>
<d:ProbeMatch>
<w:EndpointReference><w:Address>urn:uuid:{device_uuid}</w:Address></w:EndpointReference>
<d:Types>dn:NetworkVideoTransmitter</d:Types>
<d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/Sehcontrol_ScreenCam onvif://www.onvif.org/hardware/ScreenCam</d:Scopes>
<d:XAddrs>http://{local_ip}:{onvif_port}/onvif/device_service</d:XAddrs>
<d:MetadataVersion>1</d:MetadataVersion>
</d:ProbeMatch>
</d:ProbeMatches>
</e:Body>
</e:Envelope>"#,
        msg_id = uuid::Uuid::new_v4(),
        relates_to = relates_to,
        device_uuid = device_uuid,
        local_ip = local_ip,
        onvif_port = onvif_port,
    )
}

// ---------------------------------------------------------------------------
// Minimal ONVIF device_service / media_service SOAP endpoints over HTTP.
// ---------------------------------------------------------------------------

fn start_soap_server(
    rtsp_port: u16,
    onvif_port: u16,
    device_uuid: String,
    state: Arc<SharedState>,
) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("0.0.0.0", onvif_port)) {
            Ok(l) => l,
            Err(e) => {
                log::error!(
                    "[screencam][onvif] failed to start SOAP HTTP server on 0.0.0.0:{onvif_port}: {e}"
                );
                return;
            }
        };
        log::info!("[screencam][onvif] SOAP services listening on 0.0.0.0:{onvif_port}");
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let device_uuid = device_uuid.clone();
            let state = state.clone();
            std::thread::spawn(move || {
                if let Err(e) =
                    handle_soap_connection(stream, rtsp_port, onvif_port, &device_uuid, &state)
                {
                    log::debug!("[screencam][onvif] connection error: {e:?}");
                }
            });
        }
    });
}

fn handle_soap_connection(
    stream: TcpStream,
    rtsp_port: u16,
    onvif_port: u16,
    device_uuid: &str,
    state: &SharedState,
) -> ResultType<()> {
    stream.set_nodelay(true).ok();
    let peer_addr = stream.peer_addr()?;
    let mut write_half = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.trim().split_whitespace();
    let _method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").to_owned();

    let mut content_length = 0usize;
    let mut chunked = false;
    let mut expects_continue = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("transfer-encoding") {
                chunked = v.to_ascii_lowercase().contains("chunked");
            } else if k.eq_ignore_ascii_case("expect") {
                expects_continue = v.eq_ignore_ascii_case("100-continue");
            }
        }
    }

    // gSOAP — what Dahua's ONVIF client is built on — sends `Expect:
    // 100-continue` and then waits for the interim response before writing the
    // body. Never sending it means the request only arrives after the client's
    // own timeout expires, if at all.
    if expects_continue {
        write_half.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }

    // gSOAP also falls back to chunked encoding whenever it streams a request
    // without precomputing its length, in which case there is no
    // Content-Length at all and reading one would yield an empty body and a
    // "action not supported" fault for a perfectly valid call.
    let body_bytes = if chunked {
        read_chunked_body(&mut reader)?
    } else {
        let mut body_bytes = vec![0u8; content_length];
        reader.read_exact(&mut body_bytes)?;
        body_bytes
    };
    let body = String::from_utf8_lossy(&body_bytes);

    let local_ip = local_ip_for_peer(peer_addr).unwrap_or_else(|| "0.0.0.0".to_owned());
    log::debug!("[screencam][onvif] {path} from {peer_addr}");

    // Substring routing, as before. Order matters wherever one action name
    // contains another: `GetProfiles` must be tested before `GetProfile`, and
    // the plural configuration getters before their singular counterparts.
    let (status, response_body) = if path.contains("device_service") {
        if body.contains("GetDeviceInformation") {
            ("200 OK", build_get_device_information(device_uuid))
        } else if body.contains("GetServiceCapabilities") {
            ("200 OK", build_get_device_service_capabilities())
        // ONVIF 2.0's replacement for GetCapabilities. Hikvision NVRs call it
        // first and abandon the whole device if it faults, which is why an
        // otherwise complete device was never getting added.
        } else if body.contains("GetServices") {
            ("200 OK", build_get_services(&local_ip, onvif_port))
        } else if body.contains("GetCapabilities") {
            ("200 OK", build_get_capabilities(&local_ip, onvif_port))
        } else if body.contains("GetSystemDateAndTime") {
            ("200 OK", build_get_system_date_and_time())
        } else if body.contains("GetScopes") {
            ("200 OK", build_get_scopes())
        } else if body.contains("GetNetworkInterfaces") {
            ("200 OK", build_get_network_interfaces(&local_ip))
        } else {
            (
                "500 Internal Server Error",
                build_soap_fault("Action not supported"),
            )
        }
    } else if path.contains("media_service") {
        if body.contains("GetStreamUri") {
            ("200 OK", build_get_stream_uri(&local_ip, rtsp_port))
        } else if body.contains("GetProfiles") || body.contains("GetProfile") {
            build_get_profiles_response(state, body.contains("GetProfiles"))
        } else if body.contains("GetVideoEncoderConfigurations")
            || body.contains("GetVideoEncoderConfiguration")
        {
            build_video_encoder_configuration_response(
                state,
                body.contains("GetVideoEncoderConfigurations"),
            )
        } else if body.contains("GetVideoSourceConfigurations")
            || body.contains("GetVideoSourceConfiguration")
        {
            build_video_source_configuration_response(
                state,
                body.contains("GetVideoSourceConfigurations"),
            )
        } else if body.contains("GetVideoSources") {
            build_get_video_sources_response(state)
        } else if body.contains("GetServiceCapabilities") {
            ("200 OK", build_get_media_service_capabilities())
        } else if body.contains("GetSnapshotUri") {
            // There is no JPEG path in this pipeline — the encoder produces
            // H.264 only. A clean fault is the honest answer; every NVR tested
            // treats a missing snapshot as a cosmetic thumbnail gap and still
            // records the stream.
            (
                "500 Internal Server Error",
                build_soap_fault("Snapshot URI is not supported by this device"),
            )
        } else {
            (
                "500 Internal Server Error",
                build_soap_fault("Action not supported"),
            )
        }
    } else {
        ("404 Not Found", build_soap_fault("Unknown service"))
    };

    let http_response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/soap+xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.as_bytes().len(),
        response_body,
    );
    write_half.write_all(http_response.as_bytes())?;
    Ok(())
}

/// No SOAP request this server answers is anywhere near this large; the cap
/// exists so a malformed or hostile chunked body can't grow unbounded.
const MAX_SOAP_BODY: usize = 256 * 1024;

/// RFC 7230 §4.1 chunked decoding, trailers included. Deliberately minimal:
/// chunk extensions after the size are ignored, which is all any ONVIF client
/// emits.
fn read_chunked_body(reader: &mut BufReader<TcpStream>) -> ResultType<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            break; // truncated; hand back what arrived and let routing fault
        }
        let size_field = size_line.trim();
        let size_field = size_field.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_field, 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        if body.len().saturating_add(size) > MAX_SOAP_BODY {
            hbb_common::bail!("ONVIF SOAP body exceeds {MAX_SOAP_BODY} bytes");
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        // Trailing CRLF after each chunk.
        let mut terminator = String::new();
        reader.read_line(&mut terminator)?;
    }
    // Trailers, then the blank line that ends the message.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
    }
    Ok(body)
}

/// `plural` distinguishes `GetProfiles` (the whole list) from `GetProfile`
/// (one token). With a single fixed profile the payload is the same either
/// way; only the wrapping element name differs, and a client that asked the
/// singular form will not accept the plural response element.
fn build_get_profiles_response(state: &SharedState, plural: bool) -> (&'static str, String) {
    match state.onvif_resolution() {
        Some((width, height)) => ("200 OK", build_get_profiles(width, height, plural)),
        None => (
            "503 Service Unavailable",
            build_soap_fault("Video profile is not ready"),
        ),
    }
}

fn build_video_encoder_configuration_response(
    state: &SharedState,
    plural: bool,
) -> (&'static str, String) {
    match state.onvif_resolution() {
        Some((width, height)) => {
            let element = if plural {
                "GetVideoEncoderConfigurationsResponse"
            } else {
                "GetVideoEncoderConfigurationResponse"
            };
            let wrapper = if plural {
                "Configurations"
            } else {
                "Configuration"
            };
            (
                "200 OK",
                media_envelope(&format!(
                    "<trt:{element}><trt:{wrapper}>{}</trt:{wrapper}></trt:{element}>",
                    video_encoder_configuration(width, height),
                    element = element,
                    wrapper = wrapper,
                )),
            )
        }
        None => (
            "503 Service Unavailable",
            build_soap_fault("Video encoder configuration is not ready"),
        ),
    }
}

fn build_video_source_configuration_response(
    state: &SharedState,
    plural: bool,
) -> (&'static str, String) {
    match state.onvif_resolution() {
        Some((width, height)) => {
            let element = if plural {
                "GetVideoSourceConfigurationsResponse"
            } else {
                "GetVideoSourceConfigurationResponse"
            };
            let wrapper = if plural {
                "Configurations"
            } else {
                "Configuration"
            };
            (
                "200 OK",
                media_envelope(&format!(
                    "<trt:{element}><trt:{wrapper}>{}</trt:{wrapper}></trt:{element}>",
                    video_source_configuration(width, height),
                    element = element,
                    wrapper = wrapper,
                )),
            )
        }
        None => (
            "503 Service Unavailable",
            build_soap_fault("Video source configuration is not ready"),
        ),
    }
}

fn build_get_video_sources_response(state: &SharedState) -> (&'static str, String) {
    match state.onvif_resolution() {
        Some((width, height)) => (
            "200 OK",
            media_envelope(&format!(
                "<trt:GetVideoSourcesResponse>\
                 <trt:VideoSources token=\"{VIDEO_SOURCE_TOKEN}\">\
                 <tt:Framerate>{fps}</tt:Framerate>\
                 <tt:Resolution><tt:Width>{width}</tt:Width><tt:Height>{height}</tt:Height></tt:Resolution>\
                 </trt:VideoSources>\
                 </trt:GetVideoSourcesResponse>",
                fps = ADVERTISED_FPS,
                width = width,
                height = height,
            )),
        ),
        None => (
            "503 Service Unavailable",
            build_soap_fault("Video source is not ready"),
        ),
    }
}

/// Frame rate reported to ONVIF clients. The real rate comes from
/// `ScreenCamConfig::fps`, which this module isn't handed; NVRs use the value
/// only to size their own buffers and re-derive the true rate from the RTP
/// timestamps, so reporting the default is accurate enough and keeps the SOAP
/// layer independent of capture configuration.
const ADVERTISED_FPS: u32 = 10;
const VIDEO_SOURCE_TOKEN: &str = "video_src_1";
const VIDEO_SOURCE_CONFIG_TOKEN: &str = "video_src_cfg_1";
const VIDEO_ENCODER_CONFIG_TOKEN: &str = "video_enc_1";

fn media_envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<SOAP-ENV:Body>{body}</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#
    )
}

fn video_source_configuration(width: usize, height: usize) -> String {
    format!(
        "<tt:VideoSourceConfiguration token=\"{VIDEO_SOURCE_CONFIG_TOKEN}\">\
         <tt:Name>ScreenCam Source</tt:Name>\
         <tt:UseCount>1</tt:UseCount>\
         <tt:SourceToken>{VIDEO_SOURCE_TOKEN}</tt:SourceToken>\
         <tt:Bounds x=\"0\" y=\"0\" width=\"{width}\" height=\"{height}\"/>\
         </tt:VideoSourceConfiguration>"
    )
}

fn video_encoder_configuration(width: usize, height: usize) -> String {
    // SessionTimeout mirrors what the RTSP layer advertises in its Session
    // header, so an NVR that reads its keep-alive interval from either place
    // arrives at the same number.
    format!(
        "<tt:VideoEncoderConfiguration token=\"{VIDEO_ENCODER_CONFIG_TOKEN}\">\
         <tt:Name>ScreenCam Encoder</tt:Name>\
         <tt:UseCount>1</tt:UseCount>\
         <tt:Encoding>H264</tt:Encoding>\
         <tt:Resolution><tt:Width>{width}</tt:Width><tt:Height>{height}</tt:Height></tt:Resolution>\
         <tt:Quality>5</tt:Quality>\
         <tt:RateControl>\
         <tt:FrameRateLimit>{fps}</tt:FrameRateLimit>\
         <tt:EncodingInterval>1</tt:EncodingInterval>\
         <tt:BitrateLimit>4096</tt:BitrateLimit>\
         </tt:RateControl>\
         <tt:H264><tt:GovLength>{gov}</tt:GovLength><tt:H264Profile>Baseline</tt:H264Profile></tt:H264>\
         <tt:SessionTimeout>PT60S</tt:SessionTimeout>\
         </tt:VideoEncoderConfiguration>",
        width = width,
        height = height,
        fps = ADVERTISED_FPS,
        // Matches capture_loop's `fps * 2` keyframe interval.
        gov = ADVERTISED_FPS * 2,
    )
}

fn build_get_services(local_ip: &str, onvif_port: u16) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<SOAP-ENV:Body>
<tds:GetServicesResponse>
<tds:Service>
<tds:Namespace>http://www.onvif.org/ver10/device/wsdl</tds:Namespace>
<tds:XAddr>http://{ip}:{port}/onvif/device_service</tds:XAddr>
<tds:Version><tt:Major>2</tt:Major><tt:Minor>60</tt:Minor></tds:Version>
</tds:Service>
<tds:Service>
<tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace>
<tds:XAddr>http://{ip}:{port}/onvif/media_service</tds:XAddr>
<tds:Version><tt:Major>2</tt:Major><tt:Minor>60</tt:Minor></tds:Version>
</tds:Service>
</tds:GetServicesResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        ip = local_ip,
        port = onvif_port,
    )
}

fn build_get_device_service_capabilities() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
<SOAP-ENV:Body>
<tds:GetServiceCapabilitiesResponse>
<tds:Capabilities>
<tds:Network IPFilter="false" ZeroConfiguration="false" IPVersion6="false" DynDNS="false"/>
<tds:Security TLS1.1="false" TLS1.2="false" OnboardKeyGeneration="false" AccessPolicyConfig="false" Dot1X="false" RemoteUserHandling="false" X.509Token="false" SAMLToken="false" KerberosToken="false" UsernameToken="false" HttpDigest="false" RELToken="false"/>
<tds:System DiscoveryResolve="false" DiscoveryBye="false" RemoteDiscovery="false" SystemBackup="false" SystemLogging="false" FirmwareUpgrade="false" HttpFirmwareUpgrade="false" HttpSystemBackup="false" HttpSystemLogging="false" HttpSupportInformation="false"/>
</tds:Capabilities>
</tds:GetServiceCapabilitiesResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#
        .to_owned()
}

fn build_get_media_service_capabilities() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl">
<SOAP-ENV:Body>
<trt:GetServiceCapabilitiesResponse>
<trt:Capabilities SnapshotUri="false" Rotation="false" VideoSourceMode="false" OSD="false">
<trt:ProfileCapabilities MaximumNumberOfProfiles="1"/>
<trt:StreamingCapabilities RTPMulticast="false" RTP_TCP="true" RTP_RTSP_TCP="true" NonAggregateControl="false"/>
</trt:Capabilities>
</trt:GetServiceCapabilitiesResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#
        .to_owned()
}

fn build_get_scopes() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<SOAP-ENV:Body>
<tds:GetScopesResponse>
<tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/type/video_encoder</tt:ScopeItem></tds:Scopes>
<tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/name/Sehcontrol_ScreenCam</tt:ScopeItem></tds:Scopes>
<tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/hardware/ScreenCam</tt:ScopeItem></tds:Scopes>
</tds:GetScopesResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#
        .to_owned()
}

/// Reported so an NVR's device page has something to show. The MAC is not
/// discoverable from here without another platform-specific dependency, and
/// no client is known to require it, so the interface is described by address
/// only.
fn build_get_network_interfaces(local_ip: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<SOAP-ENV:Body>
<tds:GetNetworkInterfacesResponse>
<tds:NetworkInterfaces token="eth0">
<tt:Enabled>true</tt:Enabled>
<tt:IPv4>
<tt:Enabled>true</tt:Enabled>
<tt:Config>
<tt:Manual><tt:Address>{ip}</tt:Address><tt:PrefixLength>24</tt:PrefixLength></tt:Manual>
<tt:DHCP>false</tt:DHCP>
</tt:Config>
</tt:IPv4>
</tds:NetworkInterfaces>
</tds:GetNetworkInterfacesResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        ip = local_ip,
    )
}

fn build_get_device_information(device_uuid: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
<SOAP-ENV:Body>
<tds:GetDeviceInformationResponse>
<tds:Manufacturer>Sehcontrol</tds:Manufacturer>
<tds:Model>ScreenCam</tds:Model>
<tds:FirmwareVersion>{version}</tds:FirmwareVersion>
<tds:SerialNumber>{device_uuid}</tds:SerialNumber>
<tds:HardwareId>ScreenCam-1</tds:HardwareId>
</tds:GetDeviceInformationResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        version = crate::VERSION,
        device_uuid = device_uuid,
    )
}

fn build_get_capabilities(local_ip: &str, onvif_port: u16) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<SOAP-ENV:Body>
<tds:GetCapabilitiesResponse>
<tds:Capabilities>
<tt:Device><tt:XAddr>http://{ip}:{port}/onvif/device_service</tt:XAddr></tt:Device>
<tt:Media>
<tt:XAddr>http://{ip}:{port}/onvif/media_service</tt:XAddr>
<tt:StreamingCapabilities>
<tt:RTPMulticast>false</tt:RTPMulticast>
<tt:RTP_TCP>true</tt:RTP_TCP>
<tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP>
</tt:StreamingCapabilities>
</tt:Media>
</tds:Capabilities>
</tds:GetCapabilitiesResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        ip = local_ip,
        port = onvif_port,
    )
}

fn build_get_system_date_and_time() -> String {
    let now = chrono::Utc::now();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<SOAP-ENV:Body>
<tds:GetSystemDateAndTimeResponse>
<tds:SystemDateAndTime>
<tt:DateTimeType>NTP</tt:DateTimeType>
<tt:DaylightSavings>false</tt:DaylightSavings>
<tt:UTCDateTime>
<tt:Time><tt:Hour>{h}</tt:Hour><tt:Minute>{mi}</tt:Minute><tt:Second>{s}</tt:Second></tt:Time>
<tt:Date><tt:Year>{y}</tt:Year><tt:Month>{mo}</tt:Month><tt:Day>{d}</tt:Day></tt:Date>
</tt:UTCDateTime>
</tds:SystemDateAndTime>
</tds:GetSystemDateAndTimeResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        h = now.format("%H"),
        mi = now.format("%M"),
        s = now.format("%S"),
        y = now.format("%Y"),
        mo = now.format("%m"),
        d = now.format("%d"),
    )
}

const PROFILE_TOKEN: &str = "profile_1";

fn build_get_profiles(width: usize, height: usize, plural: bool) -> String {
    // Dimensions come either from the ready descriptor or from the last
    // descriptor that reached SPS/PPS/IDR readiness. No invented resolution
    // is advertised before the first stream has been confirmed.
    //
    // The configurations are the same elements GetVideoSourceConfiguration(s)
    // and GetVideoEncoderConfiguration(s) return, down to the tokens — an NVR
    // that cross-checks the profile against those calls has to see one device,
    // not two descriptions of one.
    let (element, wrapper) = if plural {
        ("GetProfilesResponse", "Profiles")
    } else {
        ("GetProfileResponse", "Profile")
    };
    media_envelope(&format!(
        "<trt:{element}>\
         <trt:{wrapper} token=\"{token}\" fixed=\"true\">\
         <tt:Name>ScreenCam Main</tt:Name>\
         {source}\
         {encoder}\
         </trt:{wrapper}>\
         </trt:{element}>",
        element = element,
        wrapper = wrapper,
        token = PROFILE_TOKEN,
        source = video_source_configuration(width, height),
        encoder = video_encoder_configuration(width, height),
    ))
}

fn build_get_stream_uri(local_ip: &str, rtsp_port: u16) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<SOAP-ENV:Body>
<trt:GetStreamUriResponse>
<trt:MediaUri>
<tt:Uri>rtsp://{ip}:{port}/live/main</tt:Uri>
<tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>
<tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>
<tt:Timeout>PT60S</tt:Timeout>
</trt:MediaUri>
</trt:GetStreamUriResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        ip = local_ip,
        port = rtsp_port,
    )
}

fn build_soap_fault(reason: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope">
<SOAP-ENV:Body>
<SOAP-ENV:Fault>
<SOAP-ENV:Code><SOAP-ENV:Value>SOAP-ENV:Receiver</SOAP-ENV:Value></SOAP-ENV:Code>
<SOAP-ENV:Reason><SOAP-ENV:Text xml:lang="en">{reason}</SOAP-ENV:Text></SOAP-ENV:Reason>
</SOAP-ENV:Fault>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        reason = reason,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_epoch_ready(state: &SharedState, width: usize, height: usize) {
        let epoch = state.stream_epoch();
        assert!(state.set_stream_dimensions(epoch, width, height));
        assert!(
            state.apply_stream_access_unit(epoch, &[&[0x67, 0x64], &[0x68, 0xee], &[0x65, 0x88]])
        );
    }

    #[test]
    fn startup_without_confirmed_resolution_returns_a_controlled_fault() {
        let state = SharedState::new();

        let (status, body) = build_get_profiles_response(&state, true);

        assert_eq!(status, "503 Service Unavailable");
        assert!(body.contains("Video profile is not ready"));
        assert!(!body.contains("1920"));
        assert!(!body.contains("1080"));
    }

    #[test]
    fn reconstruction_uses_the_last_confirmed_resolution() {
        let state = SharedState::new();
        make_epoch_ready(&state, 1600, 900);
        state.invalidate_stream();

        let (status, body) = build_get_profiles_response(&state, true);

        assert_eq!(status, "200 OK");
        assert!(body.contains("<tt:Width>1600</tt:Width>"));
        assert!(body.contains("<tt:Height>900</tt:Height>"));
    }

    #[test]
    fn newly_confirmed_resolution_replaces_the_previous_one() {
        let state = SharedState::new();
        make_epoch_ready(&state, 1600, 900);
        state.invalidate_stream();
        make_epoch_ready(&state, 2560, 1440);

        let (status, body) = build_get_profiles_response(&state, true);

        assert_eq!(status, "200 OK");
        assert!(body.contains("<tt:Width>2560</tt:Width>"));
        assert!(body.contains("<tt:Height>1440</tt:Height>"));
        assert!(!body.contains("<tt:Width>1600</tt:Width>"));
    }
}
