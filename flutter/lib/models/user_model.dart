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
      final resp = await http
          .post(
            Uri.parse('$url/api/heartbeat'),
            headers: const {'Content-Type': 'application/json'},
            body: jsonEncode({
              'id': await bind.mainGetMyId(),
              'uuid': await bind.mainGetUuid(),
            }),
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
  static Future<bool> fetchForceLogin() async {
    try {
      final url = await bind.mainGetApiServer();
      if (url.trim().isEmpty) return false;
      final resp = await http.get(Uri.parse('$url/api/client-policy'));
      if (resp.statusCode != 200) return false;
      final data = jsonDecode(decode_http_response(resp));
      return data['force_login'] == true;
    } catch (e) {
      debugPrint('Failed to fetchForceLogin: $e');
      return false;
    }
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
