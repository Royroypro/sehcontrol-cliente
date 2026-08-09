// Bounded hand-off of encoded access units from the capture thread to the
// preview publisher.
//
// The capture loop runs on a per-frame time budget (it sleeps whatever is
// left of the frame interval), so nothing it calls may block: no lock it can
// contend on, no allocation it has to wait for, no channel that applies
// backpressure. This module is therefore built entirely out of a bounded
// lock-free queue plus atomics.
//
// The producer side is **non-blocking and lock-free**, not wait-free. See
// [`PreviewTap::push`] for exactly what that does and does not promise.
//
// The queue deliberately loses data. It keeps the most recent access units
// and evicts the oldest, because a preview that is 8 frames behind is worse
// than a preview that skipped ahead. Loss is reported through a monotonic
// counter rather than a flag: a `dropped_total` that moved is impossible to
// miss, whereas a boolean can be cleared by whoever reads it last.
//
// The consumer must not spin looking for work either, so the tap carries an
// `AtomicWaker`: an accepted push wakes whoever registered. `AtomicWaker` is a
// CAS state machine, so this keeps the producer's no-lock promise below intact —
// it is deliberately not a `Condvar` and not a channel.
//
// What this module deliberately does *not* do: it never inspects the H.264
// beyond checking that the buffer starts like Annex-B, never caches SPS/PPS,
// never decides when the stream is decodable again, and knows nothing about
// MPEG-TS, SRT, sessions or tokens. Those belong to the publisher that will
// sit on the consuming end.

use std::fmt;
use std::sync::atomic::{self, AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::Waker;

use crossbeam_queue::ArrayQueue;
use hbb_common::futures::task::AtomicWaker;

/// Access units the tap holds before it starts evicting. Two thirds of a
/// second at the ~12 fps ScreenCam captures at — long enough to ride out a
/// scheduling hiccup on the publisher side, short enough that the preview
/// never drifts far behind the screen.
pub(crate) const DEFAULT_CAPACITY: usize = 8;

/// Why an access unit could not be built. Carries no bytes from the frame, so
/// it is safe to log and to forward to the panel verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccessUnitError {
    /// `pts_ms` was below zero; the 90 kHz time base downstream is unsigned.
    NegativePts,
    /// The buffer had no bytes at all.
    EmptyPayload,
    /// The buffer did not begin with an Annex-B start code.
    InvalidAnnexB,
}

impl fmt::Display for AccessUnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NegativePts => "negative presentation timestamp",
            Self::EmptyPayload => "empty access unit",
            Self::InvalidAnnexB => "access unit is not Annex-B",
        })
    }
}

impl std::error::Error for AccessUnitError {}

/// A capacity of zero would make the queue useless and `ArrayQueue::new(0)`
/// panics, so it is rejected up front instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ZeroCapacity;

impl fmt::Display for ZeroCapacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("preview tap capacity must be at least one access unit")
    }
}

impl std::error::Error for ZeroCapacity {}

/// One complete H.264 access unit as the encoder produced it, plus the little
/// metadata the publisher needs to decide what to do with it.
///
/// `annexb` is an `Arc<[u8]>` so the capture thread hands over ownership once
/// and every later clone — into the queue, out of the queue, into the muxer —
/// only touches a refcount. Nothing in here identifies a session: no token,
/// no URL, no stream name, no session or machine id. The publisher pairs
/// these with its own session state; the tap never sees it.
#[derive(Clone)]
pub(crate) struct EncodedAccessUnit {
    /// Stream epoch this unit belongs to, as `SharedState` counts them. The
    /// consumer drops anything whose epoch is stale rather than trusting
    /// queue order.
    pub(crate) epoch: u64,
    pub(crate) pts_ms: i64,
    pub(crate) keyframe: bool,
    /// Whether the unit carries its own parameter sets. Computed by the
    /// capture side, which already splits the NALs — the tap does not parse.
    pub(crate) has_sps: bool,
    pub(crate) has_pps: bool,
    pub(crate) annexb: Arc<[u8]>,
}

/// Prints the metadata and the payload *length* only. A derived `Debug` would
/// dump every byte of the frame into the log the first time someone writes
/// `{:?}`, which is both unreadable and a way to leak screen contents.
impl fmt::Debug for EncodedAccessUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedAccessUnit")
            .field("epoch", &self.epoch)
            .field("pts_ms", &self.pts_ms)
            .field("keyframe", &self.keyframe)
            .field("has_sps", &self.has_sps)
            .field("has_pps", &self.has_pps)
            .field("annexb_len", &self.annexb.len())
            .finish()
    }
}

impl EncodedAccessUnit {
    /// Takes ownership of an existing `Arc`, which is the shape the capture
    /// path will hand over — no copy of the payload happens here.
    pub(crate) fn from_arc(
        epoch: u64,
        pts_ms: i64,
        keyframe: bool,
        has_sps: bool,
        has_pps: bool,
        annexb: Arc<[u8]>,
    ) -> Result<Self, AccessUnitError> {
        validate_payload(pts_ms, &annexb)?;
        Ok(Self {
            epoch,
            pts_ms,
            keyframe,
            has_sps,
            has_pps,
            annexb,
        })
    }

    /// Convenience for a payload that was just built. `Vec<u8> -> Arc<[u8]>`
    /// copies once, unavoidably; prefer [`Self::from_arc`] on the hot path.
    pub(crate) fn from_vec(
        epoch: u64,
        pts_ms: i64,
        keyframe: bool,
        has_sps: bool,
        has_pps: bool,
        annexb: Vec<u8>,
    ) -> Result<Self, AccessUnitError> {
        Self::from_arc(epoch, pts_ms, keyframe, has_sps, has_pps, annexb.into())
    }

    /// Tests only: the capture path never has a bare slice, so offering this
    /// in production would only invite an extra copy.
    #[cfg(test)]
    fn from_slice(
        epoch: u64,
        pts_ms: i64,
        keyframe: bool,
        has_sps: bool,
        has_pps: bool,
        annexb: &[u8],
    ) -> Result<Self, AccessUnitError> {
        Self::from_arc(epoch, pts_ms, keyframe, has_sps, has_pps, annexb.into())
    }

    pub(crate) fn len(&self) -> usize {
        self.annexb.len()
    }

    /// Always `false` for any access unit that exists: the constructors
    /// reject an empty payload, and there is no other way to build one. It is
    /// here to complete the contract alongside [`Self::len`] rather than to
    /// describe a state that can occur.
    pub(crate) fn is_empty(&self) -> bool {
        self.annexb.is_empty()
    }
}

fn validate_payload(pts_ms: i64, annexb: &[u8]) -> Result<(), AccessUnitError> {
    if pts_ms < 0 {
        return Err(AccessUnitError::NegativePts);
    }
    if annexb.is_empty() {
        return Err(AccessUnitError::EmptyPayload);
    }
    if !starts_with_start_code(annexb) {
        return Err(AccessUnitError::InvalidAnnexB);
    }
    Ok(())
}

/// Same contract as the muxer's own check, kept here as a few lines rather
/// than a shared dependency so the tap stays independent of it: any number of
/// `leading_zero_8bits`, then a 3- or 4-byte start code, then at least one
/// byte of NAL. Rejecting here means a malformed buffer never reaches the
/// queue; the muxer checks again because it is a separate contract.
fn starts_with_start_code(data: &[u8]) -> bool {
    // Bounded by the buffer length, so this cannot run away.
    let mut index = 0usize;
    while index < data.len() && data[index] == 0x00 {
        index += 1;
    }
    if index < 2 || index >= data.len() || data[index] != 0x01 {
        return false;
    }
    index + 1 < data.len()
}

/// What [`PreviewTap::push`] did with the access unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TapPushResult {
    /// Queued with room to spare.
    Queued,
    /// Queued, but the oldest access unit had to be evicted to make room.
    /// `dropped_total` is the counter's new value, so a publisher that keeps
    /// the last one it saw can tell exactly that it fell behind.
    QueuedAfterDrop { dropped_total: u64 },
    /// No session is publishing; the access unit was discarded without being
    /// counted as loss, because nothing was expecting it.
    IgnoredInactive,
}

/// Outcome of [`PreviewTap::activate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TapActivation {
    /// False when the tap was already active, in which case nothing changed
    /// and no queued access unit was touched.
    pub(crate) activated: bool,
    /// Leftovers from a previous session that were cleared out. Deliberate
    /// housekeeping, never counted as loss.
    pub(crate) discarded: u64,
}

/// Outcome of [`PreviewTap::deactivate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TapDeactivation {
    /// False when the tap was already inactive.
    pub(crate) deactivated: bool,
    pub(crate) discarded: u64,
}

/// Outcome of [`PreviewTap::invalidate_stream`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TapInvalidation {
    pub(crate) epoch: u64,
    pub(crate) generation: u64,
    /// Access units of the previous epoch that were thrown away. Counted
    /// separately from saturation loss — this is not the publisher falling
    /// behind, it is the stream being replaced underneath it.
    pub(crate) discarded: u64,
}

/// A coherent `(epoch, generation)` pair, as read through the seqlock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InvalidationSnapshot {
    pub(crate) epoch: u64,
    /// Bumped by every invalidation, including a repeat of the same epoch.
    /// The consumer stores the last value it acted on and compares; that is
    /// what makes an invalidation impossible to miss, unlike a flag that
    /// whoever reads it first would clear.
    pub(crate) generation: u64,
}

/// Metrics only — no payload, no timestamps, no session data. Built fresh on
/// every call and never shared, so a caller cannot hold a view that mutates
/// under it.
///
/// **Diagnostic, not a global transaction.** `invalidated_epoch` and
/// `invalidation_generation` are read together through the seqlock and are
/// mutually coherent; every other field is sampled at a slightly different
/// instant, so `active` and `queued` may describe moments a few nanoseconds
/// apart and should not be compared as if they were captured atomically.
/// `queued <= capacity` still holds unconditionally, and taking a snapshot
/// neither drains the queue nor blocks anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreviewTapStats {
    pub(crate) active: bool,
    pub(crate) queued: usize,
    pub(crate) capacity: usize,
    pub(crate) dropped_total: u64,
    pub(crate) invalidated_epoch: u64,
    pub(crate) invalidation_generation: u64,
    pub(crate) discarded_on_invalidate_total: u64,
}

/// The bounded hand-off itself. Shared between the capture thread (which only
/// ever calls [`Self::push`]) and the publisher thread (which pops, reads
/// stats and drives activation), typically behind an `Arc`.
///
/// # Protocol the consumer must follow
///
/// The tap does no filtering of its own. It hands over whatever is in the
/// queue and reports what happened; deciding which access units are still
/// usable is the consumer's job, and the order below is not optional:
///
/// 1. take an access unit with [`Self::pop`];
/// 2. read [`Self::invalidation_snapshot`] **after** that `pop`, never before;
/// 3. compare the *generation* first — a new generation means the stream was
///    replaced, and it is the only way to notice an invalidation that reused
///    the same epoch;
/// 4. then compare `au.epoch` against the snapshot's `epoch`;
/// 5. discard the access unit if either check says it is stale;
/// 6. compare [`Self::dropped_total`] against the last value observed;
/// 7. if it moved, enter resynchronization — part of a GOP is missing;
/// 8. while resynchronizing, publish nothing until a keyframe arrives whose
///    parameter sets are valid for the current epoch.
///
/// Reading the snapshot before the `pop` would leave a window in which an
/// invalidation lands between the two and the consumer publishes an access
/// unit from the epoch that was just replaced.
///
/// **FIFO order is not evidence of epoch membership.** A producer already
/// inside `push` when [`Self::invalidate_stream`] ran can deposit an old
/// access unit *after* the drain, so a stale unit can sit at the head of the
/// queue. That is precisely why each unit carries its own `epoch` instead of
/// having it inferred from its position.
pub(crate) struct PreviewTap {
    queue: ArrayQueue<EncodedAccessUnit>,
    active: AtomicBool,
    dropped_total: AtomicU64,
    invalidated_epoch: AtomicU64,
    invalidation_generation: AtomicU64,
    discarded_on_invalidate_total: AtomicU64,
    /// Seqlock version guarding the `(invalidated_epoch,
    /// invalidation_generation)` pair. Even means stable, odd means a writer
    /// is between the two stores. Two independent atomics cannot be read as a
    /// coherent pair on their own, and a `Mutex` is not an option on this
    /// path, so the pair is versioned instead.
    invalidation_version: AtomicU64,
    /// Wakes the single consumer when there is something to do: an access unit
    /// was accepted, the tap was closed, or the stream was replaced. Only one
    /// waker is ever held, which matches the one-publisher design; registering
    /// from two consumers would simply lose one of them.
    waker: AtomicWaker,
}

impl PreviewTap {
    pub(crate) fn new() -> Self {
        Self::build(DEFAULT_CAPACITY)
    }

    /// Fallible because a zero-capacity queue cannot exist. Returns an error
    /// rather than panicking so a bad configuration value can never take the
    /// capture process down.
    pub(crate) fn with_capacity(capacity: usize) -> Result<Self, ZeroCapacity> {
        if capacity == 0 {
            return Err(ZeroCapacity);
        }
        Ok(Self::build(capacity))
    }

    fn build(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
            active: AtomicBool::new(false),
            dropped_total: AtomicU64::new(0),
            invalidated_epoch: AtomicU64::new(0),
            invalidation_generation: AtomicU64::new(0),
            discarded_on_invalidate_total: AtomicU64::new(0),
            invalidation_version: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        }
    }

    /// Opens the tap for a preview session, clearing anything a previous one
    /// left behind so the publisher never starts on stale frames.
    ///
    /// Idempotent: calling it while already active reports `activated: false`
    /// and leaves the queued access units alone — a second START for the same
    /// session must not throw away live video.
    ///
    /// The drain happens while the tap is still inactive and the flip to
    /// active is the linearization point, so no access unit of the *new*
    /// session can be swept away: `push` refuses everything until the flag is
    /// set, and everything it accepts afterwards is necessarily later than
    /// the drain. Residue left by the previous session (see [`Self::deactivate`])
    /// is therefore cleared here and reported in `discarded`.
    ///
    /// **Single controller.** Like [`Self::deactivate`], this is meant to be
    /// driven by one owner of the publisher's session lifecycle. `push` and
    /// `pop` remain safe for any number of threads, but `TapActivation`,
    /// `TapDeactivation` and the split between deliberate and accidental
    /// discards only read correctly when a single controller sequences the
    /// session transitions. This is a contract, not a runtime check.
    pub(crate) fn activate(&self) -> TapActivation {
        // Acquire pairs with the Release in `deactivate`, so the queue state
        // left by the previous session is visible before it is drained.
        if self.active.load(Ordering::Acquire) {
            return TapActivation {
                activated: false,
                discarded: 0,
            };
        }
        // Nothing can be added while inactive — `push` ignores frames then —
        // so the queue is static here. Drain first, flip second: the reverse
        // order could discard a frame of the new session.
        let discarded = self.drain_queue();
        let activated = self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        TapActivation {
            activated,
            discarded,
        }
    }

    /// Closes the tap and drains the queue. Idempotent. Afterwards `push`
    /// ignores everything until the next [`Self::activate`].
    ///
    /// **This does not promise an empty queue on return.** The sequence is:
    ///
    /// 1. `active` is set to `false`;
    /// 2. every access unit visible in the queue at that moment is drained;
    /// 3. a producer that had *already* read `active == true` may complete
    ///    its `force_push` afterwards;
    /// 4. so one access unit — bounded by the capacity if several producers
    ///    race — can still be sitting there when this returns, and
    ///    `stats().queued` can be non-zero while `active` is `false`.
    ///
    /// That residue always belongs to the session being closed and must
    /// never be treated as part of a future one. It cannot leak into the next
    /// session: [`Self::activate`] drains before it publishes `active == true`,
    /// so a fresh publisher never sees it.
    ///
    /// Accounting follows the same rule. `TapDeactivation::discarded` counts
    /// only what *this* call drained; a unit that lands afterwards is counted
    /// later, in `TapActivation::discarded`. Both are deliberate housekeeping
    /// and neither touches `dropped_total`, so the same access unit can be
    /// attributed to either bucket depending on the race — what never changes
    /// is that it is not reported as saturation loss.
    ///
    /// Draining twice would only narrow the window, not close it, so it is
    /// deliberately not done: the race is inherent to a check-then-act on a
    /// lock-free queue and is documented rather than papered over.
    ///
    /// **Single controller** — see [`Self::activate`].
    pub(crate) fn deactivate(&self) -> TapDeactivation {
        // AcqRel: the store must be visible to producers before the drain, so
        // they stop feeding what is about to be thrown away.
        if !self.active.swap(false, Ordering::AcqRel) {
            return TapDeactivation {
                deactivated: false,
                discarded: 0,
            };
        }
        let discarded = self.drain_queue();
        // A consumer parked waiting for frames has to learn the tap closed,
        // otherwise it would sit there until the session's expiry instead of
        // shutting down when told to.
        self.waker.wake();
        TapDeactivation {
            deactivated: true,
            discarded,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Hands one access unit over.
    ///
    /// **Non-blocking and lock-free — deliberately not claimed to be
    /// wait-free.** What is guaranteed: no `Mutex`, `RwLock` or `Condvar` is
    /// taken; no I/O happens; nothing is allocated during the call (the
    /// payload was allocated by the caller and only its `Arc` moves here);
    /// and the call never waits on another thread that could be descheduled
    /// while holding something.
    ///
    /// What is *not* guaranteed: `ArrayQueue::force_push` is a CAS loop, so
    /// under contention it can spin, and crossbeam's internal backoff may
    /// escalate to `std::thread::yield_now()` when a concurrent producer or
    /// consumer has moved the queue's tail but not yet published the slot's
    /// stamp. There is therefore no strict upper bound on the latency of a
    /// single call. In the intended shape — one capture thread pushing, one
    /// publisher thread popping — contention is rare and a yield costs far
    /// less than the frame interval, but a caller must not treat this as a
    /// hard real-time operation.
    ///
    /// When the queue is full the oldest access unit is evicted and
    /// `dropped_total` advances by exactly one, so the publisher learns it
    /// lost part of a GOP and can wait for the next keyframe.
    pub(crate) fn push(&self, access_unit: EncodedAccessUnit) -> TapPushResult {
        // Acquire pairs with `activate`'s Release-flavoured CAS: seeing the
        // flag set means the drain that preceded it has already happened.
        if !self.active.load(Ordering::Acquire) {
            return TapPushResult::IgnoredInactive;
        }
        let result = match self.queue.force_push(access_unit) {
            // Room was available; nothing was lost.
            None => TapPushResult::Queued,
            // The evicted unit is dropped right here: one `Arc` decrement,
            // never a copy of the payload.
            Some(_evicted) => TapPushResult::QueuedAfterDrop {
                dropped_total: saturating_increment(&self.dropped_total),
            },
        };
        // After the unit is visible in the queue, never before: the consumer
        // re-checks the queue when it wakes, so waking first could only cost a
        // spurious poll, but waking last is what makes a wakeup impossible to
        // lose. Nothing is woken when the push was ignored, which is the common
        // case while nobody previews.
        self.waker.wake();
        result
    }

    /// Registers the consumer's waker. Call it between draining the queue and
    /// awaiting, then drain once more before actually suspending — see
    /// [`Self::pop`]'s protocol — or a push that lands in the gap is missed.
    pub(crate) fn register_waker(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    /// The capture path's entry point: copies `annexb` into the tap, but only
    /// when a session is actually publishing.
    ///
    /// While the tap is inactive — which is the normal state, since nobody is
    /// previewing most of the time — the capture thread pays exactly one
    /// atomic load and returns. No validation, no allocation, no copy of the
    /// frame. That is the whole reason this helper exists instead of the
    /// caller building an [`EncodedAccessUnit`] first: constructing one would
    /// copy the payload before anyone could ask whether it was wanted.
    ///
    /// When the tap *is* active, exactly one materialization happens: the
    /// slice is turned straight into an `Arc<[u8]>`. That copy is unavoidable
    /// while the encoder lends a buffer it reuses for the next frame, and it
    /// is the only one — no intermediate `Vec`, no `to_vec().into()`, and no
    /// clone of the `Arc` before it reaches [`Self::push`]. Everything
    /// downstream (queue, eviction, `pop`, the publisher) moves refcounts.
    ///
    /// Errors are the ordinary sanitary ones from the constructor and carry
    /// no bytes of the frame. The caller is expected to ignore them and carry
    /// on: a frame the preview cannot take is not a reason to disturb
    /// anything else.
    pub(crate) fn push_annexb_copy_if_active(
        &self,
        epoch: u64,
        pts_ms: i64,
        keyframe: bool,
        has_sps: bool,
        has_pps: bool,
        annexb: &[u8],
    ) -> Result<TapPushResult, AccessUnitError> {
        // Checked first, before anything can allocate or validate.
        if !self.active.load(Ordering::Acquire) {
            return Ok(TapPushResult::IgnoredInactive);
        }
        let access_unit =
            EncodedAccessUnit::from_arc(epoch, pts_ms, keyframe, has_sps, has_pps, annexb.into())?;
        Ok(self.push(access_unit))
    }

    /// Takes the oldest access unit still held, or `None` when empty. Never
    /// blocks and never touches the loss counters — consuming is not losing.
    ///
    /// To wait for the next one without spinning, the order is not optional:
    ///
    /// 1. drain with `pop` until it returns `None`;
    /// 2. [`Self::register_waker`];
    /// 3. `pop` again, and re-check [`Self::is_active`];
    /// 4. only then suspend.
    ///
    /// Registering before the final drain is what closes the window in which a
    /// push lands between the two and its wakeup goes to nobody.
    pub(crate) fn pop(&self) -> Option<EncodedAccessUnit> {
        self.queue.pop()
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.queue.capacity()
    }

    pub(crate) fn dropped_total(&self) -> u64 {
        // Relaxed: this counter only ever grows, and the publisher compares
        // it against the last value it saw. Being one observation behind
        // costs a slightly late resync, never a missed one.
        self.dropped_total.load(Ordering::Relaxed)
    }

    /// Announces that the capture stream was replaced — a new epoch, after a
    /// display change, an encoder rebuild or a watchdog restart.
    ///
    /// The event cannot be lost. It is published through atomics rather than
    /// pushed into the bounded queue, so a full queue has no way of dropping
    /// it, and it is a monotonic generation rather than a flag, so no reader
    /// can clear it before another has seen it. Repeated calls are fine, and
    /// epochs need not be consecutive — `10 -> 12` is accepted, and even
    /// `12 -> 12` bumps the generation, because it is a new explicit event.
    ///
    /// One caveat on "cannot be lost", stated precisely rather than left as
    /// an absolute: the generation saturates instead of wrapping (wrapping
    /// would let it collide with a value a consumer already acted on). Once
    /// it reaches `u64::MAX` it stays there, and a consumer comparing only
    /// generations can no longer tell that a later invalidation happened —
    /// an invalidation that also repeats the epoch becomes invisible. At one
    /// invalidation per second that ceiling is some 5.8e11 years away, so it
    /// is unreachable for any real process; it is recorded here because the
    /// counter's monotonicity is a practical guarantee, not a mathematically
    /// unbounded one.
    ///
    /// Does not deactivate the tap: the session is still live, only the
    /// stream underneath it changed.
    ///
    /// The queue is drained *after* the pair is published, so a producer
    /// already inside `push` can leave a stale-epoch access unit behind — see
    /// the consumer protocol on [`PreviewTap`].
    pub(crate) fn invalidate_stream(&self, new_epoch: u64) -> TapInvalidation {
        // Seqlock write. The critical section is two atomic stores with no
        // user code in it, so a concurrent writer's wait here is bounded by a
        // handful of instructions — this is not a lock held across work.
        let mut version = self.invalidation_version.load(Ordering::Relaxed);
        loop {
            if version & 1 != 0 {
                // Another writer is mid-update; let it finish.
                std::hint::spin_loop();
                version = self.invalidation_version.load(Ordering::Relaxed);
                continue;
            }
            match self.invalidation_version.compare_exchange_weak(
                version,
                version.wrapping_add(1),
                // Acquire: nothing below may be hoisted above the claim.
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => version = actual,
            }
        }

        // Inside the odd-versioned window. Relaxed is enough for the pair
        // itself; the Release store below publishes both at once.
        self.invalidated_epoch.store(new_epoch, Ordering::Relaxed);
        let generation = saturating_increment(&self.invalidation_generation);
        self.invalidation_version
            .store(version.wrapping_add(2), Ordering::Release);

        // Drained after publishing, so a reader that sees the new generation
        // never also sees the queue still holding the old epoch as "current".
        // A producer racing in between can still deposit one stale unit — the
        // epoch each access unit carries is what settles that, which is why
        // it is part of the model rather than implied by queue position.
        let discarded = self.drain_queue();
        saturating_add(&self.discarded_on_invalidate_total, discarded);
        // A parked consumer must notice the replacement even if the new epoch
        // has not produced a frame yet, so it can reset its muxer and go back
        // to waiting for a keyframe instead of publishing across the seam.
        self.waker.wake();
        TapInvalidation {
            epoch: new_epoch,
            generation,
            discarded,
        }
    }

    /// Reads the `(epoch, generation)` pair back coherently: retries while a
    /// writer holds an odd version, and rejects a read whose version moved
    /// underneath it. A reader can therefore never see a new epoch paired
    /// with an old generation, or the reverse.
    pub(crate) fn invalidation_snapshot(&self) -> InvalidationSnapshot {
        loop {
            let before = self.invalidation_version.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let epoch = self.invalidated_epoch.load(Ordering::Relaxed);
            let generation = self.invalidation_generation.load(Ordering::Relaxed);
            // Acquire fence: the two loads above must not be reordered past
            // this re-read of the version, or the check would prove nothing.
            atomic::fence(Ordering::Acquire);
            if self.invalidation_version.load(Ordering::Relaxed) == before {
                return InvalidationSnapshot { epoch, generation };
            }
            std::hint::spin_loop();
        }
    }

    /// A fresh, owned snapshot of the metrics. Does not drain, does not block
    /// and shares nothing with the tap afterwards. Diagnostic and approximate
    /// across fields — see [`PreviewTapStats`] for exactly which parts are
    /// coherent with each other.
    pub(crate) fn stats(&self) -> PreviewTapStats {
        let invalidation = self.invalidation_snapshot();
        PreviewTapStats {
            active: self.active.load(Ordering::Acquire),
            queued: self.queue.len(),
            capacity: self.queue.capacity(),
            dropped_total: self.dropped_total.load(Ordering::Relaxed),
            invalidated_epoch: invalidation.epoch,
            invalidation_generation: invalidation.generation,
            discarded_on_invalidate_total: self
                .discarded_on_invalidate_total
                .load(Ordering::Relaxed),
        }
    }

    /// Tests only: deposits an access unit straight into the queue, bypassing
    /// the `active` check. Models the tail end of a `push` that had already
    /// read `active == true` before [`Self::deactivate`] flipped it — the one
    /// interleaving that leaves residue behind, which has no seam to force in
    /// a real `push`. Never reachable from production code.
    #[cfg(test)]
    fn deposit_bypassing_activation(&self, access_unit: EncodedAccessUnit) {
        self.queue.force_push(access_unit);
    }

    /// Empties the queue, returning how many access units went. Bounded by
    /// the capacity rather than looping until empty, so a producer that keeps
    /// feeding concurrently cannot hold the caller here.
    fn drain_queue(&self) -> u64 {
        let mut discarded = 0u64;
        for _ in 0..self.queue.capacity() {
            if self.queue.pop().is_none() {
                break;
            }
            discarded = discarded.saturating_add(1);
        }
        discarded
    }
}

impl Default for PreviewTap {
    fn default() -> Self {
        Self::new()
    }
}

/// Metrics only, and never the queue contents.
impl fmt::Debug for PreviewTap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stats = self.stats();
        f.debug_struct("PreviewTap")
            .field("active", &stats.active)
            .field("queued", &stats.queued)
            .field("capacity", &stats.capacity)
            .field("dropped_total", &stats.dropped_total)
            .field("invalidated_epoch", &stats.invalidated_epoch)
            .field("invalidation_generation", &stats.invalidation_generation)
            .field(
                "discarded_on_invalidate_total",
                &stats.discarded_on_invalidate_total,
            )
            .finish()
    }
}

/// These counters are process-lifetime metrics, so they saturate at
/// `u64::MAX` instead of wrapping: a counter that silently returned to zero
/// would read as "nothing was lost" to a publisher watching for an increase.
/// `fetch_add` is not usable for that reason. Relaxed throughout — the values
/// are independent of any other memory, and only their monotonicity matters.
fn saturating_add(counter: &AtomicU64, amount: u64) -> u64 {
    let previous = match counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    }) {
        // The closure never returns None, so both arms carry the value the
        // update started from; matching both keeps this total, with no
        // unwrap on a path the capture thread runs.
        Ok(previous) | Err(previous) => previous,
    };
    previous.saturating_add(amount)
}

fn saturating_increment(counter: &AtomicU64) -> u64 {
    saturating_add(counter, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::{Duration, Instant};

    /// An access unit whose payload is `len` bytes and whose tail is filled
    /// with `marker`, so a consumer can prove which producer built it and
    /// that the bytes survived intact.
    fn unit(epoch: u64, pts_ms: i64, len: usize, marker: u8) -> EncodedAccessUnit {
        let mut payload = vec![0x00, 0x00, 0x00, 0x01, 0x65];
        payload.resize(len.max(6), marker);
        EncodedAccessUnit::from_vec(epoch, pts_ms, false, false, false, payload)
            .expect("fixture must be valid")
    }

    fn active_tap(capacity: usize) -> PreviewTap {
        let tap = PreviewTap::with_capacity(capacity).expect("non-zero capacity");
        assert!(tap.activate().activated);
        tap
    }

    // --- EncodedAccessUnit ------------------------------------------------

    #[test]
    fn cloning_shares_the_payload_instead_of_copying_it() {
        let access_unit = unit(1, 0, 4096, 0x5A);
        let clone = access_unit.clone();
        assert!(
            Arc::ptr_eq(&access_unit.annexb, &clone.annexb),
            "clone must share the same allocation"
        );
        assert_eq!(Arc::strong_count(&access_unit.annexb), 2);
        drop(clone);
        assert_eq!(Arc::strong_count(&access_unit.annexb), 1);
    }

    #[test]
    fn from_arc_takes_ownership_without_copying() {
        let payload: Arc<[u8]> = vec![0, 0, 0, 1, 0x65, 0xAB].into();
        let address = payload.as_ptr();
        let access_unit =
            EncodedAccessUnit::from_arc(7, 40, true, true, true, payload).expect("valid");
        assert_eq!(access_unit.annexb.as_ptr(), address);
        assert_eq!(access_unit.epoch, 7);
        assert_eq!(access_unit.pts_ms, 40);
        assert!(access_unit.keyframe && access_unit.has_sps && access_unit.has_pps);
    }

    #[test]
    fn from_vec_builds_a_unit() {
        let access_unit =
            EncodedAccessUnit::from_vec(2, 33, false, false, false, vec![0, 0, 1, 0x41, 0x9A])
                .expect("valid");
        assert_eq!(access_unit.len(), 5);
        assert_eq!(&*access_unit.annexb, &[0, 0, 1, 0x41, 0x9A]);
    }

    #[test]
    fn a_valid_access_unit_is_never_empty() {
        let access_unit = unit(1, 0, 64, 0x11);
        assert!(!access_unit.is_empty());
        assert_eq!(access_unit.len(), 64);
        // The smallest thing any constructor will accept still has a payload.
        let smallest = EncodedAccessUnit::from_slice(0, 0, false, false, false, &[0, 0, 1, 0x65])
            .expect("valid");
        assert!(!smallest.is_empty());
        assert_eq!(smallest.len(), 4);
        // And there is no way to build an empty one.
        assert_eq!(
            EncodedAccessUnit::from_vec(0, 0, false, false, false, Vec::new()).err(),
            Some(AccessUnitError::EmptyPayload)
        );
        assert_eq!(
            EncodedAccessUnit::from_arc(0, 0, false, false, false, Vec::new().into()).err(),
            Some(AccessUnitError::EmptyPayload)
        );
        assert_eq!(
            EncodedAccessUnit::from_slice(0, 0, false, false, false, &[]).err(),
            Some(AccessUnitError::EmptyPayload)
        );
    }

    #[test]
    fn negative_pts_is_rejected() {
        assert_eq!(
            EncodedAccessUnit::from_slice(0, -1, false, false, false, &[0, 0, 1, 0x65]).err(),
            Some(AccessUnitError::NegativePts)
        );
        assert_eq!(
            EncodedAccessUnit::from_slice(0, i64::MIN, false, false, false, &[0, 0, 1, 0x65]).err(),
            Some(AccessUnitError::NegativePts)
        );
    }

    #[test]
    fn empty_payload_is_rejected() {
        assert_eq!(
            EncodedAccessUnit::from_slice(0, 0, false, false, false, &[]).err(),
            Some(AccessUnitError::EmptyPayload)
        );
    }

    #[test]
    fn invalid_annexb_is_rejected() {
        for payload in [
            vec![0x65, 0x88],                   // no start code
            vec![0xFF, 0x00, 0x00, 0x01, 0x65], // non-zero byte in front
            vec![0x00, 0x00, 0x01],             // start code with no NAL
            vec![0x00, 0x00, 0x00, 0x01],       // 4-byte start code, no NAL
            vec![0x00, 0x00, 0x00, 0x00, 0x01], // padded start code, no NAL
            vec![0x00, 0x01, 0x65],             // only one leading zero
            vec![0x00, 0x00, 0x02, 0x01, 0x65], // not a start code
            vec![0x00; 32],                     // nothing but zeros
            vec![0x00],
        ] {
            assert_eq!(
                EncodedAccessUnit::from_slice(0, 0, false, false, false, &payload).err(),
                Some(AccessUnitError::InvalidAnnexB),
                "should reject {payload:02X?}"
            );
        }
    }

    #[test]
    fn both_start_code_lengths_and_leading_zeros_are_accepted() {
        for payload in [
            vec![0x00, 0x00, 0x01, 0x65],
            vec![0x00, 0x00, 0x00, 0x01, 0x65],
            vec![0x00, 0x00, 0x00, 0x00, 0x01, 0x65],
            vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x67, 0x42],
        ] {
            let access_unit =
                EncodedAccessUnit::from_slice(0, 0, false, false, false, &payload).expect("valid");
            assert_eq!(
                &*access_unit.annexb,
                &payload[..],
                "the payload must cross untouched"
            );
        }
    }

    #[test]
    fn debug_never_prints_the_payload() {
        // A pattern that would be unmistakable in the output if the bytes
        // ever leaked into a log line.
        let mut payload = vec![0x00, 0x00, 0x00, 0x01, 0x65];
        payload.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE]);
        payload.resize(512, 0x7B);
        let access_unit =
            EncodedAccessUnit::from_slice(3, 99, true, true, false, &payload).expect("valid");

        let rendered = format!("{access_unit:?}");
        for byte in [0xDEu8, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0x7B] {
            assert!(
                !rendered.contains(&format!("{byte}")),
                "Debug leaked byte {byte:#04X}: {rendered}"
            );
        }
        assert!(rendered.contains("annexb_len: 512"));
        assert!(rendered.contains("epoch: 3"));
        assert!(rendered.contains("pts_ms: 99"));
        assert!(rendered.contains("keyframe: true"));
        assert!(!rendered.contains("annexb: ["), "no payload slice");
    }

    #[test]
    fn tap_debug_never_dumps_the_queue() {
        let tap = active_tap(4);
        tap.push(unit(1, 0, 256, 0x7B));
        let rendered = format!("{tap:?}");
        assert!(rendered.contains("queued: 1"));
        assert!(!rendered.contains("EncodedAccessUnit"), "{rendered}");
        assert!(!rendered.contains("123"), "{rendered}");
    }

    // --- capacity and activation -------------------------------------------

    #[test]
    fn default_capacity_is_eight() {
        let tap = PreviewTap::new();
        assert_eq!(tap.capacity(), 8);
        assert_eq!(DEFAULT_CAPACITY, 8);
        assert_eq!(PreviewTap::default().capacity(), 8);
    }

    #[test]
    fn capacity_is_configurable() {
        for capacity in [1usize, 2, 8, 64] {
            let tap = PreviewTap::with_capacity(capacity).expect("non-zero");
            assert_eq!(tap.capacity(), capacity);
        }
    }

    #[test]
    fn zero_capacity_is_rejected_without_panicking() {
        assert_eq!(PreviewTap::with_capacity(0).err(), Some(ZeroCapacity));
        assert!(PreviewTap::with_capacity(1).is_ok());
    }

    #[test]
    fn an_inactive_tap_ignores_pushes() {
        let tap = PreviewTap::new();
        assert!(!tap.is_active());
        for index in 0..20 {
            assert_eq!(
                tap.push(unit(1, index, 64, 1)),
                TapPushResult::IgnoredInactive
            );
        }
        assert!(tap.is_empty());
        assert_eq!(tap.dropped_total(), 0, "ignored is not lost");
    }

    #[test]
    fn activation_enables_pushes() {
        let tap = PreviewTap::new();
        let activation = tap.activate();
        assert!(activation.activated);
        assert_eq!(activation.discarded, 0);
        assert!(tap.is_active());
        assert_eq!(tap.push(unit(1, 0, 64, 1)), TapPushResult::Queued);
        assert_eq!(tap.len(), 1);
    }

    #[test]
    fn activating_an_already_active_tap_changes_nothing() {
        let tap = active_tap(8);
        tap.push(unit(1, 0, 64, 1));
        tap.push(unit(1, 40, 64, 2));
        let before = tap.stats();
        let activation = tap.activate();
        assert!(!activation.activated);
        assert_eq!(activation.discarded, 0);
        assert_eq!(tap.len(), 2, "live video must survive a repeated START");
        assert_eq!(tap.stats(), before);
    }

    #[test]
    fn activation_clears_leftovers_without_counting_them_as_loss() {
        let tap = active_tap(8);
        for index in 0..5 {
            tap.push(unit(1, index * 40, 64, 1));
        }
        // Simulate a session that ended without deactivating cleanly by
        // flipping the flag through the public API and back.
        assert_eq!(tap.deactivate().discarded, 5);
        assert!(tap.is_empty());
        // Now leave a residue behind: reactivate, queue, then go inactive via
        // a second activate cycle.
        assert!(tap.activate().activated);
        tap.push(unit(1, 0, 64, 1));
        tap.active.store(false, Ordering::Release); // abrupt end, no drain
        let activation = tap.activate();
        assert!(activation.activated);
        assert_eq!(activation.discarded, 1, "residue cleared");
        assert_eq!(tap.dropped_total(), 0, "housekeeping is not loss");
        assert!(tap.is_empty());
    }

    // --- deactivation -------------------------------------------------------

    #[test]
    fn deactivation_empties_the_queue_without_counting_drops() {
        let tap = active_tap(8);
        for index in 0..6 {
            tap.push(unit(1, index * 40, 64, 1));
        }
        let deactivation = tap.deactivate();
        assert!(deactivation.deactivated);
        assert_eq!(deactivation.discarded, 6);
        assert!(tap.is_empty());
        assert!(!tap.is_active());
        assert_eq!(tap.dropped_total(), 0);
    }

    #[test]
    fn deactivation_is_idempotent() {
        let tap = active_tap(8);
        tap.push(unit(1, 0, 64, 1));
        assert_eq!(tap.deactivate().discarded, 1);
        let second = tap.deactivate();
        assert!(!second.deactivated);
        assert_eq!(second.discarded, 0);
        assert_eq!(tap.push(unit(1, 40, 64, 1)), TapPushResult::IgnoredInactive);
    }

    /// The documented residue case, modelled through the test-only deposit
    /// rather than by trying to win a probabilistic race: a producer that had
    /// already passed the `active` check completes its push after
    /// `deactivate` drained.
    #[test]
    fn a_late_producer_can_leave_residue_that_the_next_activation_clears() {
        let tap = active_tap(8);
        for index in 0..3 {
            tap.push(unit(1, index * 40, 64, 1));
        }
        let deactivation = tap.deactivate();
        assert!(deactivation.deactivated);
        assert_eq!(deactivation.discarded, 3, "counts only what it drained");
        assert!(!tap.is_active());
        assert!(tap.is_empty());

        // The straggler lands now, on a tap that is already closed.
        tap.deposit_bypassing_activation(unit(1, 120, 64, 1));
        assert_eq!(tap.len(), 1, "residue can outlive deactivate()");
        assert_eq!(
            tap.stats().queued,
            1,
            "queued may be non-zero while inactive"
        );
        assert!(!tap.stats().active);
        // A closed tap still refuses new frames, residue or not.
        assert_eq!(
            tap.push(unit(1, 160, 64, 1)),
            TapPushResult::IgnoredInactive
        );
        assert_eq!(tap.dropped_total(), 0, "residue is not saturation loss");

        // The next session cleans it up before it can ever be observed.
        let dropped_before = tap.dropped_total();
        let activation = tap.activate();
        assert!(activation.activated);
        assert_eq!(
            activation.discarded, 1,
            "the residue is billed to this activation"
        );
        assert!(tap.is_empty(), "the queue is clean once activation returns");
        assert_eq!(
            tap.dropped_total(),
            dropped_before,
            "deliberate housekeeping never counts as loss"
        );

        // And the tap works normally afterwards.
        assert_eq!(tap.push(unit(2, 0, 64, 2)), TapPushResult::Queued);
        assert_eq!(tap.pop().expect("queued").epoch, 2);
        assert!(tap.is_empty());
    }

    /// The epoch protocol the consumer must implement: a stale unit can be
    /// deposited after an invalidation, so only `au.epoch` compared against a
    /// snapshot taken *after* the pop settles what is current.
    #[test]
    fn stale_units_are_rejected_by_epoch_and_current_ones_accepted() {
        let tap = active_tap(8);
        tap.push(unit(1, 0, 64, 1));

        let invalidation = tap.invalidate_stream(2);
        assert_eq!(invalidation.epoch, 2);
        assert_eq!(invalidation.discarded, 1, "the queued epoch-1 unit went");

        // A producer that was already inside push() lands its old unit after
        // the drain, and a caught-up producer then sends the new epoch.
        tap.deposit_bypassing_activation(unit(1, 40, 64, 1));
        assert_eq!(tap.push(unit(2, 40, 64, 2)), TapPushResult::Queued);

        // The consumer's decision, in the documented order: pop, then read
        // the snapshot, then compare.
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        while let Some(access_unit) = tap.pop() {
            let current = tap.invalidation_snapshot();
            if access_unit.epoch == current.epoch {
                accepted.push(access_unit.epoch);
            } else {
                rejected.push(access_unit.epoch);
            }
        }
        assert_eq!(rejected, vec![1], "the stale unit was at the head");
        assert_eq!(accepted, vec![2]);
        assert_eq!(tap.dropped_total(), 0, "none of this is saturation loss");
    }

    #[test]
    fn a_tap_can_be_reactivated_after_deactivation() {
        let tap = active_tap(8);
        tap.push(unit(1, 0, 64, 1));
        tap.deactivate();
        assert!(tap.activate().activated);
        assert_eq!(tap.push(unit(2, 0, 64, 9)), TapPushResult::Queued);
        let popped = tap.pop().expect("queued");
        assert_eq!(popped.epoch, 2);
        assert_eq!(tap.dropped_total(), 0);
    }

    // --- queue behaviour ----------------------------------------------------

    #[test]
    fn units_come_out_in_order_when_there_is_room() {
        let tap = active_tap(8);
        for index in 0..5i64 {
            assert_eq!(
                tap.push(unit(1, index * 40, 64, index as u8)),
                TapPushResult::Queued
            );
        }
        for index in 0..5i64 {
            let popped = tap.pop().expect("queued");
            assert_eq!(popped.pts_ms, index * 40);
        }
        assert!(tap.pop().is_none());
        assert!(tap.is_empty());
    }

    #[test]
    fn saturation_evicts_the_oldest_and_keeps_the_newest() {
        let tap = active_tap(4);
        for index in 0..4i64 {
            assert_eq!(
                tap.push(unit(1, index, 64, index as u8)),
                TapPushResult::Queued
            );
        }
        assert_eq!(tap.len(), 4);
        assert_eq!(
            tap.push(unit(1, 100, 64, 100)),
            TapPushResult::QueuedAfterDrop { dropped_total: 1 }
        );
        assert_eq!(tap.len(), 4, "still bounded");

        let remaining: Vec<i64> = std::iter::from_fn(|| tap.pop())
            .map(|access_unit| access_unit.pts_ms)
            .collect();
        assert_eq!(remaining, vec![1, 2, 3, 100], "oldest went, newest stayed");
    }

    #[test]
    fn each_saturating_push_counts_exactly_one_drop() {
        let tap = active_tap(2);
        tap.push(unit(1, 0, 64, 0));
        tap.push(unit(1, 1, 64, 1));
        assert_eq!(tap.dropped_total(), 0);
        for expected in 1..=10u64 {
            assert_eq!(
                tap.push(unit(1, expected as i64 + 1, 64, 2)),
                TapPushResult::QueuedAfterDrop {
                    dropped_total: expected
                }
            );
            assert_eq!(tap.dropped_total(), expected);
            assert_eq!(tap.len(), 2);
        }
    }

    #[test]
    fn popping_never_changes_the_drop_counters() {
        let tap = active_tap(2);
        tap.push(unit(1, 0, 64, 0));
        tap.push(unit(1, 1, 64, 1));
        tap.push(unit(1, 2, 64, 2)); // one drop
        let before = tap.stats();
        while tap.pop().is_some() {}
        let after = tap.stats();
        assert_eq!(after.dropped_total, before.dropped_total);
        assert_eq!(
            after.discarded_on_invalidate_total,
            before.discarded_on_invalidate_total
        );
        assert_eq!(after.queued, 0);
    }

    // --- the capture path's copy helper ------------------------------------

    #[test]
    fn an_inactive_tap_copies_nothing_and_validates_nothing() {
        let tap = PreviewTap::new();
        assert!(!tap.is_active());
        // Payloads that would be rejected outright if they were ever looked
        // at: the inactive path must not even reach the validation.
        for payload in [
            &[0x00, 0x00, 0x00, 0x01, 0x65][..],
            &[0xFF, 0xFF][..],
            &[][..],
        ] {
            assert_eq!(
                tap.push_annexb_copy_if_active(1, -99, false, false, false, payload),
                Ok(TapPushResult::IgnoredInactive),
                "inactive must not validate {payload:02X?}"
            );
        }
        assert!(tap.is_empty());
        assert_eq!(tap.dropped_total(), 0, "ignored is not lost");
        assert_eq!(tap.stats().discarded_on_invalidate_total, 0);
    }

    #[test]
    fn an_active_tap_queues_one_unit_with_the_exact_metadata() {
        let tap = active_tap(8);
        let mut payload = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x1F];
        payload.extend_from_slice(&[0x00, 0x00, 0x01, 0x68, 0xEE]);
        payload.extend_from_slice(&[0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);

        assert_eq!(
            tap.push_annexb_copy_if_active(7, 1234, true, true, true, &payload),
            Ok(TapPushResult::Queued)
        );
        assert_eq!(tap.len(), 1, "exactly one access unit");

        let queued = tap.pop().expect("queued");
        assert_eq!(queued.epoch, 7);
        assert_eq!(queued.pts_ms, 1234);
        assert!(queued.keyframe);
        assert!(queued.has_sps);
        assert!(queued.has_pps);
        assert_eq!(&*queued.annexb, &payload[..], "payload must be byte-exact");
        assert!(tap.is_empty());
        assert_eq!(tap.dropped_total(), 0);

        // The flags are carried, not inferred: the same bytes can be handed
        // over with every flag cleared, because the caller owns that decision.
        assert_eq!(
            tap.push_annexb_copy_if_active(7, 1274, false, false, false, &payload),
            Ok(TapPushResult::Queued)
        );
        let plain = tap.pop().expect("queued");
        assert!(!plain.keyframe && !plain.has_sps && !plain.has_pps);
    }

    #[test]
    fn an_active_tap_rejects_bad_input_without_queueing_it() {
        let tap = active_tap(8);
        assert_eq!(
            tap.push_annexb_copy_if_active(1, -1, false, false, false, &[0, 0, 1, 0x65]),
            Err(AccessUnitError::NegativePts)
        );
        assert_eq!(
            tap.push_annexb_copy_if_active(1, 0, false, false, false, &[]),
            Err(AccessUnitError::EmptyPayload)
        );
        assert_eq!(
            tap.push_annexb_copy_if_active(1, 0, false, false, false, &[0xFF, 0x00, 0x00, 0x01]),
            Err(AccessUnitError::InvalidAnnexB)
        );
        assert!(tap.is_empty(), "nothing may be queued");
        assert_eq!(tap.dropped_total(), 0);
        assert!(tap.is_active(), "a rejection must not close the tap");
    }

    #[test]
    fn the_copy_helper_saturates_exactly_like_push() {
        let tap = active_tap(2);
        let payload = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xAA];
        for pts in 0..2i64 {
            assert_eq!(
                tap.push_annexb_copy_if_active(1, pts, false, false, false, &payload),
                Ok(TapPushResult::Queued)
            );
        }
        assert_eq!(
            tap.push_annexb_copy_if_active(1, 2, false, false, false, &payload),
            Ok(TapPushResult::QueuedAfterDrop { dropped_total: 1 })
        );
        assert_eq!(tap.dropped_total(), 1, "exactly one drop");
        assert_eq!(tap.len(), 2, "still bounded");
        let kept: Vec<i64> = std::iter::from_fn(|| tap.pop())
            .map(|unit| unit.pts_ms)
            .collect();
        assert_eq!(kept, vec![1, 2], "the newest access unit was kept");
    }

    #[test]
    fn the_copy_helper_materializes_the_payload_exactly_once() {
        let tap = active_tap(4);
        let payload = vec![0x00u8, 0x00, 0x00, 0x01, 0x65, 0x11, 0x22, 0x33];
        let source = payload.as_ptr();
        tap.push_annexb_copy_if_active(1, 0, false, false, false, &payload)
            .expect("valid");

        let queued = tap.pop().expect("queued");
        // One copy: the tap owns its own allocation, not the caller's buffer.
        assert_ne!(queued.annexb.as_ptr(), source);
        assert_eq!(&*queued.annexb, &payload[..]);
        // And no further copy: every clone from here on shares that one
        // allocation, so nothing downstream can duplicate the frame.
        let address = queued.annexb.as_ptr();
        let clone = queued.clone();
        assert_eq!(clone.annexb.as_ptr(), address);
        assert!(Arc::ptr_eq(&queued.annexb, &clone.annexb));
        assert_eq!(Arc::strong_count(&queued.annexb), 2);
    }

    // --- invalidation --------------------------------------------------------

    #[test]
    fn invalidation_clears_the_queue_and_counts_separately() {
        let tap = active_tap(8);
        for index in 0..6 {
            tap.push(unit(1, index, 64, 1));
        }
        let invalidation = tap.invalidate_stream(2);
        assert_eq!(invalidation.epoch, 2);
        assert_eq!(invalidation.generation, 1);
        assert_eq!(invalidation.discarded, 6);
        assert!(tap.is_empty());
        assert!(tap.is_active(), "invalidation must not close the session");

        let stats = tap.stats();
        assert_eq!(stats.discarded_on_invalidate_total, 6);
        assert_eq!(stats.dropped_total, 0, "an epoch change is not saturation");
        assert_eq!(stats.invalidated_epoch, 2);
        assert_eq!(stats.invalidation_generation, 1);
    }

    #[test]
    fn repeated_and_skipped_epochs_are_both_accepted() {
        let tap = active_tap(8);
        assert_eq!(tap.invalidate_stream(10).generation, 1);
        // Skipped: 10 -> 12 must be fine, epochs are not required to be
        // consecutive.
        let skipped = tap.invalidate_stream(12);
        assert_eq!((skipped.epoch, skipped.generation), (12, 2));
        // Repeated: the same epoch is still a new explicit event.
        let repeated = tap.invalidate_stream(12);
        assert_eq!((repeated.epoch, repeated.generation), (12, 3));
        assert_eq!(tap.invalidation_snapshot().generation, 3);
        // Even going backwards is accepted; ordering is the caller's business.
        assert_eq!(tap.invalidate_stream(5).generation, 4);
        assert_eq!(tap.invalidation_snapshot().epoch, 5);
    }

    #[test]
    fn an_invalidation_survives_a_full_queue() {
        let tap = active_tap(4);
        for index in 0..12 {
            tap.push(unit(1, index, 64, 1)); // saturates, 8 drops
        }
        assert_eq!(tap.len(), 4);
        let dropped_before = tap.dropped_total();
        assert!(dropped_before > 0, "the queue must really be saturating");

        let invalidation = tap.invalidate_stream(99);
        assert_eq!(invalidation.epoch, 99);
        assert_eq!(invalidation.generation, 1);
        assert_eq!(invalidation.discarded, 4);
        assert_eq!(
            tap.dropped_total(),
            dropped_before,
            "invalidation adds nothing to saturation loss"
        );
        assert_eq!(tap.invalidation_snapshot().epoch, 99);
    }

    #[test]
    fn pushing_still_works_after_an_invalidation() {
        let tap = active_tap(4);
        tap.push(unit(1, 0, 64, 1));
        tap.invalidate_stream(2);
        assert_eq!(tap.push(unit(2, 0, 64, 2)), TapPushResult::Queued);
        assert_eq!(tap.pop().expect("queued").epoch, 2);
    }

    #[test]
    fn a_rejected_access_unit_cannot_disturb_the_invalidation_state() {
        let tap = active_tap(4);
        tap.invalidate_stream(7);
        let before = tap.stats();
        // Construction fails, so nothing ever reaches the tap.
        assert!(EncodedAccessUnit::from_slice(8, -1, false, false, false, &[0, 0, 1, 1]).is_err());
        assert!(EncodedAccessUnit::from_slice(8, 0, false, false, false, &[]).is_err());
        assert!(EncodedAccessUnit::from_slice(8, 0, false, false, false, &[0xFF; 8]).is_err());
        assert_eq!(tap.stats(), before);
    }

    // --- snapshots ------------------------------------------------------------

    #[test]
    fn stats_are_metrics_only_and_independently_owned() {
        let tap = active_tap(4);
        tap.push(unit(1, 0, 1024, 0x33));
        let first = tap.stats();
        assert_eq!(first.queued, 1);
        assert_eq!(first.capacity, 4);
        assert!(first.active);

        // Taking a snapshot must not drain anything.
        assert_eq!(tap.len(), 1);

        // And the snapshot is a value, not a view: later activity cannot
        // change what was already handed out.
        tap.push(unit(1, 40, 1024, 0x33));
        tap.invalidate_stream(2);
        let second = tap.stats();
        assert_eq!(first.queued, 1, "the earlier snapshot is frozen");
        assert_eq!(second.queued, 0);
        assert_ne!(first, second);

        // Nothing in the rendered form resembles payload.
        let rendered = format!("{first:?}");
        assert!(!rendered.contains("annexb"), "{rendered}");
        assert!(!rendered.contains("pts"), "{rendered}");
    }

    /// `stats()` is diagnostic: only the invalidation pair is coherent, and
    /// the rest is sampled independently. This pins the properties that are
    /// actually promised, and deliberately does not require `active` and
    /// `queued` to agree atomically.
    #[test]
    fn stats_is_a_diagnostic_snapshot_with_one_coherent_pair() {
        let tap = active_tap(4);
        tap.invalidate_stream(11);
        tap.invalidate_stream(12);
        tap.push(unit(12, 0, 64, 1));
        tap.push(unit(12, 40, 64, 1));

        let stats = tap.stats();
        let pair = tap.invalidation_snapshot();
        assert_eq!(
            (stats.invalidated_epoch, stats.invalidation_generation),
            (pair.epoch, pair.generation),
            "the pair must come from the same seqlock read"
        );
        assert_eq!((pair.epoch, pair.generation), (12, 2));
        assert!(stats.queued <= stats.capacity, "the bound always holds");

        // Sampling changes nothing and does not drain.
        let queued_before = tap.len();
        for _ in 0..1_000 {
            let sample = tap.stats();
            assert!(sample.queued <= sample.capacity);
        }
        assert_eq!(tap.len(), queued_before, "snapshots must not consume");

        // Earlier snapshots are independent values, not live views.
        let before = tap.stats();
        tap.pop();
        tap.invalidate_stream(13);
        let after = tap.stats();
        assert_eq!(before.queued, 2, "the earlier snapshot is frozen");
        assert_eq!(before.invalidation_generation, 2);
        assert_eq!(after.invalidation_generation, 3);
        assert_ne!(before, after);
    }

    // --- saturating counters ---------------------------------------------------

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(saturating_increment(&counter), u64::MAX);
        assert_eq!(saturating_increment(&counter), u64::MAX);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(saturating_add(&counter, 1_000), u64::MAX);

        let other = AtomicU64::new(u64::MAX - 5);
        assert_eq!(saturating_add(&other, 100), u64::MAX);
    }

    #[test]
    fn a_saturated_drop_counter_stays_saturated() {
        let tap = active_tap(1);
        tap.dropped_total.store(u64::MAX, Ordering::Relaxed);
        tap.push(unit(1, 0, 64, 1));
        assert_eq!(
            tap.push(unit(1, 1, 64, 1)),
            TapPushResult::QueuedAfterDrop {
                dropped_total: u64::MAX
            }
        );
        assert_eq!(tap.dropped_total(), u64::MAX);
    }

    // --- concurrency ------------------------------------------------------------

    #[test]
    fn readers_never_observe_an_incoherent_epoch_generation_pair() {
        // Each invalidation writes epoch = generation * 1000, so any pair the
        // reader accepts must satisfy that relation exactly. A torn read
        // would pair one round's epoch with another round's generation.
        let tap = Arc::new(active_tap(4));
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let tap = Arc::clone(&tap);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                for round in 1..=20_000u64 {
                    tap.invalidate_stream(round * 1000);
                }
                stop.store(true, Ordering::Release);
            })
        };
        let reader = {
            let tap = Arc::clone(&tap);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut observed = 0u64;
                let mut last_generation = 0u64;
                while !stop.load(Ordering::Acquire) {
                    let snapshot = tap.invalidation_snapshot();
                    if snapshot.generation > 0 {
                        assert_eq!(
                            snapshot.epoch,
                            snapshot.generation * 1000,
                            "torn pair: {snapshot:?}"
                        );
                    }
                    assert!(
                        snapshot.generation >= last_generation,
                        "generation went backwards"
                    );
                    last_generation = snapshot.generation;
                    observed += 1;
                }
                observed
            })
        };

        writer.join().expect("writer finished");
        let observed = reader.join().expect("reader finished");
        assert!(observed > 0, "the reader must have sampled something");
        assert_eq!(tap.invalidation_snapshot().generation, 20_000);
    }

    #[test]
    fn concurrent_invalidations_all_terminate() {
        let tap = Arc::new(active_tap(4));
        let writers: Vec<_> = (0..4u64)
            .map(|writer| {
                let tap = Arc::clone(&tap);
                thread::spawn(move || {
                    for round in 0..5_000u64 {
                        tap.invalidate_stream(writer * 1_000_000 + round);
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("no writer may hang or panic");
        }
        // Every call must have been counted exactly once, which is also the
        // proof that no writer lost its seqlock round to another.
        assert_eq!(tap.invalidation_snapshot().generation, 20_000);
        let snapshot = tap.invalidation_snapshot();
        assert_eq!(snapshot.generation, tap.stats().invalidation_generation);
    }

    #[test]
    fn producers_and_a_consumer_make_progress_without_deadlocking() {
        let tap = Arc::new(active_tap(8));
        let stop = Arc::new(AtomicBool::new(false));
        let consumed = Arc::new(AtomicUsize::new(0));

        let producers: Vec<_> = (0..2u8)
            .map(|producer| {
                let tap = Arc::clone(&tap);
                thread::spawn(move || {
                    for index in 0..5_000i64 {
                        tap.push(unit(1, index, 64, producer));
                    }
                })
            })
            .collect();
        let consumer = {
            let tap = Arc::clone(&tap);
            let stop = Arc::clone(&stop);
            let consumed = Arc::clone(&consumed);
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) || !tap.is_empty() {
                    if tap.pop().is_some() {
                        consumed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };

        for producer in producers {
            producer.join().expect("producer finished");
        }
        stop.store(true, Ordering::Release);
        consumer.join().expect("consumer finished");
        assert!(consumed.load(Ordering::Relaxed) > 0);
        assert!(tap.len() <= tap.capacity());
    }

    /// Deterministic in its invariants, not in its interleaving: the queue is
    /// designed to lose access units, so this asserts what must always hold
    /// (bounds, integrity, monotonicity, termination) and never how many
    /// arrive.
    #[test]
    fn stress_four_producers_one_consumer_with_invalidations() {
        const PRODUCERS: u8 = 4;
        const PER_PRODUCER: i64 = 10_000;
        const CAPACITY: usize = 8;
        let deadline = Instant::now() + Duration::from_secs(60);

        let tap = Arc::new(active_tap(CAPACITY));
        let done = Arc::new(AtomicBool::new(false));
        let consumed = Arc::new(AtomicUsize::new(0));

        let producers: Vec<_> = (0..PRODUCERS)
            .map(|producer| {
                let tap = Arc::clone(&tap);
                thread::spawn(move || {
                    for index in 0..PER_PRODUCER {
                        // Vary the size so the payload check is meaningful.
                        let len = 8 + (index as usize % 97);
                        tap.push(unit(index as u64 / 500, index, len, producer));
                        // Periodic invalidations from the producer side, the
                        // way the watchdog will drive them.
                        if index % 997 == 0 {
                            tap.invalidate_stream(index as u64 / 500);
                        }
                    }
                })
            })
            .collect();

        let consumer = {
            let tap = Arc::clone(&tap);
            let done = Arc::clone(&done);
            let consumed = Arc::clone(&consumed);
            thread::spawn(move || {
                let mut last = tap.stats();
                while !done.load(Ordering::Acquire) || !tap.is_empty() {
                    if let Some(access_unit) = tap.pop() {
                        // Integrity: the header survived and the tail is
                        // exactly the marker its producer wrote.
                        assert_eq!(&access_unit.annexb[..5], &[0, 0, 0, 1, 0x65]);
                        let marker = access_unit.annexb[5];
                        assert!(marker < PRODUCERS, "corrupt marker {marker}");
                        assert!(
                            access_unit.annexb[5..].iter().all(|byte| *byte == marker),
                            "payload corrupted"
                        );
                        assert!(access_unit.pts_ms >= 0);
                        assert!(access_unit.epoch <= (PER_PRODUCER as u64 / 500));
                        consumed.fetch_add(1, Ordering::Relaxed);
                    }
                    let now = tap.stats();
                    assert!(now.queued <= CAPACITY, "queue exceeded its bound");
                    assert!(now.dropped_total >= last.dropped_total, "counter fell");
                    assert!(
                        now.invalidation_generation >= last.invalidation_generation,
                        "generation fell"
                    );
                    assert!(
                        now.discarded_on_invalidate_total >= last.discarded_on_invalidate_total,
                        "invalidation counter fell"
                    );
                    assert!(now.active, "nothing here may close the session");
                    last = now;
                }
            })
        };

        for producer in producers {
            producer.join().expect("no producer may hang or panic");
        }
        done.store(true, Ordering::Release);
        consumer.join().expect("consumer must finish");

        assert!(
            Instant::now() < deadline,
            "stress run exceeded its time budget"
        );
        let stats = tap.stats();
        assert!(stats.queued <= CAPACITY);
        assert_eq!(
            stats.invalidation_generation,
            (PRODUCERS as u64) * (PER_PRODUCER as u64).div_ceil(997),
            "every invalidation must have been counted exactly once"
        );
        assert!(consumed.load(Ordering::Relaxed) > 0, "nothing got through");
        assert!(
            stats.dropped_total > 0,
            "a capacity of 8 against 40k units must have saturated"
        );
    }

    #[test]
    fn repeated_operations_never_panic() {
        let tap = PreviewTap::with_capacity(2).expect("non-zero");
        for round in 0..2_000i64 {
            tap.activate();
            tap.push(unit(round as u64, round, 16, 1));
            tap.pop();
            tap.invalidate_stream(round as u64);
            tap.stats();
            tap.deactivate();
            tap.deactivate();
            tap.push(unit(round as u64, round, 16, 1));
        }
        assert!(!tap.is_active());
        assert!(tap.is_empty());
    }

    /// A waker that only counts, so a test can assert *how many* times the tap
    /// decided to wake the consumer rather than just that it eventually did.
    struct CountingWake(AtomicUsize);

    impl std::task::Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<CountingWake>, Waker) {
        let counter = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        (counter, waker)
    }

    #[test]
    fn an_accepted_push_wakes_the_registered_consumer() {
        let tap = active_tap(4);
        let (counter, waker) = counting_waker();
        tap.register_waker(&waker);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0, "registering is not a wakeup");

        tap.push(unit(0, 0, 16, 1));

        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_ignored_push_wakes_nobody() {
        let tap = PreviewTap::with_capacity(4).expect("non-zero capacity");
        let (counter, waker) = counting_waker();
        tap.register_waker(&waker);

        assert_eq!(tap.push(unit(0, 0, 16, 1)), TapPushResult::IgnoredInactive);

        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            0,
            "an inactive tap must not disturb anything"
        );
    }

    #[test]
    fn several_pushes_are_drained_by_one_wakeup() {
        let tap = active_tap(8);
        let (counter, waker) = counting_waker();
        // One registration is consumed by the first wake; the consumer would
        // re-register on its next poll, so everything pushed until then has to
        // be reachable from that single wakeup.
        tap.register_waker(&waker);
        for pts in 0..5 {
            tap.push(unit(0, pts, 16, 1));
        }

        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "AtomicWaker holds one waker: later pushes find none registered"
        );
        let drained = std::iter::from_fn(|| tap.pop()).count();
        assert_eq!(drained, 5, "one wakeup still exposes every queued unit");
        assert_eq!(tap.dropped_total(), 0);
    }

    /// The publisher's real wait: drain, register, drain again, only then park.
    /// This is what makes the wait free of both busy-looping and lost wakeups.
    #[test]
    fn the_documented_wait_protocol_never_parks_with_work_pending() {
        let tap = active_tap(4);
        let (counter, waker) = counting_waker();

        // Nothing queued: the protocol reaches the "would park" point, and
        // getting there cost zero wakeups — no spinning, no sleeping.
        assert!(tap.pop().is_none());
        tap.register_waker(&waker);
        assert!(tap.pop().is_none());
        assert!(tap.is_active());
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);

        // A unit that lands exactly in the window between registering and
        // parking is caught by the second drain of the *next* poll, and its
        // wakeup is not lost either.
        tap.push(unit(0, 1, 16, 1));
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert!(tap.pop().is_some(), "the queued unit is reachable");
    }

    #[test]
    fn deactivation_wakes_the_consumer_so_it_can_shut_down() {
        let tap = active_tap(4);
        let (counter, waker) = counting_waker();
        tap.register_waker(&waker);

        assert!(tap.deactivate().deactivated);

        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        // And what it observes on waking is a closed tap, which is its exit
        // condition rather than an empty queue it should keep waiting on.
        assert!(!tap.is_active());
        assert!(tap.pop().is_none());
    }

    #[test]
    fn invalidation_wakes_the_consumer_before_the_new_epoch_produces_a_frame() {
        let tap = active_tap(4);
        let (counter, waker) = counting_waker();
        tap.register_waker(&waker);

        let invalidation = tap.invalidate_stream(7);

        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(invalidation.epoch, 7);
        assert!(tap.is_active(), "the session outlives the stream underneath it");
    }

    #[test]
    fn the_waker_does_not_change_capacity_or_the_drop_policy() {
        let tap = active_tap(2);
        let (counter, waker) = counting_waker();
        tap.register_waker(&waker);

        assert_eq!(tap.capacity(), 2);
        tap.push(unit(0, 1, 16, 1));
        tap.push(unit(0, 2, 16, 2));
        // Third push saturates: oldest out, newest in, exactly one drop.
        assert_eq!(
            tap.push(unit(0, 3, 16, 3)),
            TapPushResult::QueuedAfterDrop { dropped_total: 1 }
        );
        assert_eq!(tap.len(), 2);
        let kept: Vec<i64> = std::iter::from_fn(|| tap.pop())
            .map(|au| au.pts_ms)
            .collect();
        assert_eq!(kept, vec![2, 3], "still evicting the oldest");
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "the registration is consumed by the first wake, not by every push"
        );
    }

    #[test]
    fn default_capacity_is_still_eight_with_a_waker_present() {
        let tap = PreviewTap::new();
        assert_eq!(tap.capacity(), DEFAULT_CAPACITY);
        assert_eq!(tap.capacity(), 8);
    }
}
