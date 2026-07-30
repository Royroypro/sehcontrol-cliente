typedef ScreenCamPreviewLocalIdProvider = Future<String> Function();
typedef ScreenCamPreviewStartHandler = Future<void> Function(
  ScreenCamPreviewStartMessage message,
);
typedef ScreenCamPreviewStopHandler = Future<void> Function(
  ScreenCamPreviewStopMessage message,
);

class ScreenCamPreviewStartMessage {
  const ScreenCamPreviewStartMessage({
    required this.sessionId,
    required this.rustdeskId,
    required this.publishUrl,
    required this.publishToken,
    required this.streamName,
    required this.expiresIn,
  });

  final String sessionId;
  final String rustdeskId;
  final String publishUrl;
  final String publishToken;
  final String streamName;
  final int expiresIn;
}

class ScreenCamPreviewStopMessage {
  const ScreenCamPreviewStopMessage({
    required this.sessionId,
    required this.rustdeskId,
  });

  final String sessionId;
  final String rustdeskId;
}

ScreenCamPreviewStartMessage? parseScreenCamPreviewStart(Object? data) {
  if (data is! Map) return null;

  final sessionId = _trimmedNonEmptyString(data['session_id']);
  final rustdeskId = _trimmedNonEmptyString(data['rustdesk_id']);
  final publishUrl = _trimmedNonEmptyString(data['publish_url']);
  final publishToken = data['publish_token'];
  final streamName = _trimmedNonEmptyString(data['stream_name']);
  final expiresIn = data['expires_in'];

  if (sessionId == null ||
      rustdeskId == null ||
      publishUrl == null ||
      publishToken is! String ||
      publishToken.trim().isEmpty ||
      streamName == null ||
      expiresIn is! int ||
      expiresIn < 1 ||
      expiresIn > 300 ||
      !_isValidSrtPublishUrl(publishUrl)) {
    return null;
  }

  return ScreenCamPreviewStartMessage(
    sessionId: sessionId,
    rustdeskId: rustdeskId,
    publishUrl: publishUrl,
    publishToken: publishToken,
    streamName: streamName,
    expiresIn: expiresIn,
  );
}

ScreenCamPreviewStopMessage? parseScreenCamPreviewStop(Object? data) {
  if (data is! Map) return null;

  final sessionId = _trimmedNonEmptyString(data['session_id']);
  final rustdeskId = _trimmedNonEmptyString(data['rustdesk_id']);
  if (sessionId == null || rustdeskId == null) return null;

  return ScreenCamPreviewStopMessage(
    sessionId: sessionId,
    rustdeskId: rustdeskId,
  );
}

bool screenCamPreviewIdsMatch(String localId, String messageRustdeskId) {
  return localId.trim() == messageRustdeskId.trim();
}

Future<void> dispatchScreenCamPreviewStart(
  Object? data, {
  required bool isWindowsPlatform,
  required ScreenCamPreviewLocalIdProvider getLocalId,
  required ScreenCamPreviewStartHandler onValidated,
}) async {
  if (!isWindowsPlatform) return;

  try {
    final message = parseScreenCamPreviewStart(data);
    if (message == null) return;
    final localId = (await getLocalId()).trim();
    if (!screenCamPreviewIdsMatch(localId, message.rustdeskId)) return;
    await onValidated(message);
  } catch (_) {
    // Identity and downstream failures must not escape into the WebSocket.
  }
}

Future<void> dispatchScreenCamPreviewStop(
  Object? data, {
  required bool isWindowsPlatform,
  required ScreenCamPreviewLocalIdProvider getLocalId,
  required ScreenCamPreviewStopHandler onValidated,
}) async {
  if (!isWindowsPlatform) return;

  try {
    final message = parseScreenCamPreviewStop(data);
    if (message == null) return;
    final localId = (await getLocalId()).trim();
    if (!screenCamPreviewIdsMatch(localId, message.rustdeskId)) return;
    await onValidated(message);
  } catch (_) {
    // Identity and downstream failures must not escape into the WebSocket.
  }
}

String? _trimmedNonEmptyString(Object? value) {
  if (value is! String) return null;
  final trimmed = value.trim();
  return trimmed.isEmpty ? null : trimmed;
}

bool _isValidSrtPublishUrl(String value) {
  if (!value.startsWith('srt://')) return false;

  final uri = Uri.tryParse(value);
  final authorityEnd = value.indexOf(RegExp(r'[/#?]'), 'srt://'.length);
  final rawAuthority = value.substring(
    'srt://'.length,
    authorityEnd < 0 ? value.length : authorityEnd,
  );
  if (uri == null ||
      uri.scheme != 'srt' ||
      uri.host.isEmpty ||
      !uri.hasPort ||
      rawAuthority.contains('@') ||
      uri.hasFragment ||
      uri.queryParametersAll.keys
          .any((key) => key.toLowerCase() == 'streamid')) {
    return false;
  }

  try {
    return uri.port >= 1 && uri.port <= 65535;
  } on FormatException {
    return false;
  }
}
