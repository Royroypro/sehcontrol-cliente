// Turns the daemon's preview lifecycle snapshot into the message the panel
// expects, and owns the "did it actually go out?" bookkeeping.
//
// Split out of UserModel so the parts worth testing — the whitelist, the
// validation, the retry and the de-duplication — can be exercised without an
// FFI bridge or a socket.
//
// Two rules shape everything here:
//
// - **Nothing is forwarded that was not explicitly allowed.** The payload is
//   built field by field from a fixed list rather than by editing the map that
//   arrived. A future field added on the Rust side therefore cannot reach the
//   panel by accident, which matters because the publisher sits next to a URL,
//   a token and a stream id.
// - **An event counts as delivered only when the send says so.** The realtime
//   channel can be down exactly when the publisher connects, so a `false` from
//   the sender has to leave the event pending rather than mark it done.

/// The events the panel understands. Anything else is dropped.
const Set<String> _allowedEvents = {
  'screen_cam.preview.connecting',
  'screen_cam.preview.started',
  'screen_cam.preview.failed',
  'screen_cam.preview.stopped',
};

/// Failure codes the daemon is allowed to state. An unknown code is replaced
/// rather than passed through, so the panel's vocabulary stays closed even if
/// the two sides drift.
const Set<String> _knownReasons = {
  'connect_failed',
  'send_failed',
  'mux_failed',
  'retries_exhausted',
};

// The daemon also reports *why* a session stopped (`stopped`, `expired`,
// `tap_closed`), but the panel's contract for `screen_cam.preview.stopped`
// carries no cause, so it is deliberately not forwarded.

/// One validated lifecycle transition, ready to send.
class ScreenCamPreviewLifecycleEvent {
  ScreenCamPreviewLifecycleEvent({required this.key, required this.payload});

  /// Identity for de-duplication: `session_id:generation:sequence:event`.
  ///
  /// The session is part of it because the daemon can restart and begin
  /// counting from zero again; keyed on the counters alone, the first
  /// transition of a brand new session would look like one already sent and be
  /// dropped. None of these fields reach the panel — only `payload` does.
  final String key;

  /// Exactly what goes over the WebSocket.
  final Map<String, Object?> payload;
}

/// Validates the snapshot the Rust poller pushed and rebuilds it as the panel's
/// message. Returns null for anything malformed, unknown or incomplete.
ScreenCamPreviewLifecycleEvent? parseScreenCamPreviewLifecycle(Object? raw) {
  if (raw is! Map) return null;

  final event = _nonEmptyString(raw['event']);
  if (event == null || !_allowedEvents.contains(event)) return null;

  final sessionId = _nonEmptyString(raw['session_id']);
  final rustdeskId = _nonEmptyString(raw['rustdesk_id']);
  if (sessionId == null || rustdeskId == null) return null;

  // Internal ordering fields. Absent means the daemon sent something older than
  // this client expects; dropping it is safer than inventing an identity.
  final generation = _nonNegativeInt(raw['generation']);
  final sequence = _nonNegativeInt(raw['sequence']);
  if (generation == null || sequence == null) return null;

  final payload = <String, Object?>{
    'event': event,
    'session_id': sessionId,
    'rustdesk_id': rustdeskId,
  };

  if (event == 'screen_cam.preview.failed') {
    final reason = _nonEmptyString(raw['reason']);
    payload['reason'] =
        reason != null && _knownReasons.contains(reason) ? reason : 'unknown';
  }

  return ScreenCamPreviewLifecycleEvent(
    key: '$sessionId:$generation:$sequence:$event',
    payload: payload,
  );
}

/// Sends a payload; false means "not delivered, try again later".
typedef ScreenCamPreviewLifecycleSender = bool Function(
    Map<String, Object?> payload);

/// Holds at most one undelivered transition and retries it.
///
/// Only the newest is kept: the panel wants the current state, and replaying a
/// stale `connecting` after `started` would move it backwards.
class ScreenCamPreviewLifecycleDispatcher {
  ScreenCamPreviewLifecycleDispatcher(this._send);

  final ScreenCamPreviewLifecycleSender _send;

  ScreenCamPreviewLifecycleEvent? _pending;
  String? _lastDeliveredKey;

  /// Test/diagnostic view of what is still owed to the panel.
  bool get hasPending => _pending != null;
  String? get lastDeliveredKey => _lastDeliveredKey;

  /// Handles one snapshot from the daemon. Returns true when it reached the
  /// panel during this call.
  bool handle(Object? raw) {
    final event = parseScreenCamPreviewLifecycle(raw);
    if (event == null) return false;
    if (event.key == _lastDeliveredKey) return false;
    _pending = event;
    return flush();
  }

  /// Retries whatever is owed. Called again when the realtime channel comes
  /// back up, which is the case a publisher that connected while the socket was
  /// down depends on.
  bool flush() {
    final pending = _pending;
    if (pending == null) return false;
    if (!_send(pending.payload)) return false;
    // Only now: a send that failed must not be remembered as delivered.
    _lastDeliveredKey = pending.key;
    _pending = null;
    return true;
  }

  /// Logout, or the end of a membership session: nothing owed, nothing
  /// remembered, so a later session starts clean.
  void reset() {
    _pending = null;
    _lastDeliveredKey = null;
  }
}

String? _nonEmptyString(Object? value) {
  if (value is! String) return null;
  final trimmed = value.trim();
  return trimmed.isEmpty ? null : trimmed;
}

int? _nonNegativeInt(Object? value) {
  if (value is int) return value < 0 ? null : value;
  return null;
}
