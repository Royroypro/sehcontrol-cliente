// The realtime push channel's lifecycle, extracted from UserModel so it can be
// tested without opening a socket.
//
// The previous inline implementation reconnected only by luck: several of its
// exits left no timer armed and no socket open, which is a state nothing can
// recover from — the client then stays silent until the app restarts, even
// though the session is perfectly valid. The rules below exist because each one
// corresponds to a way that happened:
//
// - **Every failure path schedules a retry.** There is no `return` that leaves
//   the controller idle while a session is active, including "no api_server
//   yet" and "no access_token yet": those are transient during login and start
//   -up, not permanent.
// - **One socket, one timer, one pending reconnect.** Guarded by explicit
//   state rather than by the callers happening to call things in order.
// - **Callbacks carry a generation.** A socket that has been replaced can still
//   deliver `onDone` seconds later; without an identity check that late
//   callback tears down its successor.
// - **A deliberate close never reconnects.** Logout and replacement close the
//   socket the same way a network drop does, so intent has to be explicit.
// - **Liveness is proven by pongs, not by writes succeeding.** A container
//   restart behind a proxy routinely leaves a half-open socket: writes keep
//   succeeding into a connection nothing will ever answer, and `onDone` never
//   arrives. Only a missing pong catches that.
//
// Nothing here logs the token or a URL containing it.

import 'dart:async';
import 'dart:convert';

/// The socket surface the controller needs. An interface rather than
/// `WebSocketChannel` directly so tests can drive connect/close/messages
/// deterministically.
abstract class RealtimeSocket {
  /// Completes when the handshake succeeded, or with an error when it failed.
  /// `WebSocketChannel.connect` returns before this resolves, which is why the
  /// controller cannot treat a constructed channel as a connected one.
  Future<void> get ready;

  Stream<dynamic> get stream;

  void send(String data);

  Future<void> close();
}

typedef RealtimeSocketFactory = RealtimeSocket Function(Uri url);
typedef RealtimeApiServerProvider = Future<String> Function();
typedef RealtimeTokenProvider = String Function();
typedef RealtimeEventHandler = void Function(Map<Object?, Object?> event);
typedef RealtimeLogger = void Function(String message);

/// Why the controller is not currently connected. Exposed for diagnostics and
/// for tests; never rendered to the user.
enum RealtimeStatus {
  /// No session: `stop()` was called, or `start()` never was.
  idle,
  connecting,
  connected,

  /// Waiting out the reconnect delay.
  waiting,
}

class RealtimeChannelController {
  RealtimeChannelController({
    required RealtimeApiServerProvider apiServerProvider,
    required RealtimeTokenProvider tokenProvider,
    required RealtimeSocketFactory socketFactory,
    required RealtimeEventHandler onEvent,
    RealtimeLogger? logger,
    this.pingInterval = const Duration(seconds: 30),
    this.pongTimeout = const Duration(seconds: 60),
    this.reconnectDelay = const Duration(seconds: 5),
    this.connectTimeout = const Duration(seconds: 20),
  })  : _apiServerProvider = apiServerProvider,
        _tokenProvider = tokenProvider,
        _socketFactory = socketFactory,
        _onEvent = onEvent,
        _log = logger ?? _defaultLogger;

  static void _defaultLogger(String message) {
    // ignore: avoid_print
    print(message);
  }

  final RealtimeApiServerProvider _apiServerProvider;
  final RealtimeTokenProvider _tokenProvider;
  final RealtimeSocketFactory _socketFactory;
  final RealtimeEventHandler _onEvent;
  final RealtimeLogger _log;

  /// Ping cadence. `pongTimeout` must stay comfortably above it so a single
  /// missed round trip is not read as a dead connection.
  final Duration pingInterval;
  final Duration pongTimeout;
  final Duration reconnectDelay;
  final Duration connectTimeout;

  /// Whether a session is active. The single source of truth for "should this
  /// controller be trying at all" — the old code inferred it from an unrelated
  /// polling timer, which made every reconnect depend on that timer's state.
  bool _running = false;

  /// Bumped on every connect attempt and on every teardown. Each socket's
  /// callbacks capture the value they were created with and do nothing once it
  /// no longer matches, which is what makes a late `onDone` from a replaced
  /// socket harmless.
  int _generation = 0;

  RealtimeSocket? _socket;
  StreamSubscription<dynamic>? _subscription;
  Timer? _pingTimer;
  Timer? _reconnectTimer;
  DateTime? _lastPongAt;
  bool _connecting = false;
  RealtimeStatus _status = RealtimeStatus.idle;

  RealtimeStatus get status => _status;
  bool get isConnected => _status == RealtimeStatus.connected;
  int get generation => _generation;
  DateTime? get lastPongAt => _lastPongAt;

  /// Clock seam: tests drive time with `fakeAsync`, whose `DateTime.now()` is
  /// not advanced, so the pong watchdog reads elapsed time from here.
  DateTime Function() nowProvider = DateTime.now;

  /// Marks the session active and connects. Safe to call repeatedly — a second
  /// call while connected is a no-op rather than a reconnect, so the periodic
  /// callers that used to churn the socket no longer do.
  void start() {
    _running = true;
    if (_socket != null || _connecting) return;
    _connect();
  }

  /// Credentials may have just changed (a login persisted a new token).
  /// Reconnects even when a socket is already up, because the existing one is
  /// authenticated with the previous token.
  void restart() {
    _running = true;
    _teardown(allowReconnect: false);
    _connect();
  }

  /// Ends the session: no socket, no timers, and no reconnect will be armed
  /// afterwards.
  void stop() {
    _running = false;
    _teardown(allowReconnect: false);
    _status = RealtimeStatus.idle;
  }

  /// Sends an application payload. Returns false when there is no live socket,
  /// so callers never assume delivery.
  bool send(Map<String, Object?> payload) {
    final socket = _socket;
    if (socket == null || _status != RealtimeStatus.connected) return false;
    try {
      socket.send(jsonEncode(payload));
      return true;
    } catch (_) {
      return false;
    }
  }

  void _connect() {
    if (!_running) return;
    if (_connecting || _socket != null) return;
    _connecting = true;
    _status = RealtimeStatus.connecting;
    final generation = ++_generation;
    unawaited(_openSocket(generation));
  }

  Future<void> _openSocket(int generation) async {
    Uri? endpoint;
    try {
      final apiServer = (await _apiServerProvider()).trim();
      if (generation != _generation) return;
      if (apiServer.isEmpty) {
        _failAttempt(generation, 'Realtime skipped: missing API server');
        return;
      }
      final token = _tokenProvider();
      if (token.isEmpty) {
        // Transient during login: the token is persisted a moment after the
        // credentials come back. Retrying is what turns this from a permanent
        // dead end into a one-retry delay.
        _failAttempt(generation, 'Realtime skipped: missing access token');
        return;
      }
      endpoint = buildEndpoint(apiServer, token);
      if (endpoint == null) {
        _failAttempt(generation, 'Realtime skipped: invalid API server');
        return;
      }
    } catch (_) {
      // Reading the API server goes through the FFI and can throw; the old code
      // did this outside its try, so a throw here escaped as an unhandled error
      // and no retry was ever armed.
      _failAttempt(generation, 'Realtime skipped: configuration unavailable');
      return;
    }

    _log('Realtime connecting to ${_safeTarget(endpoint)}');
    final RealtimeSocket socket;
    try {
      socket = _socketFactory(endpoint);
    } catch (_) {
      _failAttempt(generation, 'Realtime connect failed');
      return;
    }
    if (generation != _generation) {
      unawaited(_closeQuietly(socket));
      return;
    }
    _socket = socket;

    // The stream is listened to before awaiting `ready` so a handshake failure
    // surfaces through exactly one path.
    _subscription = socket.stream.listen(
      (raw) => _handleMessage(generation, raw),
      onDone: () => _handleDrop(generation, 'Realtime disconnected'),
      onError: (Object _) => _handleDrop(generation, 'Realtime disconnected'),
      cancelOnError: true,
    );

    try {
      await socket.ready.timeout(connectTimeout);
    } catch (_) {
      if (generation != _generation) return;
      _handleDrop(generation, 'Realtime connect failed');
      return;
    }
    if (generation != _generation) return;

    _connecting = false;
    _status = RealtimeStatus.connected;
    _lastPongAt = nowProvider();
    _log('Realtime connected');
    _startPing(generation);
  }

  /// A failed attempt that never produced a socket: clear the in-flight flag
  /// and arm the retry.
  void _failAttempt(int generation, String message) {
    if (generation != _generation) return;
    _connecting = false;
    _log(message);
    _scheduleReconnect();
  }

  void _startPing(int generation) {
    _pingTimer?.cancel();
    _pingTimer = Timer.periodic(pingInterval, (_) {
      if (generation != _generation) return;
      final lastPong = _lastPongAt;
      // A write succeeding proves nothing about the peer, so the watchdog runs
      // before the next ping rather than relying on `send` to throw.
      if (lastPong != null &&
          nowProvider().difference(lastPong) > pongTimeout) {
        _log('Realtime pong timeout');
        _handleDrop(generation, 'Realtime disconnected');
        return;
      }
      try {
        _socket?.send('ping');
      } catch (_) {
        _handleDrop(generation, 'Realtime disconnected');
      }
    });
  }

  void _handleMessage(int generation, dynamic raw) {
    if (generation != _generation) return;
    if (raw is! String) return;
    final Object? decoded;
    try {
      decoded = jsonDecode(raw);
    } catch (_) {
      return;
    }
    if (decoded is! Map) return;
    final type = decoded['type'];
    if (type == 'pong') {
      _lastPongAt = nowProvider();
      return;
    }
    if (type == 'connected') {
      // Some deployments answer the handshake with this before anything else;
      // treat it as proof of a live peer too.
      _lastPongAt = nowProvider();
    }
    _onEvent(decoded.cast<Object?, Object?>());
  }

  /// An involuntary loss: tear the socket down and arm a retry.
  void _handleDrop(int generation, String message) {
    if (generation != _generation) return;
    _log(message);
    _teardown(allowReconnect: true);
  }

  void _teardown({required bool allowReconnect}) {
    // Bumping first invalidates every callback still in flight, so nothing that
    // arrives during or after this teardown can touch the next socket.
    _generation++;
    _connecting = false;
    _pingTimer?.cancel();
    _pingTimer = null;
    _reconnectTimer?.cancel();
    _reconnectTimer = null;
    _lastPongAt = null;
    final subscription = _subscription;
    _subscription = null;
    if (subscription != null) {
      unawaited(subscription.cancel().catchError((_) {}));
    }
    final socket = _socket;
    _socket = null;
    if (socket != null) {
      unawaited(_closeQuietly(socket));
    }
    if (allowReconnect && _running) {
      _scheduleReconnect();
    } else if (!_running) {
      _status = RealtimeStatus.idle;
    }
  }

  Future<void> _closeQuietly(RealtimeSocket socket) async {
    try {
      await socket.close();
    } catch (_) {
      // Closing a socket whose handshake failed throws; that is not news.
    }
  }

  void _scheduleReconnect() {
    if (!_running) return;
    if (_reconnectTimer != null) return;
    _status = RealtimeStatus.waiting;
    _log('Realtime reconnect scheduled');
    _reconnectTimer = Timer(reconnectDelay, () {
      _reconnectTimer = null;
      _connect();
    });
  }

  /// `https://host/base` becomes `wss` on `/base/api/ws` with the credential
  /// carried as a query parameter.
  ///
  /// Built with `Uri` rather than a string replace: the old
  /// `replaceFirst('http', 'ws')` produced a double slash for a configured
  /// server with a trailing slash, and would have rewritten an `http` appearing
  /// anywhere else in the string.
  static Uri? buildEndpoint(String apiServer, String token) {
    final Uri base;
    try {
      base = Uri.parse(apiServer.trim());
    } catch (_) {
      return null;
    }
    if (base.host.isEmpty) return null;
    final scheme = switch (base.scheme) {
      'https' || 'wss' => 'wss',
      'http' || 'ws' => 'ws',
      _ => null,
    };
    if (scheme == null) return null;
    final segments = [
      ...base.pathSegments.where((segment) => segment.isNotEmpty),
      'api',
      'ws',
    ];
    return base.replace(
      scheme: scheme,
      pathSegments: segments,
      queryParameters: {'token': token},
    );
  }

  /// Host and path only: the query carries the JWT and must never be logged.
  static String _safeTarget(Uri endpoint) =>
      '${endpoint.scheme}://${endpoint.host}'
      '${endpoint.hasPort ? ':${endpoint.port}' : ''}${endpoint.path}';
}
