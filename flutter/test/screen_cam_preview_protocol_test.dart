import 'package:flutter_hbb/common/screen_cam_preview_protocol.dart';
import 'package:flutter_test/flutter_test.dart';

Map<String, Object?> validStartData({
  String sessionId = 'pv_8f12ab34c5',
  String rustdeskId = '485236790',
  String publishUrl = 'srt://sehcontrol.sehuacho.com:8890',
  Object? publishToken = 'test-token-redacted',
  String streamName = 'pv_8f12ab34c5',
  Object? expiresIn = 300,
}) {
  return {
    'session_id': sessionId,
    'rustdesk_id': rustdeskId,
    'publish_url': publishUrl,
    'publish_token': publishToken,
    'stream_name': streamName,
    'expires_in': expiresIn,
  };
}

Map<String, Object?> validStopData({
  Object? sessionId = 'pv_8f12ab34c5',
  Object? rustdeskId = '485236790',
}) {
  return {
    'session_id': sessionId,
    'rustdesk_id': rustdeskId,
  };
}

void main() {
  group('ScreenCam Preview START parser', () {
    test('accepts a valid message and trims non-secret text fields', () {
      final message = parseScreenCamPreviewStart(validStartData(
        sessionId: ' pv_8f12ab34c5 ',
        rustdeskId: ' 485236790 ',
        publishUrl: ' srt://sehcontrol.sehuacho.com:8890 ',
        streamName: ' pv_8f12ab34c5 ',
      ));

      expect(message, isNotNull);
      expect(message!.sessionId, 'pv_8f12ab34c5');
      expect(message.rustdeskId, '485236790');
      expect(message.publishUrl, 'srt://sehcontrol.sehuacho.com:8890');
      expect(message.streamName, 'pv_8f12ab34c5');
      expect(message.expiresIn, 300);
    });

    test('rejects absent or non-map data', () {
      expect(parseScreenCamPreviewStart(null), isNull);
      expect(parseScreenCamPreviewStart('invalid'), isNull);
      expect(parseScreenCamPreviewStart(const [1, 2]), isNull);
    });

    test('rejects missing, empty or non-string identifiers', () {
      final missingSession = validStartData()..remove('session_id');
      final missingRustdeskId = validStartData()..remove('rustdesk_id');

      expect(parseScreenCamPreviewStart(missingSession), isNull);
      expect(
        parseScreenCamPreviewStart(validStartData(sessionId: '  ')),
        isNull,
      );
      expect(parseScreenCamPreviewStart(missingRustdeskId), isNull);
      expect(
        parseScreenCamPreviewStart(validStartData(rustdeskId: '  ')),
        isNull,
      );
      expect(
        parseScreenCamPreviewStart({
          ...validStartData(),
          'session_id': 123,
        }),
        isNull,
      );
    });

    test('rejects missing or malformed publish URLs', () {
      final missing = validStartData()..remove('publish_url');
      expect(parseScreenCamPreviewStart(missing), isNull);

      final invalidUrls = [
        '',
        'https://sehcontrol.sehuacho.com:8890',
        'SRT://sehcontrol.sehuacho.com:8890',
        'srt://:8890',
        'srt://sehcontrol.sehuacho.com',
        'srt://sehcontrol.sehuacho.com:not-a-port',
        'srt://sehcontrol.sehuacho.com:0',
        'srt://sehcontrol.sehuacho.com:65536',
        'srt://@sehcontrol.sehuacho.com:8890',
        'srt://user@sehcontrol.sehuacho.com:8890',
        'srt://user:pass@sehcontrol.sehuacho.com:8890',
        'srt://sehcontrol.sehuacho.com:8890/path#fragment',
        'srt://sehcontrol.sehuacho.com:8890?streamid=existing',
        'srt://sehcontrol.sehuacho.com:8890?StreamId=existing',
      ];
      for (var index = 0; index < invalidUrls.length; index++) {
        expect(
          parseScreenCamPreviewStart(
            validStartData(publishUrl: invalidUrls[index]),
          ),
          isNull,
          reason: 'URL variant $index should be rejected',
        );
      }
    });

    test('accepts unrelated query parameters', () {
      final message = parseScreenCamPreviewStart(validStartData(
        publishUrl:
            'srt://sehcontrol.sehuacho.com:8890?latency=120&transtype=live',
      ));
      expect(message, isNotNull);
    });

    test('rejects missing, empty or non-string publish tokens', () {
      final missing = validStartData()..remove('publish_token');
      expect(parseScreenCamPreviewStart(missing), isNull);
      expect(
        parseScreenCamPreviewStart(validStartData(publishToken: '  ')),
        isNull,
      );
      expect(
        parseScreenCamPreviewStart(validStartData(publishToken: 123)),
        isNull,
      );
    });

    test('rejects missing or empty stream names', () {
      final missing = validStartData()..remove('stream_name');
      expect(parseScreenCamPreviewStart(missing), isNull);
      expect(
        parseScreenCamPreviewStart(validStartData(streamName: '  ')),
        isNull,
      );
    });

    test('requires an integer expiration in the inclusive range 60 to 1800',
        () {
      final missing = validStartData()..remove('expires_in');
      expect(parseScreenCamPreviewStart(missing), isNull);

      for (final value in ['1800', 1.0, 0, -1, 59, 1801]) {
        expect(
          parseScreenCamPreviewStart(validStartData(expiresIn: value)),
          isNull,
          reason: 'Expiration variant should be rejected',
        );
      }
      expect(
        parseScreenCamPreviewStart(validStartData(expiresIn: 60)),
        isNotNull,
      );
      expect(
        parseScreenCamPreviewStart(validStartData(expiresIn: 1800)),
        isNotNull,
      );
    });

    test('allows unknown fields', () {
      expect(
        parseScreenCamPreviewStart({
          ...validStartData(),
          'future_field': true,
        }),
        isNotNull,
      );
    });

    test('default diagnostics do not expose the secret', () {
      final message = parseScreenCamPreviewStart(validStartData())!;
      expect(message.toString(), isNot(contains(message.publishToken)));
    });
  });

  group('ScreenCam Preview STOP parser', () {
    test('accepts a valid message and trims identifiers', () {
      final message = parseScreenCamPreviewStop(validStopData(
        sessionId: ' pv_8f12ab34c5 ',
        rustdeskId: ' 485236790 ',
      ));

      expect(message, isNotNull);
      expect(message!.sessionId, 'pv_8f12ab34c5');
      expect(message.rustdeskId, '485236790');
    });

    test('rejects absent or non-map data', () {
      expect(parseScreenCamPreviewStop(null), isNull);
      expect(parseScreenCamPreviewStop('invalid'), isNull);
      expect(parseScreenCamPreviewStop(const [1, 2]), isNull);
    });

    test('rejects missing, empty and numeric fields', () {
      final missingSession = validStopData()..remove('session_id');
      final missingRustdeskId = validStopData()..remove('rustdesk_id');

      expect(parseScreenCamPreviewStop(missingSession), isNull);
      expect(parseScreenCamPreviewStop(missingRustdeskId), isNull);
      expect(
        parseScreenCamPreviewStop(validStopData(sessionId: '  ')),
        isNull,
      );
      expect(
        parseScreenCamPreviewStop(validStopData(rustdeskId: '  ')),
        isNull,
      );
      expect(
        parseScreenCamPreviewStop(validStopData(sessionId: 123)),
        isNull,
      );
      expect(
        parseScreenCamPreviewStop(validStopData(rustdeskId: 485236790)),
        isNull,
      );
    });

    test('allows unknown fields', () {
      expect(
        parseScreenCamPreviewStop({
          ...validStopData(),
          'future_field': true,
        }),
        isNotNull,
      );
    });
  });

  group('ScreenCam Preview filtering and dispatch', () {
    test('matches IDs using only exterior trim', () {
      expect(screenCamPreviewIdsMatch(' 485236790 ', '485236790'), isTrue);
      expect(screenCamPreviewIdsMatch('AB CD', 'AB CD'), isTrue);
      expect(screenCamPreviewIdsMatch('AB CD', 'ABCD'), isFalse);
      expect(screenCamPreviewIdsMatch('Device-A', 'device-a'), isFalse);
    });

    test('dispatches START only for the matching local ID', () async {
      ScreenCamPreviewStartMessage? received;
      await dispatchScreenCamPreviewStart(
        validStartData(rustdeskId: ' 485236790 '),
        isWindowsPlatform: true,
        getLocalId: () async => ' 485236790 ',
        onValidated: (message) async => received = message,
      );
      expect(received?.sessionId, 'pv_8f12ab34c5');

      received = null;
      await dispatchScreenCamPreviewStart(
        validStartData(),
        isWindowsPlatform: true,
        getLocalId: () async => 'different-id',
        onValidated: (message) async => received = message,
      );
      expect(received, isNull);
    });

    test('dispatches STOP only for the matching local ID', () async {
      ScreenCamPreviewStopMessage? received;
      await dispatchScreenCamPreviewStop(
        validStopData(),
        isWindowsPlatform: true,
        getLocalId: () async => '485236790',
        onValidated: (message) async => received = message,
      );
      expect(received?.sessionId, 'pv_8f12ab34c5');

      received = null;
      await dispatchScreenCamPreviewStop(
        validStopData(),
        isWindowsPlatform: true,
        getLocalId: () async => 'different-id',
        onValidated: (message) async => received = message,
      );
      expect(received, isNull);
    });

    test('non-Windows ignores START and STOP before reading identity',
        () async {
      var identityReads = 0;
      var handlerCalls = 0;
      Future<String> getLocalId() async {
        identityReads++;
        return '485236790';
      }

      await dispatchScreenCamPreviewStart(
        validStartData(),
        isWindowsPlatform: false,
        getLocalId: getLocalId,
        onValidated: (_) async => handlerCalls++,
      );
      await dispatchScreenCamPreviewStop(
        validStopData(),
        isWindowsPlatform: false,
        getLocalId: getLocalId,
        onValidated: (_) async => handlerCalls++,
      );

      expect(identityReads, 0);
      expect(handlerCalls, 0);
    });

    test('identity errors and malformed messages do not escape', () async {
      var handlerCalls = 0;
      Future<String> failingIdentity() async => throw StateError('unavailable');

      await expectLater(
        dispatchScreenCamPreviewStart(
          validStartData(),
          isWindowsPlatform: true,
          getLocalId: failingIdentity,
          onValidated: (_) async => handlerCalls++,
        ),
        completes,
      );
      await expectLater(
        dispatchScreenCamPreviewStop(
          {'session_id': 123},
          isWindowsPlatform: true,
          getLocalId: failingIdentity,
          onValidated: (_) async => handlerCalls++,
        ),
        completes,
      );
      await expectLater(
        dispatchScreenCamPreviewStart(
          validStartData(),
          isWindowsPlatform: true,
          getLocalId: () async => '485236790',
          onValidated: (_) async => throw StateError('ipc unavailable'),
        ),
        completes,
      );
      expect(handlerCalls, 0);
    });

    test('validated no-op callback does not retain START data', () async {
      ScreenCamPreviewStartMessage? transient;
      await dispatchScreenCamPreviewStart(
        validStartData(),
        isWindowsPlatform: true,
        getLocalId: () async => '485236790',
        onValidated: (message) async {
          transient = message;
        },
      );
      expect(transient, isNotNull);
      transient = null;
      expect(transient, isNull);
    });
  });
}
