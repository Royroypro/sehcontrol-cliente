import 'package:flutter_hbb/common/screen_cam_preview_lifecycle.dart';
import 'package:flutter_test/flutter_test.dart';

/// A snapshot shaped exactly like the one the Rust poller pushes.
Map<String, Object?> snapshot({
  String event = 'screen_cam.preview.started',
  Object? sessionId = 'pv_8f12ab34c5',
  Object? rustdeskId = '485236790',
  Object? generation = 3,
  Object? sequence = 7,
  Map<String, Object?> extra = const {},
}) =>
    {
      'name': 'screencam_preview_lifecycle',
      'event': event,
      'session_id': sessionId,
      'rustdesk_id': rustdeskId,
      'generation': generation,
      'sequence': sequence,
      ...extra,
    };

void main() {
  group('parsing and whitelist', () {
    test('a started event becomes exactly the panel contract', () {
      final parsed = parseScreenCamPreviewLifecycle(snapshot())!;

      expect(parsed.payload, {
        'event': 'screen_cam.preview.started',
        'session_id': 'pv_8f12ab34c5',
        'rustdesk_id': '485236790',
      });
      expect(parsed.key, 'pv_8f12ab34c5:3:7:screen_cam.preview.started');
    });

    test('internal ordering fields never reach the panel', () {
      final parsed = parseScreenCamPreviewLifecycle(snapshot())!;
      expect(parsed.payload.containsKey('generation'), isFalse);
      expect(parsed.payload.containsKey('sequence'), isFalse);
      expect(parsed.payload.containsKey('name'), isFalse);
    });

    test('a failed event carries a known reason code', () {
      final parsed = parseScreenCamPreviewLifecycle(snapshot(
        event: 'screen_cam.preview.failed',
        extra: {'reason': 'retries_exhausted'},
      ))!;
      expect(parsed.payload['reason'], 'retries_exhausted');
    });

    test('an unknown reason is replaced rather than forwarded', () {
      final parsed = parseScreenCamPreviewLifecycle(snapshot(
        event: 'screen_cam.preview.failed',
        extra: {'reason': 'srt://host:8890 refused token abc'},
      ))!;
      expect(parsed.payload['reason'], 'unknown');
    });

    test('a field the daemon adds later cannot leak through', () {
      // The publisher sits next to a URL, a token and a stream id, so the
      // payload is rebuilt from a fixed list instead of being edited.
      final parsed = parseScreenCamPreviewLifecycle(snapshot(extra: {
        'publish_url': 'srt://sehcontrol.sehuacho.com:8890',
        'publish_token': 'a-real-token',
        'stream_id': 'publish:pv:token=a-real-token',
        'stream_name': 'pv_8f12ab34c5',
      }))!;

      expect(
          parsed.payload.keys.toSet(), {'event', 'session_id', 'rustdesk_id'});
      final rendered = parsed.payload.toString();
      expect(rendered, isNot(contains('a-real-token')));
      expect(rendered, isNot(contains('srt://')));
      expect(rendered, isNot(contains('publish:')));
    });

    test('unknown events are dropped', () {
      for (final event in [
        'screen_cam.preview.exploded',
        'screen_cam.update',
        'membership_status',
        '',
      ]) {
        expect(parseScreenCamPreviewLifecycle(snapshot(event: event)), isNull,
            reason: event);
      }
    });

    test('a missing or malformed identity is dropped', () {
      expect(parseScreenCamPreviewLifecycle(snapshot(sessionId: null)), isNull);
      expect(parseScreenCamPreviewLifecycle(snapshot(sessionId: '  ')), isNull);
      expect(parseScreenCamPreviewLifecycle(snapshot(sessionId: 7)), isNull);
      expect(
          parseScreenCamPreviewLifecycle(snapshot(rustdeskId: null)), isNull);
      expect(parseScreenCamPreviewLifecycle(snapshot(rustdeskId: '')), isNull);
      expect(
          parseScreenCamPreviewLifecycle(snapshot(generation: null)), isNull);
      expect(
          parseScreenCamPreviewLifecycle(snapshot(sequence: 'eight')), isNull);
      expect(parseScreenCamPreviewLifecycle(snapshot(sequence: -1)), isNull);
      expect(parseScreenCamPreviewLifecycle('not a map'), isNull);
      expect(parseScreenCamPreviewLifecycle(null), isNull);
    });
  });

  group('dispatch, retry and de-duplication', () {
    test('a live channel delivers immediately', () {
      final sent = <Map<String, Object?>>[];
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((payload) {
        sent.add(payload);
        return true;
      });

      expect(dispatcher.handle(snapshot()), isTrue);
      expect(sent, hasLength(1));
      expect(sent.single['event'], 'screen_cam.preview.started');
      expect(dispatcher.hasPending, isFalse);
    });

    test('a send that fails leaves the event owed, not delivered', () {
      var connected = false;
      final sent = <Map<String, Object?>>[];
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((payload) {
        if (!connected) return false;
        sent.add(payload);
        return true;
      });

      // The publisher connects while the WebSocket is down.
      expect(dispatcher.handle(snapshot()), isFalse);
      expect(sent, isEmpty);
      expect(dispatcher.hasPending, isTrue);
      expect(dispatcher.lastDeliveredKey, isNull,
          reason: 'a failed send must not count as delivered');

      // The socket comes back and UserModel flushes on `connected`.
      connected = true;
      expect(dispatcher.flush(), isTrue);
      expect(sent, hasLength(1));
      expect(dispatcher.hasPending, isFalse);
      expect(dispatcher.lastDeliveredKey,
          'pv_8f12ab34c5:3:7:screen_cam.preview.started');
    });

    test('the same transition is not sent twice', () {
      var calls = 0;
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((_) {
        calls++;
        return true;
      });

      dispatcher.handle(snapshot());
      dispatcher.handle(snapshot());
      dispatcher.handle(snapshot());

      expect(calls, 1);
    });

    test('de-duplication keys on the transition, not just the event name', () {
      final sent = <Map<String, Object?>>[];
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((payload) {
        sent.add(payload);
        return true;
      });

      // A retry cycle: failed, then connecting again, then started.
      dispatcher.handle(snapshot(
          event: 'screen_cam.preview.failed',
          sequence: 1,
          extra: {'reason': 'connect_failed'}));
      dispatcher.handle(
          snapshot(event: 'screen_cam.preview.connecting', sequence: 2));
      dispatcher
          .handle(snapshot(event: 'screen_cam.preview.started', sequence: 3));

      expect(sent.map((e) => e['event']), [
        'screen_cam.preview.failed',
        'screen_cam.preview.connecting',
        'screen_cam.preview.started',
      ]);
    });

    test('only the newest owed transition is retried', () {
      var connected = false;
      final sent = <Map<String, Object?>>[];
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((payload) {
        if (!connected) return false;
        sent.add(payload);
        return true;
      });

      dispatcher.handle(
          snapshot(event: 'screen_cam.preview.connecting', sequence: 1));
      dispatcher
          .handle(snapshot(event: 'screen_cam.preview.started', sequence: 2));

      connected = true;
      expect(dispatcher.flush(), isTrue);

      // Replaying `connecting` after `started` would walk the panel backwards.
      expect(sent, hasLength(1));
      expect(sent.single['event'], 'screen_cam.preview.started');
    });

    test('a malformed snapshot never becomes pending', () {
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((_) => true);
      expect(dispatcher.handle(snapshot(event: 'nonsense')), isFalse);
      expect(dispatcher.hasPending, isFalse);
    });

    test('reset clears both what is owed and what was delivered', () {
      var calls = 0;
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((_) {
        calls++;
        return true;
      });

      dispatcher.handle(snapshot());
      expect(calls, 1);

      // Logout: a later session must not be silenced by the old dedup state.
      dispatcher.reset();
      expect(dispatcher.hasPending, isFalse);
      expect(dispatcher.lastDeliveredKey, isNull);

      dispatcher.handle(snapshot());
      expect(calls, 2);
    });

    test('two sessions sharing a counter get distinct keys', () {
      // The daemon restarting resets `generation` and `sequence`, so these two
      // differ only by session.
      final first = parseScreenCamPreviewLifecycle(
          snapshot(sessionId: 'pv_first', generation: 1, sequence: 1))!;
      final second = parseScreenCamPreviewLifecycle(
          snapshot(sessionId: 'pv_second', generation: 1, sequence: 1))!;

      expect(first.key, isNot(second.key));
      expect(first.key, startsWith('pv_first:'));
      expect(second.key, startsWith('pv_second:'));
    });

    test('a restarted daemon does not silence the new session', () {
      final sent = <Map<String, Object?>>[];
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((payload) {
        sent.add(payload);
        return true;
      });

      // A long-lived session, then a restart that rewinds the counters.
      expect(
          dispatcher.handle(
              snapshot(sessionId: 'pv_old', generation: 4, sequence: 9)),
          isTrue);
      expect(
          dispatcher.handle(snapshot(
              sessionId: 'pv_new',
              generation: 1,
              sequence: 1,
              event: 'screen_cam.preview.connecting')),
          isTrue);
      expect(
          dispatcher.handle(
              snapshot(sessionId: 'pv_new', generation: 1, sequence: 2)),
          isTrue);

      expect(sent.map((e) => e['session_id']), ['pv_old', 'pv_new', 'pv_new']);
    });

    test('flush with nothing owed is a no-op', () {
      var calls = 0;
      final dispatcher = ScreenCamPreviewLifecycleDispatcher((_) {
        calls++;
        return true;
      });
      expect(dispatcher.flush(), isFalse);
      expect(calls, 0);
    });
  });
}
