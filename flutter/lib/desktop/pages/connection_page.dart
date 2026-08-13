// main window right pane

import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/material.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/desktop/widgets/popup_menu.dart';
import 'package:flutter_hbb/desktop/widgets/update_progress.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:get/get.dart';
import 'package:url_launcher/url_launcher_string.dart';
import 'package:window_manager/window_manager.dart';
import 'package:flutter_hbb/models/peer_model.dart';
import 'package:flutter_hbb/utils/multi_window_manager.dart';

import '../../common.dart';
import '../../common/formatter/id_formatter.dart';
import '../../common/widgets/peer_tab_page.dart';
import '../../common/widgets/autocomplete.dart';
import '../../models/platform_model.dart';
import '../../desktop/widgets/material_mod_popup_menu.dart' as mod_menu;

class OnlineStatusWidget extends StatefulWidget {
  const OnlineStatusWidget({Key? key, this.onSvcStatusChanged})
      : super(key: key);

  final VoidCallback? onSvcStatusChanged;

  @override
  State<OnlineStatusWidget> createState() => _OnlineStatusWidgetState();
}

/// State for the connection page.
class _OnlineStatusWidgetState extends State<OnlineStatusWidget> {
  final _svcStopped = Get.find<RxBool>(tag: 'stop-service');
  final _svcIsUsingPublicServer = true.obs;
  Timer? _updateTimer;

  double get em => 14.0;
  double? get height => bind.isIncomingOnly() ? null : em * 3;

  void onUsePublicServerGuide() {
    const url = "https://rustdesk.com/pricing";
    canLaunchUrlString(url).then((can) {
      if (can) {
        launchUrlString(url);
      }
    });
  }

  @override
  void initState() {
    super.initState();
    _updateTimer = periodic_immediate(Duration(seconds: 1), () async {
      updateStatus();
    });
  }

  @override
  void dispose() {
    _updateTimer?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final isIncomingOnly = bind.isIncomingOnly();
    startServiceWidget() => Offstage(
          offstage: !_svcStopped.value,
          child: InkWell(
                  onTap: () async {
                    await start_service(true);
                  },
                  child: Text(translate("Start service"),
                      style: TextStyle(
                          decoration: TextDecoration.underline, fontSize: em)))
              .marginOnly(left: em),
        );

    const bool HIDE_PUBLIC_SERVER_TIP = true;

    Widget setupServerWidget() {
      if (HIDE_PUBLIC_SERVER_TIP) return const SizedBox.shrink();

      return Flexible(
        child: Offstage(
          offstage: !(!_svcStopped.value &&
              stateGlobal.svcStatus.value == SvcStatus.ready &&
              _svcIsUsingPublicServer.value),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.center,
            children: [
              Text(', ', style: TextStyle(fontSize: em)),
              Flexible(
                child: InkWell(
                  onTap: onUsePublicServerGuide,
                  child: Row(
                    children: [
                      Flexible(
                        child: Text(
                          translate('setup_server_tip'),
                          style: TextStyle(
                            decoration: TextDecoration.underline,
                            fontSize: em,
                          ),
                        ),
                      ),
                    ],
                  ),
                ),
              )
            ],
          ),
        ),
      );
    }

    basicWidget(BoxConstraints constraints) {
      final showAcceptLabel = constraints.maxWidth >= 560;
      return Row(
        crossAxisAlignment: CrossAxisAlignment.center,
        children: [
          Container(
            height: 8,
            width: 8,
            decoration: BoxDecoration(
              borderRadius: BorderRadius.circular(4),
              color: _svcStopped.value ||
                      stateGlobal.svcStatus.value == SvcStatus.connecting
                  ? kColorWarn
                  : (stateGlobal.svcStatus.value == SvcStatus.ready
                      ? Color.fromARGB(255, 50, 190, 166)
                      : Color.fromARGB(255, 224, 79, 95)),
            ),
          ).marginSymmetric(horizontal: em),
          if (isIncomingOnly)
            SizedBox(width: 226, child: _buildConnStatusMsg())
          else
            Expanded(child: _buildConnStatusMsg()),
          if (!isIncomingOnly) startServiceWidget(),
          if (!isIncomingOnly) setupServerWidget(),
          if (!isIncomingOnly) ...[
            Container(
              width: 1,
              height: 28,
              margin: const EdgeInsets.symmetric(horizontal: 14),
              color: Theme.of(context).dividerColor.withOpacity(0.55),
            ),
            if (showAcceptLabel)
              Flexible(
                child: Text(
                  translate('Accept sessions via password'),
                  maxLines: 2,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(
                    fontSize: 12.5,
                    fontWeight: FontWeight.w600,
                  ),
                ),
              ),
            Tooltip(
              message: translate('Accept sessions via password'),
              child: Switch(
                value: !_svcStopped.value,
                activeColor: const Color(0xFF10A83A),
                onChanged: (value) async => await start_service(value),
              ),
            ),
          ],
        ],
      );
    }

    return SizedBox(
      height: height,
      child: LayoutBuilder(
        builder: (context, constraints) => Obx(
          () => isIncomingOnly
              ? Column(
                  children: [
                    basicWidget(constraints),
                    Align(
                            child: startServiceWidget(),
                            alignment: Alignment.centerLeft)
                        .marginOnly(top: 2.0, left: 22.0),
                  ],
                )
              : basicWidget(constraints),
        ),
      ),
    ).paddingOnly(right: isIncomingOnly ? 8 : 0);
  }

  _buildConnStatusMsg() {
    widget.onSvcStatusChanged?.call();
    final status = _svcStopped.value
        ? translate("Service is not running")
        : stateGlobal.svcStatus.value == SvcStatus.connecting
            ? translate("connecting_status")
            : stateGlobal.svcStatus.value == SvcStatus.notReady
                ? translate("not_ready_status")
                : translate('Ready');
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          status,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(fontSize: em),
        ),
        if (!bind.isIncomingOnly() &&
            !_svcStopped.value &&
            stateGlobal.svcStatus.value == SvcStatus.ready)
          Text(
            'Todo funcionando correctamente',
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: Theme.of(context).textTheme.bodySmall,
          ),
      ],
    );
  }

  updateStatus() async {
    final status =
        jsonDecode(await bind.mainGetConnectStatus()) as Map<String, dynamic>;
    final statusNum = status['status_num'] as int;
    if (statusNum == 0) {
      stateGlobal.svcStatus.value = SvcStatus.connecting;
    } else if (statusNum == -1) {
      stateGlobal.svcStatus.value = SvcStatus.notReady;
    } else if (statusNum == 1) {
      stateGlobal.svcStatus.value = SvcStatus.ready;
    } else {
      stateGlobal.svcStatus.value = SvcStatus.notReady;
    }
    _svcIsUsingPublicServer.value = await bind.mainIsUsingPublicServer();
    try {
      stateGlobal.videoConnCount.value = status['video_conn_count'] as int;
    } catch (_) {}
  }
}

/// Connection page for connecting to a remote peer.
class ConnectionPage extends StatefulWidget {
  const ConnectionPage({Key? key}) : super(key: key);

  @override
  State<ConnectionPage> createState() => _ConnectionPageState();
}

/// State for the connection page.
class _ConnectionPageState extends State<ConnectionPage>
    with SingleTickerProviderStateMixin, WindowListener {
  static const _sehBlue = Color(0xFF176B87);

  /// Controller for the id input bar.
  final _idController = IDTextEditingController();

  final RxBool _idInputFocused = false.obs;
  final FocusNode _idFocusNode = FocusNode();
  final TextEditingController _idEditingController = TextEditingController();

  /// Drives the pulsing glow on the "install at system level" nudge so it
  /// isn't easy to miss.
  late final AnimationController _installTipPulseController =
      AnimationController(
          vsync: this, duration: const Duration(milliseconds: 650))
        ..repeat(reverse: true);

  String selectedConnectionType = 'Connect';

  bool isWindowMinimized = false;

  final AllPeersLoader _allPeersLoader = AllPeersLoader();

  // https://github.com/flutter/flutter/issues/157244
  Iterable<Peer> _autocompleteOpts = [];

  final _menuOpen = false.obs;

  @override
  void initState() {
    super.initState();
    _allPeersLoader.init(setState);
    _idFocusNode.addListener(onFocusChanged);
    if (_idController.text.isEmpty) {
      WidgetsBinding.instance.addPostFrameCallback((_) async {
        final lastRemoteId = await bind.mainGetLastRemoteId();
        if (lastRemoteId != _idController.id) {
          setState(() {
            _idController.id = lastRemoteId;
          });
        }
      });
    }
    Get.put<TextEditingController>(_idEditingController);
    Get.put<IDTextEditingController>(_idController);
    windowManager.addListener(this);
  }

  @override
  void dispose() {
    _installTipPulseController.dispose();
    _idController.dispose();
    windowManager.removeListener(this);
    _allPeersLoader.clear();
    _idFocusNode.removeListener(onFocusChanged);
    _idFocusNode.dispose();
    _idEditingController.dispose();
    if (Get.isRegistered<IDTextEditingController>()) {
      Get.delete<IDTextEditingController>();
    }
    if (Get.isRegistered<TextEditingController>()) {
      Get.delete<TextEditingController>();
    }
    super.dispose();
  }

  @override
  void onWindowEvent(String eventName) {
    super.onWindowEvent(eventName);
    if (eventName == 'minimize') {
      isWindowMinimized = true;
    } else if (eventName == 'maximize' || eventName == 'restore') {
      if (isWindowMinimized && isWindows) {
        // windows can't update when minimized.
        Get.forceAppUpdate();
      }
      isWindowMinimized = false;
    }
  }

  @override
  void onWindowEnterFullScreen() {
    // Remove edge border by setting the value to zero.
    stateGlobal.resizeEdgeSize.value = 0;
  }

  @override
  void onWindowLeaveFullScreen() {
    // Restore edge border to default edge size.
    stateGlobal.resizeEdgeSize.value = stateGlobal.isMaximized.isTrue
        ? kMaximizeEdgeSize
        : windowResizeEdgeSize;
  }

  @override
  void onWindowClose() {
    super.onWindowClose();
    bind.mainOnMainWindowClose();
  }

  void onFocusChanged() {
    _idInputFocused.value = _idFocusNode.hasFocus;
    if (_idFocusNode.hasFocus) {
      if (_allPeersLoader.needLoad) {
        _allPeersLoader.getAllPeers();
      }

      final textLength = _idEditingController.value.text.length;
      // Select all to facilitate removing text, just following the behavior of address input of chrome.
      _idEditingController.selection =
          TextSelection(baseOffset: 0, extentOffset: textLength);
    }
  }

  @override
  Widget build(BuildContext context) {
    final isOutgoingOnly = bind.isOutgoingOnly();
    final installTipCard = _buildInstallTipCard(context);
    final compact = MediaQuery.of(context).size.height < 820;
    return Column(
      children: [
        Expanded(
            child: Column(children: [
          _buildConnectionHero(context, installTipCard),
          _buildDevicesPanel(context),
        ])),
        if (!isOutgoingOnly)
          Container(
            height: compact ? 56 : 68,
            margin: EdgeInsets.fromLTRB(24, 0, 24, compact ? 10 : 18),
            padding: const EdgeInsets.symmetric(horizontal: 4),
            decoration: BoxDecoration(
              color: Theme.of(context).colorScheme.surface,
              borderRadius: BorderRadius.circular(14),
              border: Border.all(color: Theme.of(context).dividerColor),
            ),
            child: Row(
              children: [
                const Expanded(child: OnlineStatusWidget()),
                Icon(Icons.monitor_heart_outlined,
                    color: Colors.green.withOpacity(0.8), size: 38),
                const SizedBox(width: 18),
              ],
            ),
          )
      ],
    );
  }

  Widget _buildDevicesPanel(BuildContext context) {
    final isDark = Theme.of(context).brightness == Brightness.dark;
    final compact = MediaQuery.of(context).size.height < 820;
    final panelBackground =
        isDark ? const Color(0xFF07131D) : const Color(0xFFFFFFFF);
    final panelCard =
        isDark ? const Color(0xFF111E31) : const Color(0xFFFFFFFF);
    final panelTheme = Theme.of(context).copyWith(
      scaffoldBackgroundColor: panelBackground,
      canvasColor: panelBackground,
      cardColor: panelCard,
      colorScheme: Theme.of(context).colorScheme.copyWith(
            surface: panelBackground,
            surfaceContainerHighest: panelCard,
          ),
    );

    return Expanded(
      child: Container(
        margin:
            EdgeInsets.fromLTRB(24, compact ? 12 : 20, 24, compact ? 10 : 18),
        clipBehavior: Clip.antiAlias,
        decoration: BoxDecoration(
          color: panelBackground,
          borderRadius: BorderRadius.circular(14),
          border: Border.all(
              color:
                  isDark ? const Color(0xFF1A2A35) : const Color(0xFFE4E8EE)),
          boxShadow: isDark
              ? null
              : const [
                  BoxShadow(
                      color: Color(0x10000000),
                      blurRadius: 14,
                      offset: Offset(0, 4))
                ],
        ),
        child: Theme(
          data: panelTheme,
          child: ColoredBox(
            color: Colors.transparent,
            child: Column(
              children: [
                Padding(
                  padding: EdgeInsets.fromLTRB(
                      20, compact ? 8 : 16, 14, compact ? 8 : 12),
                  child: Row(
                    children: [
                      Container(
                        width: 36,
                        height: 36,
                        decoration: BoxDecoration(
                          color: const Color(0xFF0AA83B),
                          borderRadius: BorderRadius.circular(8),
                        ),
                        child: const Icon(Icons.desktop_windows_outlined,
                            color: Colors.white, size: 21),
                      ),
                      const SizedBox(width: 12),
                      Text(
                        translate('My Devices').toUpperCase(),
                        style: TextStyle(
                          color: Theme.of(context).colorScheme.onSurface,
                          fontSize: 15,
                          fontWeight: FontWeight.w800,
                        ),
                      ),
                      const SizedBox(width: 20),
                      const PeerSearchBar(
                        expanded: true,
                        width: 260,
                      ),
                    ],
                  ),
                ),
                Divider(height: 1, color: Theme.of(context).dividerColor),
                Expanded(
                  child: ColoredBox(
                    color: panelBackground,
                    child: const PeerTabPage(showSearchAction: false),
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }

  Widget _buildConnectionHero(BuildContext context, Widget? installTipCard) {
    final compact = MediaQuery.of(context).size.height < 820;
    return Padding(
      padding: EdgeInsets.fromLTRB(24, compact ? 12 : 18, 24, 0),
      child: Obx(
        () {
          final daysLeft = gFFI.userModel.membershipDaysLeft.value;
          final hasMembershipNotice = gFFI.userModel.membershipBlocked.value ||
              (daysLeft != null && daysLeft <= 7);
          final hasUpdateNotice = stateGlobal.updateUrl.value.isNotEmpty ||
              bind.mainIsInstalledLowerVersion();
          final hasGeneralNotice =
              gFFI.userModel.unreadNotificationCount.value > 0 &&
                  gFFI.userModel.notifications.isNotEmpty;
          final hasNotice = installTipCard != null ||
              hasMembershipNotice ||
              hasUpdateNotice ||
              hasGeneralNotice;
          final isDark = Theme.of(context).brightness == Brightness.dark;

          return Container(
            height: compact ? 278 : 338,
            padding: EdgeInsets.all(compact ? 16 : 24),
            decoration: BoxDecoration(
              color: Theme.of(context).colorScheme.surface,
              borderRadius: BorderRadius.circular(14),
              border: Border.all(
                  color: isDark
                      ? const Color(0xFF1A2A35)
                      : const Color(0xFFE4E8EE)),
              boxShadow: isDark
                  ? null
                  : const [
                      BoxShadow(
                          color: Color(0x10000000),
                          blurRadius: 14,
                          offset: Offset(0, 4))
                    ],
            ),
            child: LayoutBuilder(
              builder: (context, constraints) {
                final left = _buildQuickConnectionCard(context);
                final right = hasNotice
                    ? _buildPriorityNotices(
                        context,
                        installTipCard: installTipCard,
                        showMembership: hasMembershipNotice,
                        showUpdate: hasUpdateNotice,
                        showGeneral: hasGeneralNotice,
                      )
                    : _buildQuickActionsGrid(context);
                if (constraints.maxWidth < 820) {
                  return Column(
                    children: [left, const SizedBox(height: 18), right],
                  );
                }
                return Row(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    Expanded(flex: 4, child: left),
                    SizedBox(width: compact ? 18 : 30),
                    Expanded(flex: 5, child: right),
                  ],
                );
              },
            ),
          );
        },
      ),
    );
  }

  Widget _buildQuickConnectionCard(BuildContext context) {
    final isDark = Theme.of(context).brightness == Brightness.dark;
    final compact = MediaQuery.of(context).size.height < 820;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        Row(
          children: [
            Container(
              width: 42,
              height: 42,
              decoration: BoxDecoration(
                color:
                    isDark ? const Color(0xFF101E2A) : const Color(0xFFF5F7FA),
                borderRadius: BorderRadius.circular(9),
                border: Border.all(color: Theme.of(context).dividerColor),
              ),
              child: Icon(Icons.link_rounded,
                  color: Theme.of(context).colorScheme.primary),
            ),
            const SizedBox(width: 12),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(translate('Control Remote Desktop').toUpperCase(),
                      style: const TextStyle(
                          fontSize: 15, fontWeight: FontWeight.w800)),
                  const SizedBox(height: 3),
                  Text(translate('desk_tip'),
                      style: TextStyle(
                          color: Theme.of(context).colorScheme.onSurfaceVariant,
                          fontSize: 11)),
                ],
              ),
            ),
          ],
        ),
        SizedBox(height: compact ? 12 : 24),
        _buildRemoteIDTextField(context),
      ],
    );
  }

  Widget _buildQuickActionsGrid(BuildContext context) {
    final compact = MediaQuery.of(context).size.height < 820;
    return Column(
      children: [
        Expanded(
          child: Row(
            children: [
              Expanded(
                child: _quickAction(Icons.bolt_rounded, translate('Connect'),
                    const Color(0xFF0877F9), () => onConnect()),
              ),
              SizedBox(width: compact ? 12 : 18),
              Expanded(
                child: _quickAction(
                    Icons.folder_open_rounded,
                    translate('Transfer file'),
                    const Color(0xFF0BA43B),
                    () => onConnect(isFileTransfer: true)),
              ),
            ],
          ),
        ),
        SizedBox(height: compact ? 12 : 18),
        Expanded(
          child: Row(
            children: [
              Expanded(
                child: _quickAction(
                    Icons.videocam_outlined,
                    translate('View camera'),
                    const Color(0xFF0877F9),
                    () => onConnect(isViewCamera: true)),
              ),
              SizedBox(width: compact ? 12 : 18),
              Expanded(
                child: _quickAction(
                    Icons.terminal_rounded,
                    translate('Terminal'),
                    const Color(0xFF0BA43B),
                    () => onConnect(isTerminal: true)),
              ),
            ],
          ),
        ),
      ],
    );
  }

  Widget _quickAction(
      IconData icon, String label, Color color, VoidCallback onTap) {
    final compact = MediaQuery.of(context).size.height < 820;
    return Material(
      color: Colors.transparent,
      borderRadius: BorderRadius.circular(12),
      child: InkWell(
        borderRadius: BorderRadius.circular(12),
        onTap: onTap,
        child: Container(
          constraints: BoxConstraints(minHeight: compact ? 96 : 132),
          padding:
              EdgeInsets.symmetric(horizontal: 12, vertical: compact ? 8 : 16),
          decoration: BoxDecoration(
              color: Theme.of(context).colorScheme.surface,
              borderRadius: BorderRadius.circular(12),
              border: Border.all(color: color),
              boxShadow: [
                BoxShadow(
                    color: color.withOpacity(0.8),
                    blurRadius: 0,
                    offset: const Offset(0, 2))
              ]),
          child: Column(
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              Icon(icon, color: color, size: compact ? 32 : 42),
              SizedBox(height: compact ? 8 : 16),
              Text(label,
                  maxLines: 2,
                  overflow: TextOverflow.ellipsis,
                  textAlign: TextAlign.center,
                  style: const TextStyle(
                      fontSize: 16, fontWeight: FontWeight.w500)),
            ],
          ),
        ),
      ),
    );
  }

  Widget _buildPriorityNotices(
    BuildContext context, {
    required Widget? installTipCard,
    required bool showMembership,
    required bool showUpdate,
    required bool showGeneral,
  }) {
    return SingleChildScrollView(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (showGeneral) _buildGeneralNotice(context),
          if (showGeneral &&
              (showUpdate || showMembership || installTipCard != null))
            const SizedBox(height: 14),
          if (showUpdate) _buildUpdateNotice(context),
          if (showUpdate && (showMembership || installTipCard != null))
            const SizedBox(height: 14),
          if (showMembership) buildMembershipBanner(context),
          if (showMembership && installTipCard != null)
            const SizedBox(height: 14),
          if (installTipCard != null) installTipCard,
        ],
      ),
    );
  }

  Widget _buildGeneralNotice(BuildContext context) {
    final notice = gFFI.userModel.notifications.first;
    return Container(
      padding: const EdgeInsets.all(18),
      decoration: BoxDecoration(
        color: Theme.of(context).colorScheme.surfaceContainerHighest,
        borderRadius: BorderRadius.circular(13),
        border: Border.all(color: const Color(0xFF10A83A)),
      ),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Icon(Icons.campaign_outlined,
              color: Color(0xFF10A83A), size: 26),
          const SizedBox(width: 12),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                if (notice.title.isNotEmpty)
                  Text(notice.title,
                      style: const TextStyle(
                          fontSize: 16, fontWeight: FontWeight.w700)),
                if (notice.title.isNotEmpty) const SizedBox(height: 6),
                Text(notice.message,
                    style: Theme.of(context).textTheme.bodySmall),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildUpdateNotice(BuildContext context) {
    final updateUrl = stateGlobal.updateUrl.value;
    return Container(
      padding: const EdgeInsets.all(18),
      decoration: BoxDecoration(
        color: Theme.of(context).colorScheme.surfaceContainerHighest,
        borderRadius: BorderRadius.circular(13),
        border: Border.all(color: const Color(0xFF0877F9)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              const Icon(Icons.system_update_alt_rounded,
                  color: Color(0xFF0877F9), size: 24),
              const SizedBox(width: 10),
              Text(translate('Update'),
                  style: const TextStyle(
                      fontSize: 16, fontWeight: FontWeight.w700)),
            ],
          ),
          const SizedBox(height: 10),
          Text(
            '${translate("new-version-of-{${bind.mainGetAppNameSync()}}-tip")} (${bind.mainGetNewVersion()}).',
            style: Theme.of(context).textTheme.bodySmall,
          ),
          const SizedBox(height: 14),
          SizedBox(
            width: double.infinity,
            child: ElevatedButton(
              onPressed: updateUrl.isEmpty
                  ? bind.mainUpdateMe
                  : () => handleUpdate(updateUrl),
              child: Text(translate('Update')),
            ),
          ),
        ],
      ),
    );
  }

  /// Callback for the connect button.
  /// Connects to the selected peer.
  void onConnect(
      {bool isFileTransfer = false,
      bool isViewCamera = false,
      bool isTerminal = false,
      bool isViewOnly = false}) {
    var id = _idController.id;
    connect(context, id,
        isFileTransfer: isFileTransfer,
        isViewCamera: isViewCamera,
        isTerminal: isTerminal,
        isViewOnly: isViewOnly);
  }

  /// Windows-only nudge to install at the system level (otherwise User
  /// Account Control can prevent full remote-desktop functionality). Shown
  /// next to the secure-connection card instead of at the bottom of the left
  /// sidebar, since that spot scrolls out of view on short/narrow windows.
  /// Pulses and uses a large filled button so it's hard to miss/ignore.
  Widget? _buildInstallTipCard(BuildContext context) {
    if (!isWindows ||
        bind.isDisableInstallation() ||
        bind.isOutgoingOnly() ||
        bind.mainIsInstalled()) {
      return null;
    }
    const baseColor = Color.fromARGB(255, 226, 66, 188);
    const pulseColor = Color.fromARGB(255, 255, 82, 82);
    return AnimatedBuilder(
      animation: _installTipPulseController,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              const Icon(Icons.warning_amber_rounded,
                  color: pulseColor, size: 22),
              const SizedBox(width: 6),
              Expanded(
                child: Text(translate('Install'),
                    style: const TextStyle(
                        fontWeight: FontWeight.bold, fontSize: 16)),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Text(
            translate('install_tip'),
            style: Theme.of(context).textTheme.bodySmall,
          ),
          const SizedBox(height: 14),
          SizedBox(
            width: double.infinity,
            child: ElevatedButton(
              style: ElevatedButton.styleFrom(
                backgroundColor: pulseColor,
                foregroundColor: Colors.white,
                padding: const EdgeInsets.symmetric(vertical: 16),
                textStyle:
                    const TextStyle(fontWeight: FontWeight.bold, fontSize: 16),
                shape: RoundedRectangleBorder(
                    borderRadius: BorderRadius.circular(10)),
              ),
              onPressed: () async {
                await rustDeskWinManager.closeAllSubWindows();
                bind.mainGotoInstall();
              },
              child: Text(translate('Install')),
            ),
          ),
        ],
      ),
      builder: (context, child) {
        final glow = Color.lerp(
            baseColor, pulseColor, _installTipPulseController.value)!;
        return Container(
          width: 260,
          padding: const EdgeInsets.all(16),
          decoration: BoxDecoration(
            color: Theme.of(context).colorScheme.surfaceContainerHighest,
            borderRadius: BorderRadius.circular(13),
            border: Border.all(color: glow, width: 2),
            boxShadow: [
              BoxShadow(
                color: glow.withOpacity(
                    0.25 + 0.35 * _installTipPulseController.value),
                blurRadius: 14,
                spreadRadius: 1,
              ),
            ],
          ),
          child: child,
        );
      },
    );
  }

  Widget _buildRemoteIDTextField(BuildContext context) {
    var w = Container(
      width: double.infinity,
      padding: const EdgeInsets.fromLTRB(18, 15, 18, 16),
      decoration: BoxDecoration(
          color: Theme.of(context).colorScheme.surface,
          borderRadius: const BorderRadius.all(Radius.circular(16)),
          border: Border.all(color: Theme.of(context).dividerColor)),
      child: Ink(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(translate('Enter Remote ID'),
                    style: Theme.of(context).textTheme.labelMedium?.copyWith(
                        color: _sehBlue, fontWeight: FontWeight.w800))
                .marginOnly(bottom: 7),
            Row(
              children: [
                Expanded(
                    child: RawAutocomplete<Peer>(
                  optionsBuilder: (TextEditingValue textEditingValue) {
                    if (textEditingValue.text == '') {
                      _autocompleteOpts = const Iterable<Peer>.empty();
                    } else if (_allPeersLoader.peers.isEmpty &&
                        !_allPeersLoader.isPeersLoaded) {
                      Peer emptyPeer = Peer(
                        id: '',
                        username: '',
                        hostname: '',
                        alias: '',
                        platform: '',
                        tags: [],
                        hash: '',
                        password: '',
                        forceAlwaysRelay: false,
                        rdpPort: '',
                        rdpUsername: '',
                        loginName: '',
                        device_group_name: '',
                        note: '',
                      );
                      _autocompleteOpts = [emptyPeer];
                    } else {
                      String textWithoutSpaces =
                          textEditingValue.text.replaceAll(" ", "");
                      if (int.tryParse(textWithoutSpaces) != null) {
                        textEditingValue = TextEditingValue(
                          text: textWithoutSpaces,
                          selection: textEditingValue.selection,
                        );
                      }
                      String textToFind = textEditingValue.text.toLowerCase();
                      _autocompleteOpts = _allPeersLoader.peers
                          .where((peer) =>
                              peer.id.toLowerCase().contains(textToFind) ||
                              peer.username
                                  .toLowerCase()
                                  .contains(textToFind) ||
                              peer.hostname
                                  .toLowerCase()
                                  .contains(textToFind) ||
                              peer.alias.toLowerCase().contains(textToFind))
                          .toList();
                      _allPeersLoader.queryOnlines(_autocompleteOpts);
                    }
                    return _autocompleteOpts;
                  },
                  focusNode: _idFocusNode,
                  textEditingController: _idEditingController,
                  fieldViewBuilder: (
                    BuildContext context,
                    TextEditingController fieldTextEditingController,
                    FocusNode fieldFocusNode,
                    VoidCallback onFieldSubmitted,
                  ) {
                    updateTextAndPreserveSelection(
                        fieldTextEditingController, _idController.text);
                    return Obx(() => TextField(
                          autocorrect: false,
                          enableSuggestions: false,
                          keyboardType: TextInputType.visiblePassword,
                          focusNode: fieldFocusNode,
                          style: const TextStyle(
                            fontFamily: 'WorkSans',
                            fontSize: 22,
                            height: 1.4,
                          ),
                          maxLines: 1,
                          cursorColor:
                              Theme.of(context).textTheme.titleLarge?.color,
                          decoration: InputDecoration(
                              filled: true,
                              fillColor: Theme.of(context)
                                  .colorScheme
                                  .surfaceContainerHighest
                                  .withOpacity(0.5),
                              prefixIcon: const Icon(Icons.link_rounded,
                                  color: _sehBlue, size: 21),
                              border: OutlineInputBorder(
                                  borderRadius: BorderRadius.circular(12),
                                  borderSide: BorderSide.none),
                              counterText: '',
                              hintText: _idInputFocused.value
                                  ? null
                                  : translate('Enter Remote ID'),
                              contentPadding: const EdgeInsets.symmetric(
                                  horizontal: 14, vertical: 12)),
                          controller: fieldTextEditingController,
                          inputFormatters: [IDTextInputFormatter()],
                          onChanged: (v) {
                            _idController.id = v;
                          },
                          onSubmitted: (_) {
                            onConnect();
                          },
                        ).workaroundFreezeLinuxMint());
                  },
                  onSelected: (option) {
                    setState(() {
                      _idController.id = option.id;
                      FocusScope.of(context).unfocus();
                    });
                  },
                  optionsViewBuilder: (BuildContext context,
                      AutocompleteOnSelected<Peer> onSelected,
                      Iterable<Peer> options) {
                    options = _autocompleteOpts;
                    double maxHeight = options.length * 50;
                    if (options.length == 1) {
                      maxHeight = 52;
                    } else if (options.length == 3) {
                      maxHeight = 146;
                    } else if (options.length == 4) {
                      maxHeight = 193;
                    }
                    maxHeight = maxHeight.clamp(0, 200);

                    return Align(
                      alignment: Alignment.topLeft,
                      child: Container(
                          decoration: BoxDecoration(
                            boxShadow: [
                              BoxShadow(
                                color: Colors.black.withOpacity(0.3),
                                blurRadius: 5,
                                spreadRadius: 1,
                              ),
                            ],
                          ),
                          child: ClipRRect(
                              borderRadius: BorderRadius.circular(5),
                              child: Material(
                                elevation: 4,
                                child: ConstrainedBox(
                                  constraints: BoxConstraints(
                                    maxHeight: maxHeight,
                                    maxWidth: 319,
                                  ),
                                  child: _allPeersLoader.peers.isEmpty &&
                                          !_allPeersLoader.isPeersLoaded
                                      ? Container(
                                          height: 80,
                                          child: Center(
                                            child: CircularProgressIndicator(
                                              strokeWidth: 2,
                                            ),
                                          ))
                                      : Padding(
                                          padding:
                                              const EdgeInsets.only(top: 5),
                                          child: ListView(
                                            children: options
                                                .map((peer) =>
                                                    AutocompletePeerTile(
                                                        onSelect: () =>
                                                            onSelected(peer),
                                                        peer: peer))
                                                .toList(),
                                          ),
                                        ),
                                ),
                              ))),
                    );
                  },
                )),
              ],
            ),
            Padding(
              padding: const EdgeInsets.only(top: 12.0),
              child: Row(mainAxisAlignment: MainAxisAlignment.end, children: [
                Expanded(
                    child: Container(
                  height: 48.0,
                  decoration: BoxDecoration(
                    gradient: const LinearGradient(
                      colors: [Color(0xFF0877F9), Color(0xFF10B63B)],
                    ),
                    borderRadius: BorderRadius.circular(10),
                  ),
                  child: ElevatedButton(
                    style: ElevatedButton.styleFrom(
                      backgroundColor: Colors.transparent,
                      foregroundColor: Colors.white,
                      shadowColor: Colors.transparent,
                      elevation: 0,
                      shape: RoundedRectangleBorder(
                          borderRadius: BorderRadius.circular(10)),
                    ),
                    onPressed: () {
                      onConnect();
                    },
                    child: Row(
                      mainAxisAlignment: MainAxisAlignment.center,
                      children: [
                        const Icon(Icons.arrow_forward_rounded, size: 18),
                        const SizedBox(width: 7),
                        Text(translate("Connect"),
                            style:
                                const TextStyle(fontWeight: FontWeight.w700)),
                      ],
                    ),
                  ),
                )),
                const SizedBox(width: 8),
                Container(
                  height: 48.0,
                  width: 48.0,
                  decoration: BoxDecoration(
                    border: Border.all(color: Theme.of(context).dividerColor),
                    borderRadius: BorderRadius.circular(8),
                  ),
                  child: Center(
                    child: StatefulBuilder(
                      builder: (context, setState) {
                        var offset = Offset(0, 0);
                        return Obx(() => InkWell(
                              child: _menuOpen.value
                                  ? Transform.rotate(
                                      angle: pi,
                                      child: Icon(IconFont.more, size: 14),
                                    )
                                  : Icon(IconFont.more, size: 14),
                              onTapDown: (e) {
                                offset = e.globalPosition;
                              },
                              onTap: () async {
                                _menuOpen.value = true;
                                final x = offset.dx;
                                final y = offset.dy;
                                await mod_menu
                                    .showMenu(
                                  context: context,
                                  position: RelativeRect.fromLTRB(x, y, x, y),
                                  items: [
                                    (
                                      '${translate('Connect')} ${translate('Read-only').toLowerCase()}',
                                      () => onConnect(isViewOnly: true)
                                    ),
                                    (
                                      'Transfer file',
                                      () => onConnect(isFileTransfer: true)
                                    ),
                                    (
                                      'View camera',
                                      () => onConnect(isViewCamera: true)
                                    ),
                                    (
                                      '${translate('Terminal')} (beta)',
                                      () => onConnect(isTerminal: true)
                                    ),
                                  ]
                                      .map((e) => MenuEntryButton<String>(
                                            childBuilder: (TextStyle? style) =>
                                                Text(
                                              translate(e.$1),
                                              style: style,
                                            ),
                                            proc: () => e.$2(),
                                            padding: EdgeInsets.symmetric(
                                                horizontal:
                                                    kDesktopMenuPadding.left),
                                            dismissOnClicked: true,
                                          ))
                                      .map((e) => e.build(
                                          context,
                                          const MenuConfig(
                                              commonColor: CustomPopupMenuTheme
                                                  .commonColor,
                                              height:
                                                  CustomPopupMenuTheme.height,
                                              dividerHeight:
                                                  CustomPopupMenuTheme
                                                      .dividerHeight)))
                                      .expand((i) => i)
                                      .toList(),
                                  elevation: 8,
                                )
                                    .then((_) {
                                  _menuOpen.value = false;
                                });
                              },
                            ));
                      },
                    ),
                  ),
                ),
              ]),
            ),
          ],
        ),
      ),
    );
    return Container(
        constraints: const BoxConstraints(maxWidth: 600), child: w);
  }
}
