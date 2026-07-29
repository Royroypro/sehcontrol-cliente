import 'dart:convert';

const _screenCamDisplayIdPrefix = r'\\.\DISPLAY';
const _screenCamDisplayIdMaxDigits = 10;

/// Mirrors the Windows service validator for the only display identifier that
/// may cross the policy IPC boundary. The daemon validates again; this keeps
/// malformed policy data from causing an IPC round trip in the first place.
bool isValidScreenCamDisplayId(String value) {
  if (value.length <= _screenCamDisplayIdPrefix.length ||
      value.length >
          _screenCamDisplayIdPrefix.length + _screenCamDisplayIdMaxDigits ||
      value.trim() != value) {
    return false;
  }
  for (var index = 0; index < _screenCamDisplayIdPrefix.length; index++) {
    final actual = value.codeUnitAt(index);
    final expected = _screenCamDisplayIdPrefix.codeUnitAt(index);
    final asciiFolded =
        actual >= 0x61 && actual <= 0x7a ? actual - 0x20 : actual;
    if (asciiFolded != expected) return false;
  }
  for (final unit in value.codeUnits) {
    if (unit <= 0x1f || (unit >= 0x7f && unit <= 0x9f)) return false;
  }
  final suffix = value.substring(_screenCamDisplayIdPrefix.length);
  if (suffix.codeUnits.any((unit) => unit < 0x30 || unit > 0x39)) {
    return false;
  }
  final number = BigInt.tryParse(suffix);
  return number != null &&
      number > BigInt.zero &&
      number <= BigInt.from(0xffffffff);
}

class ScreenCamPolicyApplyResult {
  const ScreenCamPolicyApplyResult({
    required this.attempted,
    required this.applied,
    required this.changed,
    this.error,
  });

  final bool attempted;
  final bool applied;
  final bool changed;
  final String? error;
}

typedef ScreenCamPolicySender = Future<String> Function(String payload);

class ScreenCamV2PolicyDecision {
  const ScreenCamV2PolicyDecision({
    this.policy,
    this.unresolved = false,
  });

  final Map? policy;
  final bool unresolved;
}

Map<String, dynamic> resolveScreenCamDisplayPolicy(
  Map? v1Policy,
  ScreenCamV2PolicyDecision v2Decision,
) {
  final resolved = <String, dynamic>{};
  void mergeValid(Map? source) {
    if (source == null) return;
    final selected = source['selected_display_id'];
    if (selected is String && isValidScreenCamDisplayId(selected)) {
      resolved['selected_display_id'] = selected;
    }
    final fallback = source['fallback_to_primary'];
    if (fallback is bool) {
      resolved['fallback_to_primary'] = fallback;
    }
  }

  mergeValid(v1Policy);
  if (!v2Decision.unresolved) {
    mergeValid(v2Decision.policy);
  }
  return resolved;
}

/// Encodes V2 compatibility/precedence without performing I/O. Only a
/// resolved 200 response can provide the display override that follows V1.
ScreenCamV2PolicyDecision screenCamV2PolicyDecision(
  int statusCode,
  dynamic decoded,
) {
  if (statusCode != 200 || decoded is! Map) {
    return const ScreenCamV2PolicyDecision();
  }
  if (decoded['device_uid_resolved'] != true) {
    return const ScreenCamV2PolicyDecision(unresolved: true);
  }
  final policy = decoded['screen_cam'];
  return ScreenCamV2PolicyDecision(policy: policy is Map ? policy : null);
}

/// Applies only independently valid selection fields and trusts the update
/// only after a structurally valid daemon ACK. Invalid or absent fields never
/// enter the payload and therefore cannot overwrite historical configuration.
Future<ScreenCamPolicyApplyResult> applyScreenCamDisplayPolicyUpdate(
  Map screenCam,
  ScreenCamPolicySender sender,
) async {
  final values = screenCamDisplayPolicyValues(screenCam);
  if (values.isEmpty) {
    return const ScreenCamPolicyApplyResult(
      attempted: false,
      applied: false,
      changed: false,
    );
  }
  final payload = jsonEncode({
    if (values.containsKey('screencam-selected-display-id'))
      'selected_display_id': values['screencam-selected-display-id'],
    if (values.containsKey('screencam-fallback-to-primary'))
      'fallback_to_primary': values['screencam-fallback-to-primary'] == 'Y',
  });
  try {
    final decoded = jsonDecode(await sender(payload));
    if (decoded is! Map ||
        decoded['applied'] is! bool ||
        decoded['changed'] is! bool ||
        (decoded['error'] != null && decoded['error'] is! String)) {
      return const ScreenCamPolicyApplyResult(
        attempted: true,
        applied: false,
        changed: false,
        error: 'ipc_unavailable',
      );
    }
    const allowedErrors = {
      'invalid_policy',
      'persistence_failed',
      'ipc_unavailable',
      'unsupported',
    };
    final rawError = decoded['error'];
    final error = rawError is String && allowedErrors.contains(rawError)
        ? rawError
        : rawError == null
            ? null
            : 'ipc_unavailable';
    final applied = decoded['applied'] == true;
    final changed = decoded['changed'] == true;
    if ((applied && error != null) || (!applied && error == null)) {
      return const ScreenCamPolicyApplyResult(
        attempted: true,
        applied: false,
        changed: false,
        error: 'ipc_unavailable',
      );
    }
    return ScreenCamPolicyApplyResult(
      attempted: true,
      applied: applied,
      changed: applied && changed,
      error: error,
    );
  } catch (_) {
    return const ScreenCamPolicyApplyResult(
      attempted: true,
      applied: false,
      changed: false,
      error: 'ipc_unavailable',
    );
  }
}

/// Returns the historical (V1) ScreenCam fields **restricted to the keys
/// actually present** in the payload.
///
/// `/api/client-policy` always sends the complete block, but a WebSocket
/// `screen_cam.update` may carry only a subset. Applying V1's defaults to a
/// partial payload would silently unlicense the device (`licensed` absent →
/// `false`), stop it (`desired_state` absent → `stopped`), drop it out of
/// supervised mode (`mode` absent → `local`) or wipe the RTSP credentials the
/// panel issued. An absent key therefore leaves the stored value untouched.
///
/// An **explicitly null** credential still clears it: that is how the panel
/// revokes one, and it is distinguishable from an absent key here.
Map<String, String> screenCamHistoricalPolicyValues(Map screenCam) {
  final values = <String, String>{};
  if (screenCam.containsKey('licensed')) {
    final licensed = screenCam['licensed'];
    if (licensed is bool) {
      values['screencam-licensed'] = licensed ? 'Y' : 'N';
    }
  }
  if (screenCam.containsKey('desired_state')) {
    final desiredState = screenCam['desired_state'];
    if (desiredState == 'running' || desiredState == 'stopped') {
      values['screencam-desired-state'] = desiredState as String;
    }
  }
  if (screenCam.containsKey('mode')) {
    final mode = screenCam['mode'];
    if (mode == 'local' || mode == 'managed' || mode == 'supervised') {
      values['screencam-mode'] = mode as String;
    }
  }
  if (screenCam.containsKey('rtsp_user')) {
    final rtspUser = screenCam['rtsp_user'];
    values['screencam-rtsp-user'] = rtspUser is String ? rtspUser : '';
  }
  if (screenCam.containsKey('rtsp_password')) {
    final rtspPassword = screenCam['rtsp_password'];
    values['screencam-rtsp-pass'] = rtspPassword is String ? rtspPassword : '';
  }
  return values;
}

/// Returns only independently valid display-policy fields. Absent or invalid
/// fields intentionally do not appear, so a partial V2/WS update cannot clear
/// a previously valid selection.
Map<String, String> screenCamDisplayPolicyValues(Map screenCam) {
  final values = <String, String>{};
  final selected = screenCam['selected_display_id'];
  if (selected is String && isValidScreenCamDisplayId(selected)) {
    values['screencam-selected-display-id'] = selected;
  }
  final fallback = screenCam['fallback_to_primary'];
  if (fallback is bool) {
    values['screencam-fallback-to-primary'] = fallback ? 'Y' : 'N';
  }
  return values;
}
