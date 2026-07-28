// Minimal ONVIF support for Sehcontrol ScreenCam (docs/SCREENCAM_PLAN.md, Fase 6).
//
// Scope, deliberately: WS-Discovery (so an NVR's "search for cameras" finds
// this device on its own) + the smallest ONVIF device/media SOAP surface
// that lets an NVR go from "found it" to "here's the RTSP URL" without a
// human typing anything in by hand:
//   - GetSystemDateAndTime, GetDeviceInformation, GetCapabilities (device_service)
//   - GetProfiles, GetStreamUri (media_service)
//
// Explicitly NOT implemented yet (see docs/SCREENCAM_PLAN.md for what's
// deferred and why): WS-Security/auth on these SOAP calls (matches RTSP
// having no auth either), PTZ/Events/Imaging/Analytics services, WS-Discovery
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
use std::sync::atomic::Ordering;
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
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body_bytes = vec![0u8; content_length];
    reader.read_exact(&mut body_bytes)?;
    let body = String::from_utf8_lossy(&body_bytes);

    let local_ip = local_ip_for_peer(peer_addr).unwrap_or_else(|| "0.0.0.0".to_owned());
    log::debug!("[screencam][onvif] {path} from {peer_addr}");

    let (status, response_body) = if path.contains("device_service") {
        if body.contains("GetDeviceInformation") {
            ("200 OK", build_get_device_information(device_uuid))
        } else if body.contains("GetCapabilities") {
            ("200 OK", build_get_capabilities(&local_ip, onvif_port))
        } else if body.contains("GetSystemDateAndTime") {
            ("200 OK", build_get_system_date_and_time())
        } else {
            (
                "500 Internal Server Error",
                build_soap_fault("Action not supported"),
            )
        }
    } else if path.contains("media_service") {
        if body.contains("GetStreamUri") {
            ("200 OK", build_get_stream_uri(&local_ip, rtsp_port))
        } else if body.contains("GetProfiles") {
            let width = state.width.load(Ordering::Relaxed);
            let height = state.height.load(Ordering::Relaxed);
            ("200 OK", build_get_profiles(width, height))
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

fn build_get_profiles(width: usize, height: usize) -> String {
    // A monitor's real resolution isn't known until capture actually starts
    // (see SharedState::width/height in mod.rs) — fall back to a placeholder
    // so a client asking before then still gets a well-formed profile.
    let (width, height) = if width == 0 || height == 0 {
        (1920, 1080)
    } else {
        (width, height)
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<SOAP-ENV:Body>
<trt:GetProfilesResponse>
<trt:Profiles token="{token}" fixed="true">
<tt:Name>ScreenCam Main</tt:Name>
<tt:VideoSourceConfiguration token="video_src_1">
<tt:Name>ScreenCam Source</tt:Name>
<tt:SourceToken>video_src_1</tt:SourceToken>
<tt:Bounds x="0" y="0" width="{width}" height="{height}"/>
</tt:VideoSourceConfiguration>
<tt:VideoEncoderConfiguration token="video_enc_1">
<tt:Name>ScreenCam Encoder</tt:Name>
<tt:Encoding>H264</tt:Encoding>
<tt:Resolution><tt:Width>{width}</tt:Width><tt:Height>{height}</tt:Height></tt:Resolution>
</tt:VideoEncoderConfiguration>
</trt:Profiles>
</trt:GetProfilesResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        token = PROFILE_TOKEN,
        width = width,
        height = height,
    )
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
