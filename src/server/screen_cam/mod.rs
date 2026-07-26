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

mod rtp;
mod rtsp;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hbb_common::{anyhow::anyhow, bail, log, message_proto::video_frame, ResultType};
use scrap::{
    codec::{Encoder, EncoderApi, EncoderCfg},
    hwcodec::{HwRamEncoder, HwRamEncoderConfig},
    CodecFormat, Display, TraitCapturer,
};

use rtsp::Session;

pub struct ScreenCamConfig {
    pub monitor_index: usize,
    pub fps: u32,
    pub rtsp_port: u16,
    /// 0.0-1.0, forwarded to the same quality->bitrate curve the remote
    /// desktop encoders already use (see HwRamEncoder::bitrate in
    /// libs/scrap/src/common/hwcodec.rs) — 0.5 lands in the "Media" range
    /// the plan's UI mock calls for.
    pub quality: f32,
}

impl Default for ScreenCamConfig {
    fn default() -> Self {
        Self {
            monitor_index: 0,
            fps: 10,
            rtsp_port: 8554,
            quality: 0.5,
        }
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
pub fn start(cfg: ScreenCamConfig) {
    std::thread::spawn(move || {
        if let Err(e) = run(cfg) {
            log::error!("[screencam] stopped: {e:?}");
        }
    });
}

fn run(cfg: ScreenCamConfig) -> ResultType<()> {
    let state = Arc::new(SharedState::new());
    rtsp::start_listener(cfg.rtsp_port, state.clone())?;

    let (encoder_name, encoder_mc_name) = wait_for_h264_encoder(Duration::from_secs(35))
        .ok_or_else(|| {
            anyhow!(
                "no_h264_encoder: no hardware H.264 encoder detected on this machine \
                 (needs a working NVENC/QuickSync/AMF/VAAPI driver — see \
                 docs/SCREENCAM_PLAN.md §3.1, option A has no software fallback)"
            )
        })?;
    log::info!("[screencam] using hardware encoder: {}", encoder_name);

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

    loop {
        let loop_start = Instant::now();
        match capturer.frame(spf) {
            Ok(frame) => {
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
                log::error!("[screencam] capture error: {e}, will keep retrying");
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
