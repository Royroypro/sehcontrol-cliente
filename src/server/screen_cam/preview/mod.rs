// Sehcontrol ScreenCam — SRT preview (Entrega 4, see docs/SCREENCAM_PLAN.md).
//
// The panel-driven preview publishes the already-encoded H.264 access units
// this module's siblings produce as MPEG-TS over SRT, so an administrator can
// watch through WebRTC/WHEP without the RTSP path being involved at all.
//
// Four pieces:
//
// - `control`: owns the validated preview session and the tap lifecycle.
// - `mpegts`: turns one access unit into transport stream packets.
// - `tap`: the bounded, lock-free hand-off the capture thread pushes encoded
//   access units into, and the publisher drains.
// - `publisher`: the worker that joins the other three, and the only one here
//   that owns a socket or an async runtime.
//
// The first three still know nothing about each other. `publisher` is what
// wires them together, and it is also the only place a failure can originate
// that the capture pipeline — and therefore RTSP and UDP — must be shielded
// from.
pub(super) mod control;
pub(super) mod mpegts;
pub(super) mod publisher;
// The tap exposes a small diagnostic surface (`with_capacity`, `len`,
// `capacity`, `from_vec`) that its own tests use and the publisher has no reason
// to call. `mpegts` and `publisher` carry no such allowance any more, so dead
// code in the new C1 paths is a warning rather than something silenced here.
#[allow(dead_code)]
pub(super) mod tap;
