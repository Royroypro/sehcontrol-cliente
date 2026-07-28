import 'dart:async';
import 'dart:convert';

import 'package:bot_toast/bot_toast.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common/hbbs/hbbs.dart';
import 'package:flutter_hbb/models/ab_model.dart';
import 'package:get/get.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import '../common.dart';
import '../utils/http_service.dart' as http;
import 'model.dart';
import 'platform_model.dart';

bool refreshingUser = false;
const _trustedServerKeyOption = 'trusted-server-key';
const _trustedServerKeyFingerprintOption = 'trusted-server-key-fingerprint';

class ServerNotification {
  final String id;
  final String title;
  final String message;
  final DateTime receivedAt;

  const ServerNotification({
    required this.id,
    required this.title,
    required this.message,
    required this.receivedAt,
  });
}

class UserModel {
  final RxString userName = ''.obs;
  final RxString displayName = ''.obs;
  final RxString avatar = ''.obs;
  final RxBool isAdmin = false.obs;
  final RxString networkError = ''.obs;

  // Membership/license gating (Sehcontrol membership panel integration).
  // Cosmetic only: the real enforcement lives server-side in hbbs.
  final RxBool membershipBlocked = false.obs;
  final RxString membershipMessage = ''.obs;
  final RxnInt membershipDaysLeft = RxnInt();
  final RxString membershipPlanName = ''.obs;
  final Rx<DateTime?> membershipExpiresAt = Rx<DateTime?>(null);
  final RxnInt membershipDeviceCount = RxnInt();
  final RxnInt membershipMaxDevices = RxnInt();
  /// Support contact number (no leading "+", e.g. "51948793154"), from
  /// `/api/client-policy`'s `whatsapp_number`. Server-configured on purpose —
  /// used both by the "Soporte" sidebar link and the expiry-warning banner's
  /// "Contactar por WhatsApp" button, so changing the number is an admin-side
  /// change, not a client release. Empty when not configured; both call
  /// sites hide the WhatsApp option entirely rather than show a wrong number.
  final RxString whatsappNumber = ''.obs;
  // Messages received during this session. The server notification is acked
  // immediately, so retain its content locally for the notification bell.
  final RxInt unreadNotificationCount = 0.obs;
  final RxList<ServerNotification> notifications = <ServerNotification>[].obs;
  final Set<String> _seenNotificationIds = {};
  Timer? _membershipTimer;
  Timer? _heartbeatTimer;
  bool _heartbeatInFlight = false;
  bool _heartbeatConfirmed = false;
  WebSocketChannel? _realtimeChannel;
  Timer? _realtimePingTimer;
  bool _realtimeReconnectScheduled = false;

  bool get isLogin => userName.isNotEmpty;
  String get displayNameOrUserName =>
      displayName.value.trim().isEmpty ? userName.value : displayName.value;
  String get accountLabelWithHandle {
    final username = userName.value.trim();
    if (username.isEmpty) {
      return '';
    }
    final preferred = displayName.value.trim();
    if (preferred.isEmpty || preferred == username) {
      return username;
    }
    return '$preferred (@$username)';
  }

  WeakReference<FFI> parent;

  UserModel(this.parent) {
    userName.listen((p0) {
      // When user name becomes empty, show login button
      // When user name becomes non-empty:
      //  For _updateLocalUserInfo, network error will be set later
      //  For login success, should clear network error
      networkError.value = '';
    });
  }

  void refreshCurrentUser() async {
    if (bind.isDisableAccount()) return;
    networkError.value = '';
    final token = bind.mainGetLocalOption(key: 'access_token');
    if (token == '') {
      await updateOtherModels();
      return;
    }
    _updateLocalUserInfo();
    final url = await bind.mainGetApiServer();
    final body = {
      'id': await bind.mainGetMyId(),
      'uuid': await bind.mainGetUuid()
    };
    if (refreshingUser) return;
    try {
      refreshingUser = true;
      final http.Response response;
      try {
        response = await http.post(Uri.parse('$url/api/currentUser'),
            headers: {
              'Content-Type': 'application/json',
              'Authorization': 'Bearer $token'
            },
            body: json.encode(body));
      } catch (e) {
        networkError.value = e.toString();
        rethrow;
      }
      refreshingUser = false;
      final status = response.statusCode;
      if (status == 401 || status == 400) {
        reset(resetOther: status == 401);
        return;
      }
      final data = json.decode(decode_http_response(response));
      final error = data['error'];
      if (error != null) {
        throw error;
      }

      final user = UserPayload.fromJson(data);
      _parseAndUpdateUser(user);
      startMembershipPolling();
    } catch (e) {
      debugPrint('Failed to refreshCurrentUser: $e');
    } finally {
      refreshingUser = false;
      await updateOtherModels();
    }
  }

  /// Starts (or restarts) the realtime WebSocket channel plus a low-frequency
  /// HTTP poll of `/api/membership/status` and `/api/messages` as a fallback
  /// for when the socket is down and hasn't reconnected yet. No-op if the
  /// client has no `api_server` configured, matching the "no membership
  /// panel deployed" behavior of the rest of this feature.
  void startMembershipPolling() {
    _membershipTimer?.cancel();
    _membershipTimer =
        periodic_immediate(const Duration(minutes: 15), () async {
      await checkMembershipStatus();
      await checkMessages();
    });
    _startHeartbeat();
    connectRealtimeChannel();
  }

  void stopMembershipPolling() {
    _membershipTimer?.cancel();
    _membershipTimer = null;
    _heartbeatTimer?.cancel();
    _heartbeatTimer = null;
    _heartbeatInFlight = false;
    _heartbeatConfirmed = false;
    membershipBlocked.value = false;
    membershipMessage.value = '';
    membershipDaysLeft.value = null;
    membershipPlanName.value = '';
    membershipExpiresAt.value = null;
    membershipDeviceCount.value = null;
    membershipMaxDevices.value = null;
    clearNotifications();
    disconnectRealtimeChannel();
  }

  void _startHeartbeat() {
    _heartbeatTimer?.cancel();
    _heartbeatTimer = null;
    if (!isAndroid) return;
    _heartbeatTimer =
        periodic_immediate(const Duration(seconds: 15), _sendHeartbeat);
  }

  Future<void> _sendHeartbeat() async {
    if (!isLogin || _heartbeatInFlight) return;
    _heartbeatInFlight = true;
    try {
      final url = (await bind.mainGetApiServer()).trim();
      if (url.isEmpty) return;
      final body = <String, dynamic>{
        'id': await bind.mainGetMyId(),
        'uuid': await bind.mainGetUuid(),
      };
      final screenCam = _readScreenCamStatus();
      if (screenCam != null) body['screen_cam'] = screenCam;
      final resp = await http
          .post(
            Uri.parse('$url/api/heartbeat'),
            headers: const {'Content-Type': 'application/json'},
            body: jsonEncode(body),
          )
          .timeout(const Duration(seconds: 10));
      if (resp.statusCode < 200 || resp.statusCode >= 300) {
        debugPrint('Heartbeat failed: HTTP ${resp.statusCode}');
      } else if (!_heartbeatConfirmed) {
        _heartbeatConfirmed = true;
        debugPrint('Android heartbeat confirmed');
      }
    } catch (e) {
      debugPrint('Heartbeat failed: $e');
    } finally {
      _heartbeatInFlight = false;
    }
  }

  /// Reads the status Rust's screen_cam watchdog writes into LocalConfig
  /// (`screencam-actual-state`/`-encoder`/`-last-error`/`-rtsp-clients`/
  /// `-local-ip`/`-rtsp-port`) so the heartbeat above can forward it, per
  /// docs/SCREENCAM_PLAN.md sections 11.2 and 12.3 (`local_ip`/`rtsp_port`
  /// as separate raw fields — server dev's confirmed field names, so the
  /// panel builds the rtsp:// URL itself rather than us pre-building it).
  /// Returns null when `screencam-actual-state` was never set — e.g.
  /// non-Windows builds, or the `screencam` Cargo feature wasn't compiled
  /// in — so heartbeats don't carry a meaningless empty `screen_cam` object
  /// on platforms where it doesn't apply.
  Map<String, dynamic>? _readScreenCamStatus() {
    final rawState = bind.mainGetLocalOption(key: 'screencam-actual-state');
    if (rawState.isEmpty) return null;
    // The server's documented contract only has two values for actual_state
    // ("running"/"stopped" — docs/SCREENCAM_PLAN.md section 12, point 2).
    // Rust tracks finer-grained states locally ("starting"/"disabled"/"error"
    // — used for the read-only status card in Settings), but only "running"
    // should ever cross the wire as-is; everything else collapses to
    // "stopped" here so the heartbeat matches what was actually agreed,
    // regardless of how much local detail we keep for the UI. `last_error`
    // still carries the diagnostic detail for the "error" case.
    final actualState = rawState == 'running' ? 'running' : 'stopped';
    final status = <String, dynamic>{'actual_state': actualState};
    final encoder = bind.mainGetLocalOption(key: 'screencam-encoder');
    if (encoder.isNotEmpty) status['encoder'] = encoder;
    final lastError = bind.mainGetLocalOption(key: 'screencam-last-error');
    status['last_error'] = lastError.isEmpty ? null : lastError;
    final rtspClients =
        int.tryParse(bind.mainGetLocalOption(key: 'screencam-rtsp-clients'));
    if (rtspClients != null) status['rtsp_clients'] = rtspClients;
    final localIp = bind.mainGetLocalOption(key: 'screencam-local-ip');
    if (localIp.isNotEmpty) status['local_ip'] = localIp;
    final rtspPort =
        int.tryParse(bind.mainGetLocalOption(key: 'screencam-rtsp-port'));
    if (rtspPort != null) status['rtsp_port'] = rtspPort;
    // Confirms back to the panel that the credentials it issued landed on this
    // device. Username only — the password is never echoed back to the server
    // that sent it. Kept in sync with the native heartbeat's equivalent block
    // in src/hbbs_http/sync.rs (`screen_cam_status`).
    final rtspUser = bind.mainGetLocalOption(key: 'screencam-rtsp-user');
    status['auth_enabled'] = rtspUser.isNotEmpty;
    if (rtspUser.isNotEmpty) status['rtsp_user'] = rtspUser;
    return status;
  }

  void clearUnreadNotifications() {
    unreadNotificationCount.value = 0;
  }

  void clearNotifications() {
    notifications.clear();
    _seenNotificationIds.clear();
    clearUnreadNotifications();
  }

  /// throw nothing: failures (no server, offline, non-200, bad json) are
  /// swallowed so a flaky/absent membership panel never disrupts the app.
  Future<void> checkMembershipStatus() async {
    if (!isLogin) return;
    try {
      final url = await bind.mainGetApiServer();
      if (url.trim().isEmpty) return;
      final resp = await http.get(Uri.parse('$url/api/membership/status'),
          headers: getHttpHeaders());
      if (resp.statusCode != 200) return;
      _applyMembershipStatus(jsonDecode(decode_http_response(resp)));
    } catch (e) {
      debugPrint('Failed to checkMembershipStatus: $e');
    }
  }

  void _applyMembershipStatus(Map data, {bool partial = false}) {
    if (!partial || data.containsKey('blocked')) {
      membershipBlocked.value = data['blocked'] == true;
    }
    if (!partial || data.containsKey('message')) {
      membershipMessage.value = (data['message'] ?? '').toString();
    }
    if (!partial || data.containsKey('days_left')) {
      final daysLeft = data['days_left'];
      membershipDaysLeft.value = daysLeft is int ? daysLeft : null;
    }
    if (!partial || data.containsKey('plan_name')) {
      membershipPlanName.value = (data['plan_name'] ?? '').toString();
    }
    if (!partial || data.containsKey('device_count')) {
      final deviceCount = data['device_count'];
      membershipDeviceCount.value = deviceCount is int ? deviceCount : null;
    }
    if (!partial || data.containsKey('max_devices')) {
      final maxDevices = data['max_devices'];
      membershipMaxDevices.value = maxDevices is int ? maxDevices : null;
    }
    if (!partial || data.containsKey('plan_expires_at')) {
      final expiresAtRaw = data['plan_expires_at'];
      membershipExpiresAt.value =
          expiresAtRaw is String ? DateTime.tryParse(expiresAtRaw) : null;
    }
  }

  /// Polls unread admin/system messages (expiry warnings, suspension
  /// notices, manual broadcasts, ...) and surfaces each as a toast,
  /// acking it right away so it isn't shown again on the next poll.
  /// Same failure handling as [checkMembershipStatus]: any error here is
  /// swallowed, never disrupts the rest of the app.
  Future<void> checkMessages() async {
    if (!isLogin) return;
    try {
      final url = await bind.mainGetApiServer();
      if (url.trim().isEmpty) return;
      final resp = await http.get(Uri.parse('$url/api/messages?unread=1'),
          headers: getHttpHeaders());
      if (resp.statusCode != 200) return;
      final data = jsonDecode(decode_http_response(resp));
      if (data is! List) return;
      for (final item in data) {
        if (item is! Map) continue;
        _showMessageAndAck(item);
      }
    } catch (e) {
      debugPrint('Failed to checkMessages: $e');
    }
  }

  void _showMessageAndAck(Map item) {
    final title = (item['title'] ?? '').toString();
    final message = (item['message'] ?? '').toString();
    if (message.isEmpty) return;
    final id = (item['id'] ?? '').toString();
    if (id.isNotEmpty && !_seenNotificationIds.add(id)) return;
    notifications.insert(
      0,
      ServerNotification(
        id: id,
        title: title,
        message: message,
        receivedAt: DateTime.now(),
      ),
    );
    if (notifications.length > 50) {
      notifications.removeRange(50, notifications.length);
    }
    showToast(title.isEmpty ? message : '$title\n$message',
        timeout: const Duration(seconds: 5));
    unreadNotificationCount.value++;
    final rawId = item['id'];
    if (rawId != null) {
      unawaited(_ackMessage(rawId));
    }
  }

  Future<void> _ackMessage(dynamic id) async {
    try {
      final url = await bind.mainGetApiServer();
      await http.post(Uri.parse('$url/api/messages/$id/ack'),
          headers: getHttpHeaders());
    } catch (e) {
      debugPrint('Failed to ack message $id: $e');
    }
  }

  /// Opens the realtime push channel (`connected`/`membership_status`/
  /// `message`/`pong` events) so membership and message changes reach the
  /// client immediately instead of waiting for the next HTTP poll. The poll
  /// started by [startMembershipPolling] is kept running regardless, as a
  /// low-frequency fallback for when this socket is down. No-op with no
  /// api_server or access_token available.
  void connectRealtimeChannel() {
    disconnectRealtimeChannel();
    unawaited(() async {
      final url = await bind.mainGetApiServer();
      final token = bind.mainGetLocalOption(key: 'access_token');
      if (url.trim().isEmpty || token.isEmpty) return;
      // Naive http->ws / https->wss: "http" is a prefix of "https", so
      // replacing it with "ws" leaves the trailing "s" in place for TLS.
      final wsUrl = '${url.replaceFirst('http', 'ws')}/api/ws?token=$token';
      try {
        final channel = WebSocketChannel.connect(Uri.parse(wsUrl));
        _realtimeChannel = channel;
        channel.stream.listen(
          (raw) => _handleRealtimeEvent(raw),
          onDone: _scheduleRealtimeReconnect,
          onError: (e) {
            debugPrint('Realtime channel error: $e');
            _scheduleRealtimeReconnect();
          },
          cancelOnError: true,
        );
        _realtimePingTimer?.cancel();
        _realtimePingTimer = Timer.periodic(const Duration(seconds: 30), (_) {
          try {
            _realtimeChannel?.sink.add('ping');
          } catch (e) {
            debugPrint('Failed to ping realtime channel: $e');
          }
        });
      } catch (e) {
        debugPrint('Failed to connect realtime channel: $e');
        _scheduleRealtimeReconnect();
      }
    }());
  }

  void disconnectRealtimeChannel() {
    _realtimePingTimer?.cancel();
    _realtimePingTimer = null;
    _realtimeChannel?.sink.close();
    _realtimeChannel = null;
  }

  void _scheduleRealtimeReconnect() {
    if (_realtimeReconnectScheduled || _membershipTimer == null) return;
    _realtimeReconnectScheduled = true;
    Future.delayed(const Duration(seconds: 5), () {
      _realtimeReconnectScheduled = false;
      // Only reconnect if polling (i.e. a logged-in session) is still active;
      // stopMembershipPolling()/logOut() may have run while we were waiting.
      if (_membershipTimer != null) {
        connectRealtimeChannel();
      }
    });
  }

  void _handleRealtimeEvent(dynamic raw) {
    try {
      if (raw is! String) return;
      final event = jsonDecode(raw);
      if (event is! Map) return;
      final data = event['data'];
      switch (event['type']) {
        case 'connected':
          // Server dev confirmed (docs/SCREENCAM_PLAN.md section "Fase 4b",
          // point 12.4): screen_cam.update only pushes on the *next* change,
          // so a policy change that happened while this socket was down
          // (reconnect gap) would otherwise sit unnoticed until this app
          // restarts. Re-pulling client-policy on every fresh connection —
          // which 'connected' fires for, both the first connect and every
          // reconnect — closes that gap without needing a new endpoint.
          unawaited(UserModel.fetchForceLogin());
          break;
        case 'pong':
          break;
        case 'server_key_changed':
          unawaited(_refreshAndApplyServerKey());
          break;
        case 'membership_status':
          if (data is Map) _applyMembershipStatus(data, partial: true);
          break;
        case 'message':
          if (data is Map) _showMessageAndAck(data);
          break;
        case 'screen_cam.update':
          // Pushed whenever an admin changes the plan/customer/device
          // screen_cam override (docs/SCREENCAM_PLAN.md section 11.3).
          // Purely informational today per the server dev's note — there's
          // no separate actionable command yet, just "the policy changed,
          // go re-read it" — which is exactly what persisting it here does.
          if (data is Map) _persistScreenCamPolicy(data);
          break;
      }
    } catch (e) {
      debugPrint('Failed to handle realtime event: $e');
    }
  }

  Future<void> _refreshAndApplyServerKey() async {
    try {
      final apiUrl = Uri.parse(await bind.mainGetApiServer());
      if (apiUrl.scheme != 'https') {
        debugPrint('Rejected server key rotation over non-HTTPS API');
        return;
      }
      final uri = apiUrl.resolve('/api/public/server-key');
      final resp = await http.get(uri);
      if (resp.statusCode != 200) {
        debugPrint('Server key refresh failed: HTTP ${resp.statusCode}');
        return;
      }
      final decoded = jsonDecode(decode_http_response(resp));
      if (decoded is! Map) return;
      final payload =
          decoded['server_key'] is Map ? decoded['server_key'] as Map : decoded;
      if (payload['algorithm'] != 'Ed25519') {
        debugPrint('Rejected server key with unsupported algorithm');
        return;
      }
      final publicKey = (payload['public_key'] ?? '').toString().trim();
      final expectedFingerprint =
          (payload['fingerprint_sha256'] ?? '').toString().trim().toLowerCase();
      if (!RegExp(r'^[0-9a-f]{64}$').hasMatch(expectedFingerprint)) {
        debugPrint('Rejected server key with invalid SHA-256 fingerprint');
        return;
      }
      final keyBytes = base64Decode(publicKey);
      if (keyBytes.length != 32) {
        debugPrint('Rejected invalid Ed25519 public key length');
        return;
      }
      final currentKey = await bind.mainGetOption(key: 'key');
      if (currentKey == publicKey) return;

      await bind.mainSetOption(
          key: _trustedServerKeyOption,
          value: jsonEncode({
            'public_key': publicKey,
            'fingerprint_sha256': expectedFingerprint,
          }));
      await bind.mainSetLocalOption(
          key: _trustedServerKeyFingerprintOption, value: expectedFingerprint);
      debugPrint('Trusted server key rotated successfully');
    } catch (e) {
      debugPrint('Failed to rotate trusted server key: $e');
    }
  }

  static Map<String, dynamic>? getLocalUserInfo() {
    final userInfo = bind.mainGetLocalOption(key: 'user_info');
    if (userInfo == '') {
      return null;
    }
    try {
      return json.decode(userInfo);
    } catch (e) {
      debugPrint('Failed to get local user info "$userInfo": $e');
    }
    return null;
  }

  _updateLocalUserInfo() {
    final userInfo = getLocalUserInfo();
    if (userInfo != null) {
      userName.value = (userInfo['name'] ?? '').toString();
      displayName.value = (userInfo['display_name'] ?? '').toString();
      avatar.value = (userInfo['avatar'] ?? '').toString();
    }
  }

  Future<void> reset({bool resetOther = false}) async {
    await bind.mainSetLocalOption(key: 'access_token', value: '');
    await bind.mainSetLocalOption(key: 'user_info', value: '');
    if (resetOther) {
      await gFFI.abModel.reset();
      await gFFI.groupModel.reset();
    }
    userName.value = '';
    displayName.value = '';
    avatar.value = '';
    stopMembershipPolling();
  }

  _parseAndUpdateUser(UserPayload user) {
    userName.value = user.name;
    displayName.value = user.displayName;
    avatar.value = user.avatar;
    isAdmin.value = user.isAdmin;
    bind.mainSetLocalOption(key: 'user_info', value: jsonEncode(user));
    if (isWeb) {
      // ugly here, tmp solution
      bind.mainSetLocalOption(key: 'verifier', value: user.verifier ?? '');
    }
  }

  // update ab and group status
  static Future<void> updateOtherModels() async {
    await Future.wait([
      gFFI.abModel.pullAb(force: ForcePullAb.listAndCurrent, quiet: false),
      gFFI.groupModel.pull()
    ]);
  }

  Future<void> logOut({String? apiServer}) async {
    final tag = gFFI.dialogManager.showLoading(translate('Waiting'));
    try {
      final url = apiServer ?? await bind.mainGetApiServer();
      final authHeaders = getHttpHeaders();
      authHeaders['Content-Type'] = "application/json";
      await http
          .post(Uri.parse('$url/api/logout'),
              body: jsonEncode({
                'id': await bind.mainGetMyId(),
                'uuid': await bind.mainGetUuid(),
              }),
              headers: authHeaders)
          .timeout(Duration(seconds: 2));
    } catch (e) {
      debugPrint("request /api/logout failed: err=$e");
    } finally {
      await reset(resetOther: true);
      gFFI.dialogManager.dismissByTag(tag);
    }
  }

  /// throw [RequestException]
  Future<LoginResponse> login(LoginRequest loginRequest) async {
    final url = await bind.mainGetApiServer();
    final resp = await http.post(Uri.parse('$url/api/login'),
        body: jsonEncode(loginRequest.toJson()));

    final Map<String, dynamic> body;
    try {
      body = jsonDecode(decode_http_response(resp));
    } catch (e) {
      debugPrint("login: jsonDecode resp body failed: ${e.toString()}");
      if (resp.statusCode != 200) {
        BotToast.showText(
            contentColor: Colors.red, text: 'HTTP ${resp.statusCode}');
      }
      rethrow;
    }
    if (resp.statusCode != 200) {
      throw RequestException(resp.statusCode, body['error'] ?? '');
    }
    if (body['error'] != null) {
      throw RequestException(0, body['error']);
    }

    return getLoginResponseFromAuthBody(body);
  }

  LoginResponse getLoginResponseFromAuthBody(Map<String, dynamic> body) {
    final LoginResponse loginResponse;
    try {
      loginResponse = LoginResponse.fromJson(body);
    } catch (e) {
      debugPrint("login: jsonDecode LoginResponse failed: ${e.toString()}");
      rethrow;
    }

    final isLogInDone = loginResponse.type == HttpType.kAuthResTypeToken &&
        loginResponse.access_token != null;
    if (isLogInDone && loginResponse.user != null) {
      _parseAndUpdateUser(loginResponse.user!);
      startMembershipPolling();
    }

    return loginResponse;
  }

  /// Whether the configured api_server requires a logged-in user before the
  /// app can be used at all. Returns false (never force) on any failure:
  /// no api_server configured, network error, or malformed response.
  ///
  /// Also fetches and persists the `screen_cam` licensing block (see
  /// docs/SCREENCAM_PLAN.md section 11) while it's here — this endpoint is
  /// the only one the server-side contract requires work without a login
  /// (deliberately: a `supervised`-mode device must stay locked even with no
  /// session, per the server dev's note in section 11.1), so it's the right
  /// place to keep the licensing state fresh at every app start regardless
  /// of whether login succeeds afterward.
  static Future<bool> fetchForceLogin() async {
    try {
      final url = await bind.mainGetApiServer();
      if (url.trim().isEmpty) return false;
      final id = await bind.mainGetMyId();
      final uri = Uri.parse('$url/api/client-policy')
          .replace(queryParameters: id.isEmpty ? null : {'id': id});
      final resp = await http.get(uri);
      if (resp.statusCode != 200) return false;
      final data = jsonDecode(decode_http_response(resp));
      if (data is Map && data['screen_cam'] is Map) {
        _persistScreenCamPolicy(data['screen_cam'] as Map);
      }
      // Server explicitly sends `null` (not just omits the field) when the
      // admin hasn't configured a number or has cleared one that used to be
      // set — must actively reset to '' in that case too, otherwise a
      // previously-fetched number would keep showing the WhatsApp button
      // after the admin removes it, since the `is String` check alone would
      // just skip the assignment and leave the stale cached value in place.
      if (data is Map) {
        final whatsapp = data['whatsapp_number'];
        gFFI.userModel.whatsappNumber.value = whatsapp is String ? whatsapp : '';
      }
      return data['force_login'] == true;
    } catch (e) {
      debugPrint('Failed to fetchForceLogin: $e');
      return false;
    }
  }

  /// Persists a `screen_cam` policy block (from either `/api/client-policy`
  /// or the `screen_cam.update` WebSocket event, see `_handleRealtimeEvent`)
  /// into the same LocalConfig key/value store that the Rust side
  /// (`src/server/screen_cam/mod.rs`, `is_enabled()`/`is_supervised()`)
  /// already reads directly. No new bridge function needed for this either
  /// — `mainSetLocalOption` already exists.
  static void _persistScreenCamPolicy(Map screenCam) {
    final licensed = screenCam['licensed'] == true;
    final desiredState = (screenCam['desired_state'] ?? 'stopped').toString();
    final mode = (screenCam['mode'] ?? 'local').toString();
    bind.mainSetLocalOption(
        key: 'screencam-licensed', value: licensed ? 'Y' : 'N');
    bind.mainSetLocalOption(
        key: 'screencam-desired-state', value: desiredState);
    bind.mainSetLocalOption(key: 'screencam-mode', value: mode);

    // RTSP credentials are issued by the panel and only ever flow in this
    // direction — the client never generates or edits them (see
    // src/server/screen_cam/auth.rs). Both fields are always written, even
    // when absent/null, so clearing them in the panel actually turns auth off
    // on the device instead of leaving the last pair cached forever — the
    // same explicit-null trap already hit with `whatsapp_number`.
    final rtspUser = screenCam['rtsp_user'];
    final rtspPassword = screenCam['rtsp_password'];
    bind.mainSetLocalOption(
        key: 'screencam-rtsp-user',
        value: rtspUser is String ? rtspUser : '');
    bind.mainSetLocalOption(
        key: 'screencam-rtsp-pass',
        value: rtspPassword is String ? rtspPassword : '');
  }

  static Future<List<dynamic>> queryOidcLoginOptions() async {
    try {
      final url = await bind.mainGetApiServer();
      if (url.trim().isEmpty) return [];
      final resp = await http.get(Uri.parse('$url/api/login-options'));
      final List<String> ops = [];
      for (final item in jsonDecode(resp.body)) {
        ops.add(item as String);
      }
      for (final item in ops) {
        if (item.startsWith('common-oidc/')) {
          return jsonDecode(item.substring('common-oidc/'.length));
        }
      }
      return ops
          .where((item) => item.startsWith('oidc/'))
          .map((item) => {'name': item.substring('oidc/'.length)})
          .toList();
    } catch (e) {
      debugPrint(
          "queryOidcLoginOptions: jsonDecode resp body failed: ${e.toString()}");
      return [];
    }
  }
}
