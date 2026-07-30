import 'dart:async';
import 'dart:convert';

import 'package:fake_async/fake_async.dart';
import 'package:flutter_hbb/common/realtime_channel.dart';
import 'package:flutter_test/flutter_test.dart';

/// A socket the test drives by hand: nothing here opens a connection, so the
/// whole lifecycle — handshake, drops, pongs, teardown — is deterministic.
class FakeSocket implements RealtimeSocket {
  FakeSocket(this.url);

  final Uri url;
  final Completer<void> _ready = Completer<void>();
  final StreamController<dynamic> _incoming =
      StreamController<dynamic>.broadcast();
  final List<String> sent = <String>[];
  bool closed = false;

  @override
  Future<void> get ready => _ready.future;

  @override
  Stream<dynamic> get stream => _incoming.stream;

  @override
  void send(String data) {
    if (closed) throw StateError('socket closed');
    sent.add(data);
  }

  @override
  Future<void> close() async {
    if (closed) return;
    closed = true;
    await _incoming.close();
  }

  void completeHandshake() => _ready.complete();
  void failHandshake() => _ready.completeError(StateError('handshake failed'));

  /// No-op once closed: a replaced socket really is shut, and a test asserting
  /// "nothing arrives from it" should not have to special-case that.
  void deliver(Map<String, Object?> event) {
    if (_incoming.isClosed) return;
    _incoming.add(jsonEncode(event));
  }

  /// The peer went away: what a panel restart looks like when the FIN arrives.
  void dropFromPeer() {
    if (!_incoming.isClosed) _incoming.close();
  }

  void errorFromPeer() {
    if (!_incoming.isClosed) _incoming.addError(StateError('reset by peer'));
  }
}

/// Everything the controller depends on, wired for a test.
class Harness {
  Harness({
    this.apiServer = 'https://panel.example.com',
    this.token = 'jwt-placeholder',
  });

  String apiServer;
  String token;
  bool apiServerThrows = false;

  final List<FakeSocket> sockets = <FakeSocket>[];
  final List<Map<Object?, Object?>> events = <Map<Object?, Object?>>[];
  final List<String> logs = <String>[];
  DateTime now = DateTime.utc(2026, 1, 1);

  late final RealtimeChannelController controller = RealtimeChannelController(
    apiServerProvider: () async {
      if (apiServerThrows) throw StateError('ffi unavailable');
      return apiServer;
    },
    tokenProvider: () => token,
    socketFactory: (url) {
      final socket = FakeSocket(url);
      sockets.add(socket);
      return socket;
    },
    onEvent: events.add,
    logger: logs.add,
    pingInterval: const Duration(seconds: 30),
    pongTimeout: const Duration(seconds: 60),
    reconnectDelay: const Duration(seconds: 5),
    connectTimeout: const Duration(seconds: 20),
  )..nowProvider = () => now;

  FakeSocket get latest => sockets.last;

  /// Advances both the timer wheel and the clock the pong watchdog reads, so
  /// they can never disagree.
  void elapse(FakeAsync async, Duration duration) {
    now = now.add(duration);
    async.elapse(duration);
  }

  /// Brings a freshly started controller to the connected state.
  void settleConnected(FakeAsync async) {
    async.flushMicrotasks();
    latest.completeHandshake();
    async.flushMicrotasks();
  }
}

void main() {
  group('endpoint construction', () {
    test('https becomes wss and the token travels in the query', () {
      final url = RealtimeChannelController.buildEndpoint(
          'https://panel.example.com', 'abc')!;
      expect(url.scheme, 'wss');
      expect(url.host, 'panel.example.com');
      expect(url.path, '/api/ws');
      expect(url.queryParameters['token'], 'abc');
    });

    test('a trailing slash does not produce a double slash', () {
      final url = RealtimeChannelController.buildEndpoint(
          'https://panel.example.com/', 'abc')!;
      expect(url.path, '/api/ws');
      expect(url.toString(), isNot(contains('//api')));
    });

    test('a base path is preserved', () {
      final url = RealtimeChannelController.buildEndpoint(
          'https://panel.example.com/sehcontrol', 'abc')!;
      expect(url.path, '/sehcontrol/api/ws');
    });

    test('plain http becomes ws, and "http" elsewhere is not rewritten', () {
      final url = RealtimeChannelController.buildEndpoint(
          'http://panel.example.com/http-api', 'abc')!;
      expect(url.scheme, 'ws');
      expect(url.path, '/http-api/api/ws');
    });

    test('garbage is rejected instead of producing a bad socket', () {
      expect(RealtimeChannelController.buildEndpoint('', 'abc'), isNull);
      expect(
          RealtimeChannelController.buildEndpoint('not a url', 'abc'), isNull);
      expect(
          RealtimeChannelController.buildEndpoint('ftp://host', 'abc'), isNull);
    });
  });

  group('connect preconditions', () {
    // 1
    test('no token: nothing is opened, but a retry is armed', () {
      fakeAsync((async) {
        final harness = Harness(token: '');
        harness.controller.start();
        async.flushMicrotasks();

        expect(harness.sockets, isEmpty);
        expect(
            harness.logs, contains('Realtime skipped: missing access token'));
        expect(harness.controller.status, RealtimeStatus.waiting);

        // The token lands a moment later, as it does during login.
        harness.token = 'jwt-placeholder';
        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();
        expect(harness.sockets, hasLength(1),
            reason: 'a missing token must be transient, not terminal');

        harness.controller.stop();
      });
    });

    // 2
    test('no api server: nothing is opened, but a retry is armed', () {
      fakeAsync((async) {
        final harness = Harness(apiServer: '');
        harness.controller.start();
        async.flushMicrotasks();

        expect(harness.sockets, isEmpty);
        expect(harness.logs, contains('Realtime skipped: missing API server'));

        harness.apiServer = 'https://panel.example.com';
        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();
        expect(harness.sockets, hasLength(1));

        harness.controller.stop();
      });
    });

    test('a throwing api server provider still arms a retry', () {
      fakeAsync((async) {
        final harness = Harness()..apiServerThrows = true;
        harness.controller.start();
        async.flushMicrotasks();

        expect(harness.sockets, isEmpty);
        expect(harness.logs,
            contains('Realtime skipped: configuration unavailable'));

        harness.apiServerThrows = false;
        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();
        expect(harness.sockets, hasLength(1));

        harness.controller.stop();
      });
    });

    // 3
    test('token and api server present: one socket, correct url', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);

        expect(harness.sockets, hasLength(1));
        expect(harness.latest.url.scheme, 'wss');
        expect(harness.latest.url.host, 'panel.example.com');
        expect(harness.latest.url.path, '/api/ws');
        expect(
            harness.latest.url.queryParameters['token'], 'jwt-placeholder');
        expect(harness.controller.isConnected, isTrue);
        expect(harness.logs, contains('Realtime connected'));

        harness.controller.stop();
      });
    });

    test('the log never contains the token', () {
      fakeAsync((async) {
        final harness = Harness(token: 'super-secret-jwt');
        harness.controller.start();
        harness.settleConnected(async);
        harness.latest.dropFromPeer();
        async.flushMicrotasks();

        for (final line in harness.logs) {
          expect(line, isNot(contains('super-secret-jwt')));
          expect(line, isNot(contains('token=')));
        }
        harness.controller.stop();
      });
    });

    // 5
    test('a start while already connected does not churn the socket', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);

        harness.controller.start();
        harness.controller.start();
        async.flushMicrotasks();

        expect(harness.sockets, hasLength(1));
        expect(harness.latest.closed, isFalse);

        harness.controller.stop();
      });
    });
  });

  group('reconnection', () {
    // 4
    test('onDone schedules a reconnect and a new socket appears', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);

        harness.latest.dropFromPeer();
        async.flushMicrotasks();
        expect(harness.logs, contains('Realtime disconnected'));
        expect(harness.controller.status, RealtimeStatus.waiting);
        expect(harness.sockets, hasLength(1), reason: 'not before the delay');

        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();
        expect(harness.sockets, hasLength(2));

        harness.controller.stop();
      });
    });

    // 5
    test('onError schedules a reconnect', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);

        harness.latest.errorFromPeer();
        async.flushMicrotasks();
        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();

        expect(harness.sockets, hasLength(2));
        harness.controller.stop();
      });
    });

    test('a failed handshake schedules a reconnect', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        async.flushMicrotasks();
        harness.latest.failHandshake();
        async.flushMicrotasks();

        expect(harness.controller.isConnected, isFalse);
        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();
        expect(harness.sockets, hasLength(2));

        harness.controller.stop();
      });
    });

    test('it keeps retrying while the panel stays down', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        async.flushMicrotasks();

        // Five consecutive failed handshakes: the old implementation stopped
        // after the first dead end.
        for (var attempt = 0; attempt < 5; attempt++) {
          harness.latest.failHandshake();
          async.flushMicrotasks();
          harness.elapse(async, const Duration(seconds: 5));
          async.flushMicrotasks();
        }
        expect(harness.sockets, hasLength(6));

        // And when it comes back, it connects.
        harness.settleConnected(async);
        expect(harness.controller.isConnected, isTrue);

        harness.controller.stop();
      });
    });

    // 6
    test('repeated drops do not stack reconnect timers', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);

        final socket = harness.latest;
        socket.dropFromPeer();
        socket.errorFromPeer();
        socket.dropFromPeer();
        async.flushMicrotasks();

        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();

        expect(harness.sockets, hasLength(2),
            reason: 'three drop signals must still mean one reconnect');
        expect(
            harness.logs
                .where((l) => l == 'Realtime reconnect scheduled')
                .length,
            1);

        harness.controller.stop();
      });
    });

    // 7
    test('a replaced socket cannot tear down its successor', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);
        final stale = harness.latest;

        // A new token arrives: restart() replaces the socket.
        harness.token = 'second-token';
        harness.controller.restart();
        harness.settleConnected(async);
        final current = harness.latest;
        expect(harness.sockets, hasLength(2));
        expect(current, isNot(same(stale)));
        expect(harness.controller.isConnected, isTrue);

        // The old socket only now reports that it went away.
        stale.dropFromPeer();
        stale.errorFromPeer();
        async.flushMicrotasks();
        harness.elapse(async, const Duration(seconds: 10));
        async.flushMicrotasks();

        expect(harness.controller.isConnected, isTrue,
            reason: 'the live socket must survive a late callback');
        expect(harness.sockets, hasLength(2),
            reason: 'and nothing extra opened');
        expect(current.closed, isFalse);

        harness.controller.stop();
      });
    });

    test('a replaced socket is shut, so nothing can arrive from it', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);
        final stale = harness.latest;

        harness.controller.restart();
        harness.settleConnected(async);

        // Two layers protect this: the socket is closed and unsubscribed, and
        // even if a frame did arrive its generation no longer matches.
        expect(stale.closed, isTrue);
        stale.deliver({'type': 'membership_status', 'data': {}});
        async.flushMicrotasks();
        expect(harness.events, isEmpty);

        harness.latest.deliver({'type': 'membership_status', 'data': {}});
        async.flushMicrotasks();
        expect(harness.events, hasLength(1));

        harness.controller.stop();
      });
    });
  });

  group('liveness', () {
    // 11
    test('a pong refreshes the deadline and is not an app event', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);
        final firstPong = harness.controller.lastPongAt;

        harness.elapse(async, const Duration(seconds: 30));
        async.flushMicrotasks();
        expect(harness.latest.sent, contains('ping'));

        harness.latest.deliver({'type': 'pong'});
        async.flushMicrotasks();

        expect(harness.controller.lastPongAt, isNot(firstPong));
        expect(harness.events, isEmpty, reason: 'pong is transport, not app');

        harness.controller.stop();
      });
    });

    // 9
    test('a socket that stops answering is dropped and reconnected', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);
        final dead = harness.latest;

        // The peer is gone but the socket never reports it — writes keep
        // succeeding. Only the missing pong can catch this.
        harness.elapse(async, const Duration(seconds: 30));
        async.flushMicrotasks();
        harness.elapse(async, const Duration(seconds: 30));
        async.flushMicrotasks();
        harness.elapse(async, const Duration(seconds: 30));
        async.flushMicrotasks();

        expect(harness.logs, contains('Realtime pong timeout'));
        expect(dead.closed, isTrue);

        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();
        expect(harness.sockets, hasLength(2));

        harness.controller.stop();
      });
    });

    test('a socket that keeps answering is never dropped', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);

        for (var round = 0; round < 10; round++) {
          harness.elapse(async, const Duration(seconds: 30));
          async.flushMicrotasks();
          harness.latest.deliver({'type': 'pong'});
          async.flushMicrotasks();
        }

        expect(harness.controller.isConnected, isTrue);
        expect(harness.sockets, hasLength(1));
        expect(harness.logs, isNot(contains('Realtime pong timeout')));

        harness.controller.stop();
      });
    });
  });

  group('session lifecycle', () {
    // 8 + 10
    test('stop cancels everything and never reconnects', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);
        final socket = harness.latest;

        harness.controller.stop();
        async.flushMicrotasks();

        expect(socket.closed, isTrue);
        expect(harness.controller.status, RealtimeStatus.idle);

        // The intentional close makes the stream fire onDone; that must not be
        // mistaken for a network drop.
        harness.elapse(async, const Duration(minutes: 5));
        async.flushMicrotasks();
        expect(harness.sockets, hasLength(1));
      });
    });

    test('stop while a reconnect is pending cancels it', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);

        harness.latest.dropFromPeer();
        async.flushMicrotasks();
        expect(harness.controller.status, RealtimeStatus.waiting);

        harness.controller.stop();
        harness.elapse(async, const Duration(minutes: 5));
        async.flushMicrotasks();

        expect(harness.sockets, hasLength(1));
        expect(harness.controller.status, RealtimeStatus.idle);
      });
    });

    // 12 + 13: the login flows persist the token and then ask for a refresh.
    test('restart after a token is persisted uses the new credential', () {
      fakeAsync((async) {
        // startMembershipPolling() runs before login.dart persists the token,
        // which is the ordering that broke the original implementation.
        final harness = Harness(token: '');
        harness.controller.start();
        async.flushMicrotasks();
        expect(harness.sockets, isEmpty);

        harness.token = 'token-from-login';
        harness.controller.restart();
        harness.settleConnected(async);

        expect(harness.sockets, hasLength(1));
        expect(harness.latest.url.query, 'token=token-from-login');
        expect(harness.controller.isConnected, isTrue);

        harness.controller.stop();
      });
    });

    test('restart replaces a socket authenticated with an older token', () {
      fakeAsync((async) {
        final harness = Harness(token: 'old-token');
        harness.controller.start();
        harness.settleConnected(async);
        final old = harness.latest;

        harness.token = 'new-token';
        harness.controller.restart();
        harness.settleConnected(async);

        expect(old.closed, isTrue);
        expect(harness.latest.url.query, 'token=new-token');
        expect(harness.sockets, hasLength(2));

        harness.controller.stop();
      });
    });

    // 14
    test('app start with a stored token connects without a login', () {
      fakeAsync((async) {
        final harness = Harness(token: 'stored-token');
        harness.controller.start();
        harness.settleConnected(async);

        expect(harness.controller.isConnected, isTrue);
        expect(harness.latest.url.query, 'token=stored-token');

        harness.controller.stop();
      });
    });

    test('a full panel restart cycle recovers end to end', () {
      fakeAsync((async) {
        final harness = Harness();
        harness.controller.start();
        harness.settleConnected(async);

        // Panel goes down.
        harness.latest.dropFromPeer();
        async.flushMicrotasks();
        // Still booting: the first attempt fails.
        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();
        harness.latest.failHandshake();
        async.flushMicrotasks();
        // Second attempt succeeds.
        harness.elapse(async, const Duration(seconds: 5));
        async.flushMicrotasks();
        harness.settleConnected(async);

        expect(harness.controller.isConnected, isTrue);
        expect(harness.sockets, hasLength(3));

        // And the push the whole feature depends on now arrives.
        harness.latest.deliver({
          'type': 'screen_cam.preview.start',
          'data': {'session_id': 'pv_1'},
        });
        async.flushMicrotasks();
        expect(harness.events, hasLength(1));
        expect(harness.events.single['type'], 'screen_cam.preview.start');

        harness.controller.stop();
      });
    });

    test('send only reports success on a live socket', () {
      fakeAsync((async) {
        final harness = Harness();
        expect(harness.controller.send({'type': 'x'}), isFalse);

        harness.controller.start();
        harness.settleConnected(async);
        expect(harness.controller.send({'type': 'x'}), isTrue);
        expect(harness.latest.sent.last, jsonEncode({'type': 'x'}));

        harness.controller.stop();
        expect(harness.controller.send({'type': 'x'}), isFalse);
      });
    });
  });
}
