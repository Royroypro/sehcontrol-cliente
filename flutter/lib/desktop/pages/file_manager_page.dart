import 'dart:async';
import 'dart:io';
import 'dart:math';

import 'package:extended_text/extended_text.dart';
import 'package:flutter_hbb/common/widgets/dialog.dart';
import 'package:flutter_hbb/desktop/widgets/dragable_divider.dart';
import 'package:percent_indicator/percent_indicator.dart';
import 'package:desktop_drop/desktop_drop.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_breadcrumb/flutter_breadcrumb.dart';
import 'package:flutter_hbb/desktop/widgets/list_search_action_listener.dart';
import 'package:flutter_hbb/desktop/widgets/menu_button.dart';
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/common/widgets/file_preview.dart';
import 'package:flutter_hbb/desktop/pages/file_preview_panel.dart';
import 'package:flutter_hbb/models/file_model.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:get/get.dart';
import 'package:flutter_hbb/web/dummy.dart'
    if (dart.library.html) 'package:flutter_hbb/web/web_unique.dart';

import '../../consts.dart';
import '../../desktop/widgets/material_mod_popup_menu.dart' as mod_menu;
import '../../common.dart';
import '../../models/model.dart';
import '../../models/platform_model.dart';
import '../widgets/popup_menu.dart';

/// status of location bar
enum LocationStatus {
  /// normal bread crumb bar
  bread,

  /// show path text field
  pathLocation,

  /// show file search bar text field
  fileSearchBar
}

/// The status of currently focused scope of the mouse
enum MouseFocusScope {
  /// Mouse is in local field.
  local,

  /// Mouse is in remote field.
  remote,

  /// Mouse is not in local field, remote neither.
  none
}

/// File browser layout for each side.
enum FileViewMode {
  /// Compact single-line rows (name + size).
  list,

  /// Detailed rows with name / modified / size columns.
  details,

  /// Grid of tiles with thumbnails.
  tiles,
}

/// A dated section of entries for the "organize by date" grouping.
class _DateGroup {
  final String label;
  final DateTime day;
  final List<Entry> entries;
  _DateGroup(this.label, this.day, this.entries);
}

/// Gesture callbacks shared between the list and grid renderers for one entry.
class _EntryActions {
  final VoidCallback onTap;
  final VoidCallback onSecondaryTap;
  final void Function(TapDownDetails) onSecondaryTapDown;
  _EntryActions(this.onTap, this.onSecondaryTap, this.onSecondaryTapDown);
}

class FileManagerPage extends StatefulWidget {
  FileManagerPage(
      {Key? key,
      required this.id,
      required this.password,
      required this.isSharedPassword,
      this.tabController,
      this.connToken,
      this.forceRelay})
      : super(key: key);
  final String id;
  final String? password;
  final bool? isSharedPassword;
  final bool? forceRelay;
  final String? connToken;
  final DesktopTabController? tabController;
  final SimpleWrapper<State<FileManagerPage>?> _lastState = SimpleWrapper(null);

  FFI get ffi => (_lastState.value! as _FileManagerPageState)._ffi;

  @override
  State<StatefulWidget> createState() {
    final state = _FileManagerPageState();
    _lastState.value = state;
    return state;
  }
}

class _FileManagerPageState extends State<FileManagerPage>
    with AutomaticKeepAliveClientMixin, WidgetsBindingObserver {
  final _mouseFocusScope = Rx<MouseFocusScope>(MouseFocusScope.none);

  final _dropMaskVisible = false.obs; // TODO impl drop mask
  final _overlayKeyState = OverlayKeyState();
  final _uniqueKey = UniqueKey();

  late FFI _ffi;

  FileModel get model => _ffi.fileModel;
  JobController get jobController => model.jobController;

  @override
  void initState() {
    super.initState();
    _ffi = FFI(null);
    _ffi.start(widget.id,
        isFileTransfer: true,
        password: widget.password,
        isSharedPassword: widget.isSharedPassword,
        connToken: widget.connToken,
        forceRelay: widget.forceRelay);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _ffi.dialogManager
          .showLoading(translate('Connecting...'), onCancel: closeConnection);
    });
    Get.put<FFI>(_ffi, tag: 'ft_${widget.id}');
    WakelockManager.enable(_uniqueKey);
    if (isWeb) {
      _ffi.ffiModel.updateEventListener(_ffi.sessionId, widget.id);
    }
    debugPrint("File manager page init success with id ${widget.id}");
    _ffi.dialogManager.setOverlayState(_overlayKeyState);
    // Call onSelected in post frame callback, since we cannot guarantee that the callback will not call setState.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      widget.tabController?.onSelected?.call(widget.id);
    });
    WidgetsBinding.instance.addObserver(this);
  }

  @override
  void dispose() {
    model.close().whenComplete(() {
      _ffi.close();
      _ffi.dialogManager.dismissAll();
      WakelockManager.disable(_uniqueKey);
      Get.delete<FFI>(tag: 'ft_${widget.id}');
    });
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  @override
  bool get wantKeepAlive => true;

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    super.didChangeAppLifecycleState(state);
    if (state == AppLifecycleState.resumed) {
      jobController.jobTable.refresh();
    }
  }

  Widget willPopScope(Widget child) {
    if (isWeb) {
      return WillPopScope(
        onWillPop: () async {
          clientClose(_ffi.sessionId, _ffi);
          return false;
        },
        child: child,
      );
    } else {
      return child;
    }
  }

  @override
  Widget build(BuildContext context) {
    super.build(context);
    return Overlay(key: _overlayKeyState.key, initialEntries: [
      OverlayEntry(builder: (_) {
        return willPopScope(Scaffold(
          backgroundColor: Theme.of(context).scaffoldBackgroundColor,
          body: Column(
            children: [
              Expanded(
                child: Row(
                  children: [
                    if (!isWeb)
                      Flexible(
                          flex: 3,
                          child: dropArea(FileManagerView(
                              model.localController, _ffi, _mouseFocusScope))),
                    Flexible(
                        flex: 3,
                        child: dropArea(FileManagerView(
                            model.remoteController, _ffi, _mouseFocusScope))),
                    Flexible(flex: 2, child: _buildSidePanel())
                  ],
                ),
              ),
              _buildStatusBar(),
            ],
          ),
        ));
      })
    ]);
  }

  /// Right column: shows the preview panel when a single file is selected,
  /// otherwise falls back to the transfer status list.
  Widget _buildSidePanel() {
    return Obx(() {
      final target = model.previewTarget.value;
      if (target != null) {
        final controller =
            target.isLocal ? model.localController : model.remoteController;
        return FilePreviewPanel(
          key: ValueKey('${target.isLocal}_${target.entry.path}'),
          entry: target.entry,
          isLocal: target.isLocal,
          controller: controller,
          onClose: () {
            model.previewTarget.value = null;
            model.localController.selectedItems.clear();
            model.remoteController.selectedItems.clear();
          },
        );
      }
      return statusList();
    });
  }

  /// Bottom status bar: secure-connection indicator, live transfer speeds and
  /// active-transfer count.
  Widget _buildStatusBar() {
    final labelColor = Theme.of(context).tabBarTheme.labelColor;
    return Container(
      height: 34,
      padding: const EdgeInsets.symmetric(horizontal: 16),
      decoration: BoxDecoration(
        color: Theme.of(context).cardColor,
        border: Border(
          top: BorderSide(color: Theme.of(context).dividerColor, width: 0.5),
        ),
      ),
      child: Row(
        children: [
          Icon(Icons.lock_outline, size: 14, color: Colors.green),
          const SizedBox(width: 6),
          Text(
            translate('Secure Connection'),
            style: TextStyle(fontSize: 12, color: labelColor),
          ),
          const Spacer(),
          Obx(() {
            double up = 0, down = 0;
            var active = 0;
            for (final job in jobController.jobTable) {
              if (job.type == JobType.transfer &&
                  job.state == JobState.inProgress) {
                active++;
                if (job.isRemoteToLocal) {
                  down += job.speed;
                } else {
                  up += job.speed;
                }
              }
            }
            String fmt(double s) => '${readableFileSize(s)}/s';
            return Row(
              children: [
                Icon(Icons.arrow_upward, size: 14, color: MyTheme.darkGray),
                const SizedBox(width: 2),
                Text(fmt(up),
                    style: TextStyle(fontSize: 12, color: labelColor)),
                const SizedBox(width: 14),
                Icon(Icons.arrow_downward, size: 14, color: MyTheme.darkGray),
                const SizedBox(width: 2),
                Text(fmt(down),
                    style: TextStyle(fontSize: 12, color: labelColor)),
                const SizedBox(width: 16),
                Text(
                  active > 0
                      ? '$active ${translate('Active')}'
                      : translate('Idle'),
                  style: TextStyle(fontSize: 12, color: MyTheme.darkGray),
                ),
              ],
            );
          }),
          const Spacer(),
          if (!isWeb) _DiskUsageIndicator(controller: model.localController),
        ],
      ),
    );
  }

  Widget dropArea(FileManagerView fileView) {
    return DropTarget(
        onDragDone: (detail) =>
            handleDragDone(detail, fileView.controller.isLocal),
        onDragEntered: (enter) {
          _dropMaskVisible.value = true;
        },
        onDragExited: (exit) {
          _dropMaskVisible.value = false;
        },
        child: fileView);
  }

  Widget generateCard(Widget child) {
    return Container(
      decoration: BoxDecoration(
        color: Theme.of(context).cardColor,
        borderRadius: BorderRadius.all(
          Radius.circular(15.0),
        ),
      ),
      child: child,
    );
  }

  /// transfer status list
  /// watch transfer status
  Widget statusList() {
    Widget getIcon(JobProgress job) {
      final color = Theme.of(context).tabBarTheme.labelColor;
      switch (job.type) {
        case JobType.deleteDir:
        case JobType.deleteFile:
          return Icon(Icons.delete_outline, color: color);
        default:
          return Transform.rotate(
            angle: isWeb
                ? job.isRemoteToLocal
                    ? pi / 2
                    : pi / 2 * 3
                : job.isRemoteToLocal
                    ? pi
                    : 0,
            child: Icon(Icons.arrow_forward_ios, color: color),
          );
      }
    }

    statusListView(List<JobProgress> jobs) => ListView.builder(
          controller: ScrollController(),
          itemBuilder: (BuildContext context, int index) {
            final item = jobs[index];
            final status = item.getStatus();
            return Padding(
              padding: const EdgeInsets.only(bottom: 5),
              child: generateCard(
                Column(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Row(
                      crossAxisAlignment: CrossAxisAlignment.center,
                      children: [
                        getIcon(item)
                            .marginSymmetric(horizontal: 10, vertical: 12),
                        Expanded(
                          child: Column(
                            mainAxisSize: MainAxisSize.min,
                            crossAxisAlignment: CrossAxisAlignment.start,
                            children: [
                              Tooltip(
                                waitDuration: Duration(milliseconds: 500),
                                message: item.jobName,
                                child: ExtendedText(
                                  item.jobName,
                                  maxLines: 1,
                                  overflow: TextOverflow.ellipsis,
                                  overflowWidget: TextOverflowWidget(
                                      child: Text("..."),
                                      position: TextOverflowPosition.start),
                                ),
                              ),
                              Tooltip(
                                waitDuration: Duration(milliseconds: 500),
                                message: status,
                                child: Text(status,
                                    style: TextStyle(
                                      fontSize: 12,
                                      color: MyTheme.darkGray,
                                    )).marginOnly(top: 6),
                              ),
                              Offstage(
                                offstage: item.type != JobType.transfer ||
                                    item.state != JobState.inProgress,
                                child: LinearPercentIndicator(
                                  animateFromLastPercent: true,
                                  center: Text(item.percentText),
                                  barRadius: Radius.circular(15),
                                  percent: item.percent,
                                  progressColor: MyTheme.accent,
                                  backgroundColor: Theme.of(context).hoverColor,
                                  lineHeight: kDesktopFileTransferRowHeight,
                                ).paddingSymmetric(vertical: 8),
                              ),
                            ],
                          ),
                        ),
                        Row(
                          mainAxisAlignment: MainAxisAlignment.end,
                          children: [
                            Offstage(
                              offstage: item.state != JobState.paused,
                              child: MenuButton(
                                tooltip: translate("Resume"),
                                onPressed: () {
                                  jobController.resumeJob(item.id);
                                },
                                child: SvgPicture.asset(
                                  "assets/refresh.svg",
                                  colorFilter: svgColor(Colors.white),
                                ),
                                color: MyTheme.accent,
                                hoverColor: MyTheme.accent80,
                              ),
                            ),
                            MenuButton(
                              tooltip: translate("Delete"),
                              child: SvgPicture.asset(
                                "assets/close.svg",
                                colorFilter: svgColor(Colors.white),
                              ),
                              onPressed: () {
                                jobController.jobTable.removeAt(index);
                                jobController.cancelJob(item.id);
                              },
                              color: MyTheme.accent,
                              hoverColor: MyTheme.accent80,
                            ),
                          ],
                        ).marginAll(12),
                      ],
                    ),
                  ],
                ),
              ),
            );
          },
          itemCount: jobController.jobTable.length,
        );

    return PreferredSize(
      preferredSize: const Size(200, double.infinity),
      child: Container(
          margin: const EdgeInsets.only(top: 16.0, bottom: 16.0, right: 16.0),
          padding: const EdgeInsets.all(8.0),
          child: Obx(
            () => jobController.jobTable.isEmpty
                ? generateCard(
                    Center(
                      child: Column(
                        mainAxisAlignment: MainAxisAlignment.center,
                        children: [
                          SvgPicture.asset(
                            "assets/transfer.svg",
                            colorFilter: svgColor(
                                Theme.of(context).tabBarTheme.labelColor),
                            height: 40,
                          ).paddingOnly(bottom: 10),
                          Text(
                            translate("No transfers in progress"),
                            textAlign: TextAlign.center,
                            textScaler: TextScaler.linear(1.20),
                            style: TextStyle(
                                color:
                                    Theme.of(context).tabBarTheme.labelColor),
                          ),
                        ],
                      ),
                    ),
                  )
                : statusListView(jobController.jobTable),
          )),
    );
  }

  void handleDragDone(DropDoneDetails details, bool isLocal) {
    if (isLocal) {
      // ignore local
      return;
    }
    final items = SelectedItems(isLocal: false);
    for (var file in details.files) {
      final f = File(file.path);
      items.add(Entry()
        ..path = file.path
        ..name = file.name
        ..size = FileSystemEntity.isDirectorySync(f.path) ? 0 : f.lengthSync());
    }
    final otherSideData = model.localController.directoryData();
    model.remoteController.sendFiles(items, otherSideData);
  }
}

class FileManagerView extends StatefulWidget {
  final FileController controller;
  final FFI _ffi;
  final Rx<MouseFocusScope> _mouseFocusScope;

  FileManagerView(this.controller, this._ffi, this._mouseFocusScope);

  @override
  State<StatefulWidget> createState() => _FileManagerViewState();
}

class _FileManagerViewState extends State<FileManagerView> {
  final _locationStatus = LocationStatus.bread.obs;
  final _locationNode = FocusNode();
  final _locationBarKey = GlobalKey();
  final _searchText = "".obs;
  final _breadCrumbScroller = ScrollController();
  final _keyboardNode = FocusNode();
  final _listSearchBuffer = TimeoutStringBuffer();
  final _nameColWidth = 0.0.obs;
  final _modifiedColWidth = 0.0.obs;
  final _sizeColWidth = 0.0.obs;
  final _fileListScrollController = ScrollController();
  final _globalHeaderKey = GlobalKey();
  final _viewMode = FileViewMode.details.obs;
  final _groupByDate = false.obs;
  final _typeFilter = FileFilter.all.obs;

  /// Date-group labels the user has expanded via "See all".
  final _expandedGroups = <String>{}.obs;

  /// Currently selected date bucket in the sidebar (null = all dates).
  final _selectedDateBucket = Rxn<DateTime>();

  /// Whether the date sidebar shows every bucket or just the first few.
  final _dateSidebarExpanded = false.obs;

  /// Number of items shown per date group before the "See all" toggle.
  static const int _groupPreviewCount = 12;

  /// Number of date buckets shown in the sidebar before "See more".
  static const int _sidebarBucketCount = 6;

  /// Max size of a remote image auto-downloaded to render a grid thumbnail.
  static const int _remoteThumbSizeLimit = 10 * 1024 * 1024;

  /// [_lastClickTime], [_lastClickEntry] help to handle double click
  var _lastClickTime =
      DateTime.now().millisecondsSinceEpoch - bind.getDoubleClickTime() - 1000;
  Entry? _lastClickEntry;

  double? _windowWidthPrev;
  double _fileTransferMinimumWidth = 0.0;

  FileController get controller => widget.controller;
  bool get isLocal => widget.controller.isLocal;
  FFI get _ffi => widget._ffi;
  SelectedItems get selectedItems => controller.selectedItems;

  @override
  void initState() {
    super.initState();
    // register location listener
    _locationNode.addListener(onLocationFocusChanged);
    controller.directory.listen((e) {
      breadCrumbScrollToEnd();
      _selectedDateBucket.value = null;
      _expandedGroups.clear();
    });
  }

  @override
  void dispose() {
    _locationNode.removeListener(onLocationFocusChanged);
    _locationNode.dispose();
    _keyboardNode.dispose();
    _breadCrumbScroller.dispose();
    _fileListScrollController.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    _handleColumnPorportions();
    return Container(
      margin: const EdgeInsets.all(16.0),
      padding: const EdgeInsets.all(8.0),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          headTools(),
          Expanded(
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Expanded(
                    child: MouseRegion(
                  onEnter: (evt) {
                    widget._mouseFocusScope.value = isLocal
                        ? MouseFocusScope.local
                        : MouseFocusScope.remote;
                    _keyboardNode.requestFocus();
                  },
                  onExit: (evt) =>
                      widget._mouseFocusScope.value = MouseFocusScope.none,
                  child: _buildFileList(context, _fileListScrollController),
                ))
              ],
            ),
          ),
          _buildSelectionFooter(),
        ],
      ),
    );
  }

  Widget _buildSelectionFooter() {
    return Obx(() {
      final total = controller.directory.value.entries.length;
      final selected = selectedItems.items;
      final count = selected.length;
      var size = 0;
      for (final e in selected) {
        if (e.isFile) size += e.size;
      }
      final label = count == 0
          ? '$total ${translate('files')}'
          : '$count ${translate('of')} $total ${translate('selected')}';
      return Container(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
        margin: const EdgeInsets.only(top: 4),
        decoration: BoxDecoration(
          color: Theme.of(context).cardColor,
          borderRadius: BorderRadius.circular(8),
        ),
        child: Row(
          children: [
            Icon(Icons.check_circle_outline,
                size: 14, color: MyTheme.darkGray),
            const SizedBox(width: 6),
            Expanded(
              child: Text(label,
                  style: TextStyle(fontSize: 12, color: MyTheme.darkGray)),
            ),
            if (count > 0)
              Text(readableFileSize(size.toDouble()),
                  style: TextStyle(fontSize: 12, color: MyTheme.darkGray)),
          ],
        ),
      );
    });
  }

  void _handleColumnPorportions() {
    final windowWidthNow = MediaQuery.of(context).size.width;
    if (_windowWidthPrev == null) {
      _windowWidthPrev = windowWidthNow;
      final defaultColumnWidth = windowWidthNow * 0.115;
      _fileTransferMinimumWidth = defaultColumnWidth / 3;
      _nameColWidth.value = defaultColumnWidth;
      _modifiedColWidth.value = defaultColumnWidth;
      _sizeColWidth.value = defaultColumnWidth;
    }

    if (_windowWidthPrev != windowWidthNow) {
      final difference = windowWidthNow / _windowWidthPrev!;
      _windowWidthPrev = windowWidthNow;
      _fileTransferMinimumWidth *= difference;
      _nameColWidth.value *= difference;
      _modifiedColWidth.value *= difference;
      _sizeColWidth.value *= difference;
    }
  }

  void onLocationFocusChanged() {
    debugPrint("focus changed on local");
    if (_locationNode.hasFocus) {
      // ignore
    } else {
      // lost focus, change to bread
      if (_locationStatus.value != LocationStatus.fileSearchBar) {
        _locationStatus.value = LocationStatus.bread;
      }
    }
  }

  Widget headTools() {
    var uploadButtonTapPosition = RelativeRect.fill;
    RxBool isUploadFolder =
        (bind.mainGetLocalOption(key: 'upload-folder-button') == 'Y').obs;
    return Container(
      child: Column(
        children: [
          // symbols
          PreferredSize(
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.center,
                    children: [
                      Container(
                          width: 50,
                          height: 50,
                          decoration: BoxDecoration(
                            borderRadius: BorderRadius.all(Radius.circular(8)),
                            color: MyTheme.accent,
                          ),
                          padding: EdgeInsets.all(8.0),
                          child: FutureBuilder<String>(
                              future: bind.sessionGetPlatform(
                                  sessionId: _ffi.sessionId,
                                  isRemote: !isLocal),
                              builder: (context, snapshot) {
                                if (snapshot.hasData &&
                                    snapshot.data!.isNotEmpty) {
                                  return getPlatformImage('${snapshot.data}');
                                } else {
                                  return CircularProgressIndicator(
                                    color: Theme.of(context)
                                        .tabBarTheme
                                        .labelColor,
                                  );
                                }
                              })),
                      Text(isLocal
                              ? translate("Local Computer")
                              : translate("Remote Computer"))
                          .marginOnly(left: 8.0)
                    ],
                  ),
                  preferredSize: Size(double.infinity, 70))
              .paddingOnly(bottom: 15),
          // buttons
          Row(
            children: [
              Row(
                children: [
                  MenuButton(
                    tooltip: translate('Back'),
                    padding: EdgeInsets.only(
                      right: 3,
                    ),
                    child: RotatedBox(
                      quarterTurns: 2,
                      child: SvgPicture.asset(
                        "assets/arrow.svg",
                        colorFilter:
                            svgColor(Theme.of(context).tabBarTheme.labelColor),
                      ),
                    ),
                    color: Theme.of(context).cardColor,
                    hoverColor: Theme.of(context).hoverColor,
                    onPressed: () {
                      selectedItems.clear();
                      controller.goBack();
                    },
                  ),
                  MenuButton(
                    tooltip: translate('Parent directory'),
                    child: RotatedBox(
                      quarterTurns: 3,
                      child: SvgPicture.asset(
                        "assets/arrow.svg",
                        colorFilter:
                            svgColor(Theme.of(context).tabBarTheme.labelColor),
                      ),
                    ),
                    color: Theme.of(context).cardColor,
                    hoverColor: Theme.of(context).hoverColor,
                    onPressed: () {
                      selectedItems.clear();
                      controller.goToParentDirectory();
                    },
                  ),
                ],
              ),
              Expanded(
                child: Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 3.0),
                  child: Container(
                    decoration: BoxDecoration(
                      color: Theme.of(context).cardColor,
                      borderRadius: BorderRadius.all(
                        Radius.circular(8.0),
                      ),
                    ),
                    child: Padding(
                      padding: EdgeInsets.symmetric(vertical: 2.5),
                      child: GestureDetector(
                        onTap: () {
                          _locationStatus.value =
                              _locationStatus.value == LocationStatus.bread
                                  ? LocationStatus.pathLocation
                                  : LocationStatus.bread;
                          Future.delayed(Duration.zero, () {
                            if (_locationStatus.value ==
                                LocationStatus.pathLocation) {
                              _locationNode.requestFocus();
                            }
                          });
                        },
                        child: Obx(
                          () => Container(
                            child: Row(
                              children: [
                                Expanded(
                                    child: _locationStatus.value ==
                                            LocationStatus.bread
                                        ? buildBread()
                                        : buildPathLocation()),
                              ],
                            ),
                          ),
                        ),
                      ),
                    ),
                  ),
                ),
              ),
              Obx(() {
                switch (_locationStatus.value) {
                  case LocationStatus.bread:
                    return MenuButton(
                      tooltip: translate('Search'),
                      onPressed: () {
                        _locationStatus.value = LocationStatus.fileSearchBar;
                        Future.delayed(
                            Duration.zero, () => _locationNode.requestFocus());
                      },
                      child: SvgPicture.asset(
                        "assets/search.svg",
                        colorFilter:
                            svgColor(Theme.of(context).tabBarTheme.labelColor),
                      ),
                      color: Theme.of(context).cardColor,
                      hoverColor: Theme.of(context).hoverColor,
                    );
                  case LocationStatus.pathLocation:
                    return MenuButton(
                      onPressed: null,
                      child: SvgPicture.asset(
                        "assets/close.svg",
                        colorFilter:
                            svgColor(Theme.of(context).tabBarTheme.labelColor),
                      ),
                      color: Theme.of(context).disabledColor,
                      hoverColor: Theme.of(context).hoverColor,
                    );
                  case LocationStatus.fileSearchBar:
                    return MenuButton(
                      tooltip: translate('Clear'),
                      onPressed: () {
                        onSearchText("", isLocal);
                        _locationStatus.value = LocationStatus.bread;
                      },
                      child: SvgPicture.asset(
                        "assets/close.svg",
                        colorFilter:
                            svgColor(Theme.of(context).tabBarTheme.labelColor),
                      ),
                      color: Theme.of(context).cardColor,
                      hoverColor: Theme.of(context).hoverColor,
                    );
                }
              }),
              MenuButton(
                tooltip: translate('Refresh File'),
                padding: EdgeInsets.only(
                  left: 3,
                ),
                onPressed: () {
                  controller.refresh();
                },
                child: SvgPicture.asset(
                  "assets/refresh.svg",
                  colorFilter:
                      svgColor(Theme.of(context).tabBarTheme.labelColor),
                ),
                color: Theme.of(context).cardColor,
                hoverColor: Theme.of(context).hoverColor,
              ),
              _buildViewModeToggle(),
              _buildFilterButton(),
            ],
          ),
          Row(
            textDirection: isLocal ? TextDirection.ltr : TextDirection.rtl,
            children: [
              Expanded(
                child: Row(
                  mainAxisAlignment:
                      isLocal ? MainAxisAlignment.start : MainAxisAlignment.end,
                  children: [
                    MenuButton(
                      tooltip: translate('Home'),
                      padding: EdgeInsets.only(
                        right: 3,
                      ),
                      onPressed: () {
                        controller.goToHomeDirectory();
                      },
                      child: SvgPicture.asset(
                        "assets/home.svg",
                        colorFilter:
                            svgColor(Theme.of(context).tabBarTheme.labelColor),
                      ),
                      color: Theme.of(context).cardColor,
                      hoverColor: Theme.of(context).hoverColor,
                    ),
                    MenuButton(
                      tooltip: translate('Create Folder'),
                      onPressed: () {
                        final name = TextEditingController();
                        String? errorText;
                        _ffi.dialogManager.show((setState, close, context) {
                          name.addListener(() {
                            if (errorText != null) {
                              setState(() {
                                errorText = null;
                              });
                            }
                          });
                          submit() {
                            if (name.value.text.isNotEmpty) {
                              if (!PathUtil.validName(name.value.text,
                                  controller.options.value.isWindows)) {
                                setState(() {
                                  errorText = translate("Invalid folder name");
                                });
                                return;
                              }
                              controller.createDir(PathUtil.join(
                                controller.directory.value.path,
                                name.value.text,
                                controller.options.value.isWindows,
                              ));
                              close();
                            }
                          }

                          cancel() => close(false);
                          return CustomAlertDialog(
                            title: Row(
                              mainAxisAlignment: MainAxisAlignment.center,
                              children: [
                                SvgPicture.asset("assets/folder_new.svg",
                                    colorFilter: svgColor(MyTheme.accent)),
                                Text(
                                  translate("Create Folder"),
                                ).paddingOnly(
                                  left: 10,
                                ),
                              ],
                            ),
                            content: Column(
                              mainAxisSize: MainAxisSize.min,
                              children: [
                                TextFormField(
                                  decoration: InputDecoration(
                                    labelText: translate(
                                      "Please enter the folder name",
                                    ),
                                    errorText: errorText,
                                  ),
                                  controller: name,
                                  autofocus: true,
                                ).workaroundFreezeLinuxMint(),
                              ],
                            ),
                            actions: [
                              dialogButton(
                                "Cancel",
                                icon: Icon(Icons.close_rounded),
                                onPressed: cancel,
                                isOutline: true,
                              ),
                              dialogButton(
                                "Ok",
                                icon: Icon(Icons.done_rounded),
                                onPressed: submit,
                              ),
                            ],
                            onSubmit: submit,
                            onCancel: cancel,
                          );
                        });
                      },
                      child: SvgPicture.asset(
                        "assets/folder_new.svg",
                        colorFilter:
                            svgColor(Theme.of(context).tabBarTheme.labelColor),
                      ),
                      color: Theme.of(context).cardColor,
                      hoverColor: Theme.of(context).hoverColor,
                    ),
                    Obx(() => MenuButton(
                          tooltip: translate('Delete'),
                          onPressed: SelectedItems.valid(selectedItems.items)
                              ? () async {
                                  await (controller
                                      .removeAction(selectedItems));
                                  selectedItems.clear();
                                }
                              : null,
                          child: SvgPicture.asset(
                            "assets/trash.svg",
                            colorFilter: svgColor(
                                Theme.of(context).tabBarTheme.labelColor),
                          ),
                          color: Theme.of(context).cardColor,
                          hoverColor: Theme.of(context).hoverColor,
                        )),
                    menu(isLocal: isLocal),
                  ],
                ),
              ),
              if (isWeb)
                Obx(() => ElevatedButton.icon(
                      style: ButtonStyle(
                        padding: MaterialStateProperty.all<EdgeInsetsGeometry>(
                            isLocal
                                ? EdgeInsets.only(left: 10)
                                : EdgeInsets.only(right: 10)),
                        backgroundColor: MaterialStateProperty.all(
                          selectedItems.items.isEmpty
                              ? MyTheme.accent80
                              : MyTheme.accent,
                        ),
                      ),
                      onPressed: () =>
                          {webselectFiles(is_folder: isUploadFolder.value)},
                      label: InkWell(
                        hoverColor: Colors.transparent,
                        splashColor: Colors.transparent,
                        highlightColor: Colors.transparent,
                        focusColor: Colors.transparent,
                        onTapDown: (e) {
                          final x = e.globalPosition.dx;
                          final y = e.globalPosition.dy;
                          uploadButtonTapPosition =
                              RelativeRect.fromLTRB(x, y, x, y);
                        },
                        onTap: () async {
                          final value = await showMenu<bool>(
                              context: context,
                              position: uploadButtonTapPosition,
                              items: [
                                PopupMenuItem<bool>(
                                  value: false,
                                  child: Text(translate('Upload files')),
                                ),
                                PopupMenuItem<bool>(
                                  value: true,
                                  child: Text(translate('Upload folder')),
                                ),
                              ]);
                          if (value != null) {
                            isUploadFolder.value = value;
                            bind.mainSetLocalOption(
                                key: 'upload-folder-button',
                                value: value ? 'Y' : '');
                            webselectFiles(is_folder: value);
                          }
                        },
                        child: Icon(Icons.arrow_drop_down),
                      ),
                      icon: Text(
                        translate(isUploadFolder.isTrue
                            ? 'Upload folder'
                            : 'Upload files'),
                        textAlign: TextAlign.right,
                        style: TextStyle(
                          color: Colors.white,
                        ),
                      ).marginOnly(left: 8),
                    )).marginOnly(left: 16),
              Obx(() => ElevatedButton.icon(
                    style: ButtonStyle(
                      padding: MaterialStateProperty.all<EdgeInsetsGeometry>(
                          isLocal
                              ? EdgeInsets.only(left: 10)
                              : EdgeInsets.only(right: 10)),
                      backgroundColor: MaterialStateProperty.all(
                        selectedItems.items.isEmpty
                            ? MyTheme.accent80
                            : MyTheme.accent,
                      ),
                    ),
                    onPressed: SelectedItems.valid(selectedItems.items)
                        ? () {
                            final otherSideData =
                                controller.getOtherSideDirectoryData();
                            controller.sendFiles(selectedItems, otherSideData);
                            selectedItems.clear();
                          }
                        : null,
                    icon: isLocal
                        ? Text(
                            translate('Send'),
                            textAlign: TextAlign.right,
                            style: TextStyle(
                              color: selectedItems.items.isEmpty
                                  ? Theme.of(context).brightness ==
                                          Brightness.light
                                      ? MyTheme.grayBg
                                      : MyTheme.darkGray
                                  : Colors.white,
                            ),
                          )
                        : isWeb
                            ? Offstage()
                            : RotatedBox(
                                quarterTurns: 2,
                                child: SvgPicture.asset(
                                  "assets/arrow.svg",
                                  colorFilter: svgColor(
                                      selectedItems.items.isEmpty
                                          ? Theme.of(context).brightness ==
                                                  Brightness.light
                                              ? MyTheme.grayBg
                                              : MyTheme.darkGray
                                          : Colors.white),
                                  alignment: Alignment.bottomRight,
                                ),
                              ),
                    label: isLocal
                        ? SvgPicture.asset(
                            "assets/arrow.svg",
                            colorFilter: svgColor(selectedItems.items.isEmpty
                                ? Theme.of(context).brightness ==
                                        Brightness.light
                                    ? MyTheme.grayBg
                                    : MyTheme.darkGray
                                : Colors.white),
                          )
                        : Text(
                            translate(isWeb ? 'Download' : 'Receive'),
                            style: TextStyle(
                              color: selectedItems.items.isEmpty
                                  ? Theme.of(context).brightness ==
                                          Brightness.light
                                      ? MyTheme.grayBg
                                      : MyTheme.darkGray
                                  : Colors.white,
                            ),
                          ),
                  )),
            ],
          ).marginOnly(top: 8.0)
        ],
      ),
    );
  }

  Widget menu({bool isLocal = false}) {
    var menuPos = RelativeRect.fill;

    final List<MenuEntryBase<String>> items = [
      MenuEntrySwitch<String>(
        switchType: SwitchType.scheckbox,
        text: translate("Show Hidden Files"),
        getter: () async {
          return controller.options.value.showHidden;
        },
        setter: (bool v) async {
          controller.toggleShowHidden();
        },
        padding: kDesktopMenuPadding,
        dismissOnClicked: true,
      ),
      MenuEntryButton(
          childBuilder: (style) => Text(translate("Select All"), style: style),
          proc: () => setState(() =>
              selectedItems.selectAll(controller.directory.value.entries)),
          padding: kDesktopMenuPadding,
          dismissOnClicked: true),
      MenuEntryButton(
          childBuilder: (style) =>
              Text(translate("Unselect All"), style: style),
          proc: () => selectedItems.clear(),
          padding: kDesktopMenuPadding,
          dismissOnClicked: true)
    ];

    return Listener(
      onPointerDown: (e) {
        final x = e.position.dx;
        final y = e.position.dy;
        menuPos = RelativeRect.fromLTRB(x, y, x, y);
      },
      child: MenuButton(
        tooltip: translate('More'),
        onPressed: () => mod_menu.showMenu(
          context: context,
          position: menuPos,
          items: items
              .map(
                (e) => e.build(
                  context,
                  MenuConfig(
                      commonColor: CustomPopupMenuTheme.commonColor,
                      height: CustomPopupMenuTheme.height,
                      dividerHeight: CustomPopupMenuTheme.dividerHeight),
                ),
              )
              .expand((i) => i)
              .toList(),
          elevation: 8,
        ),
        child: SvgPicture.asset(
          "assets/dots.svg",
          colorFilter: svgColor(Theme.of(context).tabBarTheme.labelColor),
        ),
        color: Theme.of(context).cardColor,
        hoverColor: Theme.of(context).hoverColor,
      ),
    );
  }

  Widget _buildViewModeToggle() {
    Widget btn(FileViewMode mode, IconData icon, String tip) {
      return Obx(() {
        final selected = _viewMode.value == mode;
        return Tooltip(
          message: translate(tip),
          child: InkWell(
            borderRadius: BorderRadius.circular(6),
            onTap: () => _viewMode.value = mode,
            child: Container(
              padding: const EdgeInsets.all(6),
              decoration: BoxDecoration(
                color: selected ? MyTheme.accent : Colors.transparent,
                borderRadius: BorderRadius.circular(6),
              ),
              child: Icon(
                icon,
                size: 18,
                color: selected
                    ? Colors.white
                    : Theme.of(context).tabBarTheme.labelColor,
              ),
            ),
          ),
        );
      });
    }

    return Container(
      margin: const EdgeInsets.only(left: 6),
      padding: const EdgeInsets.all(2),
      decoration: BoxDecoration(
        color: Theme.of(context).cardColor,
        borderRadius: BorderRadius.circular(8),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          btn(FileViewMode.list, Icons.view_list_outlined, 'List'),
          btn(FileViewMode.details, Icons.view_headline_outlined, 'Details'),
          btn(FileViewMode.tiles, Icons.grid_view_outlined, 'Tiles'),
        ],
      ),
    );
  }

  Widget _buildFilterButton() {
    return Obx(() {
      final active = _typeFilter.value != FileFilter.all;
      return PopupMenuButton<FileFilter>(
        tooltip: translate('Filter'),
        position: PopupMenuPosition.under,
        onSelected: (f) => _typeFilter.value = f,
        itemBuilder: (context) => FileFilter.values.map((f) {
          final selected = _typeFilter.value == f;
          return PopupMenuItem<FileFilter>(
            value: f,
            child: Row(
              children: [
                Icon(fileFilterIcon(f),
                    size: 18,
                    color: selected
                        ? MyTheme.accent
                        : Theme.of(context).tabBarTheme.labelColor),
                const SizedBox(width: 10),
                Text(fileFilterLabel(f),
                    style: TextStyle(
                        color: selected ? MyTheme.accent : null,
                        fontWeight:
                            selected ? FontWeight.w600 : FontWeight.normal)),
              ],
            ),
          );
        }).toList(),
        child: Container(
          margin: const EdgeInsets.only(left: 6),
          padding: const EdgeInsets.all(8),
          decoration: BoxDecoration(
            color: active ? MyTheme.accent : Theme.of(context).cardColor,
            borderRadius: BorderRadius.circular(8),
          ),
          child: Icon(
            Icons.filter_list,
            size: 18,
            color:
                active ? Colors.white : Theme.of(context).tabBarTheme.labelColor,
          ),
        ),
      );
    });
  }

  Widget _buildGroupByDateToggle() {
    return Obx(() {
      final on = _groupByDate.value;
      return InkWell(
        borderRadius: BorderRadius.circular(6),
        onTap: () {
          _groupByDate.value = !on;
          if (on) _selectedDateBucket.value = null;
        },
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(Icons.calendar_month_outlined,
                  size: 16,
                  color: on
                      ? MyTheme.accent
                      : Theme.of(context).tabBarTheme.labelColor),
              const SizedBox(width: 6),
              Text(
                translate('Organize by date'),
                style: TextStyle(
                  fontSize: 12,
                  color: on
                      ? MyTheme.accent
                      : Theme.of(context).tabBarTheme.labelColor,
                  fontWeight: on ? FontWeight.w600 : FontWeight.normal,
                ),
              ),
              Icon(on ? Icons.expand_less : Icons.expand_more,
                  size: 16, color: MyTheme.darkGray),
            ],
          ),
        ),
      );
    });
  }

  /// Groups [entries] into dated sections: Today, Yesterday, then one section
  /// per distinct older date (most recent first).
  List<_DateGroup> _dateGroups(List<Entry> entries) {
    final now = DateTime.now();
    final today = DateTime(now.year, now.month, now.day);
    final yesterday = today.subtract(const Duration(days: 1));
    final map = <DateTime, List<Entry>>{};
    for (final e in entries) {
      final d = e.lastModified();
      final key = DateTime(d.year, d.month, d.day);
      (map[key] ??= []).add(e);
    }
    final keys = map.keys.toList()..sort((a, b) => b.compareTo(a));
    return keys.map((k) {
      String label;
      if (k == today) {
        label = translate('Today');
      } else if (k == yesterday) {
        label = translate('Yesterday');
      } else {
        label = _formatDateLabel(k);
      }
      return _DateGroup(label, k, map[k]!);
    }).toList();
  }

  String _formatDateLabel(DateTime d) {
    const weekdayKeys = [
      'Monday',
      'Tuesday',
      'Wednesday',
      'Thursday',
      'Friday',
      'Saturday',
      'Sunday'
    ];
    final wd = translate(weekdayKeys[d.weekday - 1]);
    String two(int n) => n.toString().padLeft(2, '0');
    return '$wd ${two(d.day)}/${two(d.month)}/${d.year}';
  }

  Widget _buildDateSidebar(List<_DateGroup> groups, int total) {
    final bucket = _selectedDateBucket.value;
    final expanded = _dateSidebarExpanded.value;
    final visible = expanded
        ? groups
        : groups.take(_sidebarBucketCount).toList(growable: false);
    return Container(
      width: 176,
      padding: const EdgeInsets.symmetric(vertical: 6, horizontal: 4),
      child: ListView(
        children: [
          _sidebarItem(translate('All dates'), total, bucket == null,
              () => _selectedDateBucket.value = null),
          for (final g in visible)
            _sidebarItem(g.label, g.entries.length, bucket == g.day,
                () => _selectedDateBucket.value = g.day),
          if (groups.length > _sidebarBucketCount)
            Padding(
              padding: const EdgeInsets.only(top: 4),
              child: TextButton.icon(
                onPressed: () =>
                    _dateSidebarExpanded.value = !_dateSidebarExpanded.value,
                icon: Icon(expanded ? Icons.expand_less : Icons.expand_more,
                    size: 18),
                label: Text(
                    expanded ? translate('See less') : translate('See more')),
              ),
            ),
        ],
      ),
    );
  }

  Widget _sidebarItem(
      String label, int count, bool selected, VoidCallback onTap) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 2),
      child: InkWell(
        borderRadius: BorderRadius.circular(8),
        onTap: onTap,
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 8),
          decoration: BoxDecoration(
            color: selected
                ? MyTheme.accent.withOpacity(0.15)
                : Colors.transparent,
            borderRadius: BorderRadius.circular(8),
          ),
          child: Row(
            children: [
              Icon(
                selected
                    ? Icons.radio_button_checked
                    : Icons.radio_button_unchecked,
                size: 16,
                color: selected ? MyTheme.accent : MyTheme.darkGray,
              ),
              const SizedBox(width: 8),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      label,
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        fontSize: 12,
                        fontWeight:
                            selected ? FontWeight.w600 : FontWeight.normal,
                        color: selected ? MyTheme.accent : null,
                      ),
                    ),
                    Text('$count ${translate('files')}',
                        style:
                            TextStyle(fontSize: 10, color: MyTheme.darkGray)),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget _buildSeeAllButton(String label, int total, bool expanded) {
    return Center(
      child: TextButton.icon(
        onPressed: () {
          if (expanded) {
            _expandedGroups.remove(label);
          } else {
            _expandedGroups.add(label);
          }
        },
        icon: Icon(expanded ? Icons.expand_less : Icons.expand_more, size: 18),
        label: Text(expanded
            ? translate('See less')
            : '${translate('See all')} ($total)'),
      ),
    );
  }

  Widget _groupHeader(String label, int count) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(4, 12, 4, 6),
      child: Row(
        children: [
          Text(label,
              style: const TextStyle(fontWeight: FontWeight.w600, fontSize: 13)),
          const Spacer(),
          Text('$count ${translate('files')}',
              style: TextStyle(fontSize: 11, color: MyTheme.darkGray)),
        ],
      ),
    );
  }

  /// Builds the shared tap / double-click / context-menu callbacks for [entry].
  _EntryActions _entryActions(BuildContext context, Entry entry,
      List<Entry> filteredEntries, Rx<Entry?> rightClickEntry) {
    var secondaryPosition = RelativeRect.fromLTRB(0, 0, 0, 0);
    onTap() {
      final items = selectedItems;
      if (_checkDoubleClick(entry)) {
        controller.openDirectory(entry.path);
        items.clear();
        if (entry.isDirectory || entry.isDrive) {
          _ffi.fileModel.previewTarget.value = null;
        }
        return;
      }
      _onSelectedChanged(items, filteredEntries, entry, isLocal);
    }

    onSecondaryTap() {
      final items = [
        if (!entry.isDrive &&
            versionCmp(_ffi.ffiModel.pi.version, "1.3.0") >= 0)
          mod_menu.PopupMenuItem(
            child: Text(translate("Rename")),
            height: CustomPopupMenuTheme.height,
            onTap: () {
              controller.renameAction(entry, isLocal);
            },
          )
      ];
      if (items.isNotEmpty) {
        rightClickEntry.value = entry;
        final future = mod_menu.showMenu(
          context: context,
          position: secondaryPosition,
          items: items,
        );
        future.then((value) {
          rightClickEntry.value = null;
        });
        future.onError((error, stackTrace) {
          rightClickEntry.value = null;
        });
      }
    }

    onSecondaryTapDown(TapDownDetails details) {
      secondaryPosition = RelativeRect.fromLTRB(details.globalPosition.dx,
          details.globalPosition.dy, details.globalPosition.dx,
          details.globalPosition.dy);
    }

    return _EntryActions(onTap, onSecondaryTap, onSecondaryTapDown);
  }

  Widget _entryLeadingIcon(BuildContext context, Entry entry, {double? size}) {
    if (entry.isDrive) {
      return Image(
        image: iconHardDrive,
        fit: BoxFit.scaleDown,
        width: size,
        height: size,
        color: Theme.of(context).iconTheme.color?.withOpacity(0.7),
      ).paddingAll(4);
    }
    return SvgPicture.asset(
      entry.isFile ? "assets/file.svg" : "assets/folder.svg",
      width: size,
      height: size,
      colorFilter: svgColor(Theme.of(context).tabBarTheme.labelColor),
    );
  }

  Widget _buildListRow(BuildContext context, Entry entry,
      List<Entry> filteredEntries, Rx<Entry?> rightClickEntry, bool compact) {
    final actions =
        _entryActions(context, entry, filteredEntries, rightClickEntry);
    final sizeStr =
        entry.isFile ? readableFileSize(entry.size.toDouble()) : "";
    final lastModifiedStr = entry.isDrive
        ? " "
        : "${entry.lastModified().toString().replaceAll(".000", "")}   ";

    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 1),
      child: Obx(() {
        final isSel = selectedItems.items.contains(entry);
        final nameWidget = Row(children: [
          _entryLeadingIcon(context, entry),
          Expanded(
              child: Text(entry.name.nonBreaking,
                  style: TextStyle(color: isSel ? Colors.white : null),
                  overflow: TextOverflow.ellipsis))
        ]);
        return Container(
          decoration: BoxDecoration(
            color: isSel ? MyTheme.button : Theme.of(context).cardColor,
            borderRadius: BorderRadius.all(Radius.circular(5.0)),
            border: rightClickEntry.value == entry
                ? Border.all(color: MyTheme.button, width: 1.0)
                : null,
          ),
          key: ValueKey(entry.name),
          height: kDesktopFileTransferRowHeight,
          child: GestureDetector(
            onSecondaryTap: actions.onSecondaryTap,
            onSecondaryTapDown: actions.onSecondaryTapDown,
            child: InkWell(
              onTap: actions.onTap,
              child: compact
                  ? Row(children: [
                      Expanded(
                          child: Tooltip(
                              waitDuration: const Duration(milliseconds: 500),
                              message: entry.name,
                              child: nameWidget)),
                      const SizedBox(width: 8),
                      Text(sizeStr,
                          style: TextStyle(
                              fontSize: 11,
                              color:
                                  isSel ? Colors.white70 : MyTheme.darkGray)),
                    ]).paddingSymmetric(horizontal: 4)
                  : Row(children: [
                      Obx(() => Container(
                          width: _nameColWidth.value,
                          child: Tooltip(
                              waitDuration: const Duration(milliseconds: 500),
                              message: entry.name,
                              child: nameWidget))),
                      const SizedBox(width: 2.0),
                      Obx(() => SizedBox(
                          width: _modifiedColWidth.value,
                          child: Tooltip(
                              waitDuration: const Duration(milliseconds: 500),
                              message: lastModifiedStr,
                              child: Text(lastModifiedStr,
                                  overflow: TextOverflow.ellipsis,
                                  style: TextStyle(
                                      fontSize: 12,
                                      color: isSel
                                          ? Colors.white70
                                          : MyTheme.darkGray))))),
                      const SizedBox(width: 2.0),
                      Expanded(
                          child: Text(sizeStr,
                              overflow: TextOverflow.ellipsis,
                              style: TextStyle(
                                  fontSize: 10,
                                  color: isSel
                                      ? Colors.white70
                                      : MyTheme.darkGray))),
                    ]),
            ),
          ),
        );
      }),
    );
  }

  Widget _buildGridTile(BuildContext context, Entry entry,
      List<Entry> filteredEntries, Rx<Entry?> rightClickEntry) {
    final actions =
        _entryActions(context, entry, filteredEntries, rightClickEntry);
    return Obx(() {
      final isSel = selectedItems.items.contains(entry);
      return GestureDetector(
        onSecondaryTap: actions.onSecondaryTap,
        onSecondaryTapDown: actions.onSecondaryTapDown,
        child: InkWell(
          borderRadius: BorderRadius.circular(8),
          onTap: actions.onTap,
          child: Container(
            decoration: BoxDecoration(
              color: isSel ? MyTheme.button : Theme.of(context).cardColor,
              borderRadius: BorderRadius.circular(8),
              border: rightClickEntry.value == entry
                  ? Border.all(color: MyTheme.button, width: 1.0)
                  : null,
            ),
            padding: const EdgeInsets.all(6),
            child: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Expanded(child: Center(child: _tileThumb(context, entry))),
                const SizedBox(height: 4),
                Tooltip(
                  waitDuration: const Duration(milliseconds: 500),
                  message: entry.name,
                  child: Text(
                    entry.name,
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    textAlign: TextAlign.center,
                    style: TextStyle(
                        fontSize: 11,
                        color: isSel ? Colors.white : null),
                  ),
                ),
                if (entry.isFile)
                  Text(
                    readableFileSize(entry.size.toDouble()),
                    style: TextStyle(
                        fontSize: 9,
                        color: isSel ? Colors.white70 : MyTheme.darkGray),
                  ),
              ],
            ),
          ),
        ),
      );
    });
  }

  Widget _tileThumb(BuildContext context, Entry entry) {
    if (entry.isDrive || entry.isDirectory) {
      return _entryLeadingIcon(context, entry, size: 44);
    }
    final info = fileTypeInfoOf(entry.name);
    if (info.kind == PreviewKind.image) {
      if (isLocal) {
        return ClipRRect(
          borderRadius: BorderRadius.circular(6),
          child: Image.file(
            File(entry.path),
            fit: BoxFit.cover,
            width: double.infinity,
            cacheWidth: 200,
            errorBuilder: (_, __, ___) =>
                Icon(info.icon, size: 40, color: info.color),
          ),
        );
      }
      // Remote image: fetch a cached copy on demand (size-capped, throttled).
      if (entry.size <= _remoteThumbSizeLimit) {
        return _RemoteImageThumb(
          key: ValueKey('thumb_${entry.path}_${entry.modifiedTime}'),
          controller: controller,
          entry: entry,
          info: info,
        );
      }
    }
    return Icon(info.icon, size: 40, color: info.color);
  }

  /// Renders a flat list/grid for a given [entries] set in the current mode.
  Widget _buildEntriesBody(
      BuildContext context,
      ScrollController scrollController,
      List<Entry> entries,
      Rx<Entry?> rightClickEntry,
      FileViewMode mode) {
    if (mode == FileViewMode.tiles) {
      return GridView.builder(
        controller: scrollController,
        padding: const EdgeInsets.all(8),
        gridDelegate: const SliverGridDelegateWithMaxCrossAxisExtent(
          maxCrossAxisExtent: 128,
          mainAxisExtent: 124,
          crossAxisSpacing: 8,
          mainAxisSpacing: 8,
        ),
        itemCount: entries.length,
        itemBuilder: (context, index) =>
            _buildGridTile(context, entries[index], entries, rightClickEntry),
      );
    }
    final compact = mode == FileViewMode.list;
    return ListView.builder(
      controller: scrollController,
      itemExtent: kDesktopFileTransferRowHeight,
      itemCount: entries.length,
      itemBuilder: (context, index) =>
          _buildListRow(context, entries[index], entries, rightClickEntry, compact),
    );
  }

  /// Renders entries grouped into dated sections (Today / Yesterday / per-date).
  Widget _buildGroupedBody(
      BuildContext context,
      ScrollController scrollController,
      List<Entry> filteredEntries,
      Rx<Entry?> rightClickEntry,
      FileViewMode mode,
      List<_DateGroup> groups) {
    // Visual order matches the grouped rendering, so range/shift selection and
    // keyboard navigation stay consistent with what the user sees.
    final ordered = groups.expand((g) => g.entries).toList(growable: false);
    if (groups.isEmpty) {
      return const SizedBox.shrink();
    }
    final slivers = <Widget>[];
    for (final g in groups) {
      final total = g.entries.length;
      final expanded = _expandedGroups.contains(g.label);
      final shownCount =
          expanded ? total : (total < _groupPreviewCount ? total : _groupPreviewCount);
      slivers.add(SliverToBoxAdapter(child: _groupHeader(g.label, total)));
      if (mode == FileViewMode.tiles) {
        slivers.add(SliverPadding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          sliver: SliverGrid(
            gridDelegate: const SliverGridDelegateWithMaxCrossAxisExtent(
              maxCrossAxisExtent: 128,
              mainAxisExtent: 124,
              crossAxisSpacing: 8,
              mainAxisSpacing: 8,
            ),
            delegate: SliverChildBuilderDelegate(
              (context, index) => _buildGridTile(
                  context, g.entries[index], ordered, rightClickEntry),
              childCount: shownCount,
            ),
          ),
        ));
      } else {
        final compact = mode == FileViewMode.list;
        slivers.add(SliverList(
          delegate: SliverChildBuilderDelegate(
            (context, index) => _buildListRow(
                context, g.entries[index], ordered, rightClickEntry, compact),
            childCount: shownCount,
          ),
        ));
      }
      if (total > _groupPreviewCount) {
        slivers.add(SliverToBoxAdapter(
          child: _buildSeeAllButton(g.label, total, expanded),
        ));
      }
    }
    return CustomScrollView(controller: scrollController, slivers: slivers);
  }

  Widget _buildFileList(
      BuildContext context, ScrollController scrollController) {
    final fd = controller.directory.value;
    final entries = fd.entries;
    Rx<Entry?> rightClickEntry = Rx(null);

    return ListSearchActionListener(
      node: _keyboardNode,
      buffer: _listSearchBuffer,
      onNext: (buffer) {
        debugPrint("searching next for $buffer");
        assert(buffer.length == 1);
        assert(selectedItems.items.length <= 1);
        var skipCount = 0;
        if (selectedItems.items.isNotEmpty) {
          final index = entries.indexOf(selectedItems.items.first);
          if (index < 0) {
            return;
          }
          skipCount = index + 1;
        }
        var searchResult = entries
            .skip(skipCount)
            .where((element) => element.name.toLowerCase().startsWith(buffer));
        if (searchResult.isEmpty) {
          // cannot find next, lets restart search from head
          debugPrint("restart search from head");
          searchResult = entries.where(
              (element) => element.name.toLowerCase().startsWith(buffer));
        }
        if (searchResult.isEmpty) {
          selectedItems.clear();
          return;
        }
        _jumpToEntry(isLocal, searchResult.first, scrollController,
            kDesktopFileTransferRowHeight);
      },
      onSearch: (buffer) {
        debugPrint("searching for $buffer");
        final selectedEntries = selectedItems;
        final searchResult = entries
            .where((element) => element.name.toLowerCase().startsWith(buffer));
        selectedEntries.clear();
        if (searchResult.isEmpty) {
          selectedItems.clear();
          return;
        }
        _jumpToEntry(isLocal, searchResult.first, scrollController,
            kDesktopFileTransferRowHeight);
      },
      child: Obx(() {
        final entries = controller.directory.value.entries;
        final typeFilter = _typeFilter.value;
        final filteredEntries = entries.where((element) {
          if (_searchText.isNotEmpty &&
              !element.name.contains(_searchText.value)) {
            return false;
          }
          return entryMatchesFilter(element, typeFilter);
        }).toList(growable: false);
        final mode = _viewMode.value;
        final grouped = _groupByDate.value;
        late final Widget body;
        if (grouped) {
          final groups = _dateGroups(filteredEntries);
          final bucket = _selectedDateBucket.value;
          _DateGroup? selectedGroup;
          if (bucket != null) {
            for (final g in groups) {
              if (g.day == bucket) {
                selectedGroup = g;
                break;
              }
            }
          }
          final listArea = selectedGroup != null
              ? _buildEntriesBody(context, scrollController,
                  selectedGroup.entries, rightClickEntry, mode)
              : _buildGroupedBody(context, scrollController, filteredEntries,
                  rightClickEntry, mode, groups);
          body = Row(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              _buildDateSidebar(groups, filteredEntries.length),
              const VerticalDivider(width: 1),
              Expanded(child: listArea),
            ],
          );
        } else {
          body = _buildEntriesBody(
              context, scrollController, filteredEntries, rightClickEntry, mode);
        }

        return Column(
          children: [
            Align(
              alignment: Alignment.centerLeft,
              child: _buildGroupByDateToggle(),
            ),
            if (mode != FileViewMode.tiles && !grouped)
              Row(
                children: [
                  Expanded(child: _buildFileBrowserHeader(context)),
                ],
              ),
            Expanded(child: body),
          ],
        );
      }),
    );
  }

  onSearchText(String searchText, bool isLocal) {
    selectedItems.clear();
    _searchText.value = searchText;
  }

  void _jumpToEntry(bool isLocal, Entry entry,
      ScrollController scrollController, double rowHeight) {
    final entries = controller.directory.value.entries;
    final index = entries.indexOf(entry);
    if (index == -1) {
      debugPrint("entry is not valid: ${entry.path}");
    }
    final selectedEntries = selectedItems;
    final searchResult = entries.where((element) => element == entry);
    selectedEntries.clear();
    if (searchResult.isEmpty) {
      return;
    }
    final offset = min(
        max(scrollController.position.minScrollExtent,
            entries.indexOf(searchResult.first) * rowHeight),
        scrollController.position.maxScrollExtent);
    scrollController.jumpTo(offset);
    selectedEntries.add(searchResult.first);
    debugPrint("focused on ${searchResult.first.name}");
  }

  void _onSelectedChanged(SelectedItems selectedItems, List<Entry> entries,
      Entry entry, bool isLocal) {
    final isCtrlDown = RawKeyboard.instance.keysPressed
            .contains(LogicalKeyboardKey.controlLeft) ||
        RawKeyboard.instance.keysPressed
            .contains(LogicalKeyboardKey.controlRight);
    final isShiftDown = RawKeyboard.instance.keysPressed
            .contains(LogicalKeyboardKey.shiftLeft) ||
        RawKeyboard.instance.keysPressed
            .contains(LogicalKeyboardKey.shiftRight);
    if (isCtrlDown) {
      if (selectedItems.items.contains(entry)) {
        selectedItems.remove(entry);
      } else {
        selectedItems.add(entry);
      }
    } else if (isShiftDown) {
      final List<int> indexGroup = [];
      for (var selected in selectedItems.items) {
        indexGroup.add(entries.indexOf(selected));
      }
      indexGroup.add(entries.indexOf(entry));
      indexGroup.removeWhere((e) => e == -1);
      final maxIndex = indexGroup.reduce(max);
      final minIndex = indexGroup.reduce(min);
      selectedItems.clear();
      entries
          .getRange(minIndex, maxIndex + 1)
          .forEach((e) => selectedItems.add(e));
    } else {
      selectedItems.clear();
      selectedItems.add(entry);
    }
    _updatePreviewTarget(selectedItems, isLocal);
    setState(() {});
  }

  void _updatePreviewTarget(SelectedItems selectedItems, bool isLocal) {
    final files = selectedItems.items.where((e) => e.isFile).toList();
    if (files.length == 1) {
      _ffi.fileModel.previewTarget.value =
          PreviewTarget(isLocal, files.first);
    } else {
      final current = _ffi.fileModel.previewTarget.value;
      // Only clear if the cleared side owned the current preview.
      if (current != null && current.isLocal == isLocal) {
        _ffi.fileModel.previewTarget.value = null;
      }
    }
  }

  bool _checkDoubleClick(Entry entry) {
    final current = DateTime.now().millisecondsSinceEpoch;
    final elapsed = current - _lastClickTime;
    _lastClickTime = current;
    if (_lastClickEntry == entry) {
      if (elapsed < bind.getDoubleClickTime()) {
        return true;
      }
    } else {
      _lastClickEntry = entry;
    }
    return false;
  }

  void _onDrag(double dx, RxDouble column1, RxDouble column2) {
    if (column1.value + dx <= _fileTransferMinimumWidth ||
        column2.value - dx <= _fileTransferMinimumWidth) {
      return;
    }
    column1.value += dx;
    column2.value -= dx;
    column1.value = max(_fileTransferMinimumWidth, column1.value);
    column2.value = max(_fileTransferMinimumWidth, column2.value);
  }

  Widget _buildFileBrowserHeader(BuildContext context) {
    final padding = EdgeInsets.all(1.0);
    return SizedBox(
      key: _globalHeaderKey,
      height: kDesktopFileTransferHeaderHeight,
      child: Row(
        children: [
          Obx(
            () => headerItemFunc(
                _nameColWidth.value, SortBy.name, translate("Name")),
          ),
          DraggableDivider(
            axis: Axis.vertical,
            onPointerMove: (dx) =>
                _onDrag(dx, _nameColWidth, _modifiedColWidth),
            padding: padding,
          ),
          Obx(
            () => headerItemFunc(_modifiedColWidth.value, SortBy.modified,
                translate("Modified")),
          ),
          DraggableDivider(
              axis: Axis.vertical,
              onPointerMove: (dx) =>
                  _onDrag(dx, _modifiedColWidth, _sizeColWidth),
              padding: padding),
          Expanded(
              child: headerItemFunc(
                  _sizeColWidth.value, SortBy.size, translate("Size")))
        ],
      ),
    );
  }

  Widget headerItemFunc(double? width, SortBy sortBy, String name) {
    final headerTextStyle =
        Theme.of(context).dataTableTheme.headingTextStyle ?? TextStyle();
    return ObxValue<Rx<bool?>>(
        (ascending) => InkWell(
              onTap: () {
                if (ascending.value == null) {
                  ascending.value = true;
                } else {
                  ascending.value = !ascending.value!;
                }
                controller.changeSortStyle(sortBy,
                    isLocal: isLocal, ascending: ascending.value!);
              },
              child: SizedBox(
                width: width,
                height: kDesktopFileTransferHeaderHeight,
                child: Row(
                  children: [
                    Expanded(
                      child: Text(
                        name,
                        style: headerTextStyle,
                        overflow: TextOverflow.ellipsis,
                      ).marginOnly(left: 4),
                    ),
                    ascending.value != null
                        ? Icon(
                            ascending.value!
                                ? Icons.keyboard_arrow_up_rounded
                                : Icons.keyboard_arrow_down_rounded,
                          )
                        : SizedBox()
                  ],
                ),
              ),
            ), () {
      if (controller.sortBy.value == sortBy) {
        return controller.sortAscending.obs;
      } else {
        return Rx<bool?>(null);
      }
    }());
  }

  Widget buildBread() {
    final items = getPathBreadCrumbItems(isLocal, (list) {
      var path = "";
      for (var item in list) {
        path = PathUtil.join(path, item, controller.options.value.isWindows);
      }
      controller.openDirectory(path);
    });

    return items.isEmpty
        ? Offstage()
        : Row(
            key: _locationBarKey,
            mainAxisAlignment: MainAxisAlignment.spaceBetween,
            children: [
                Expanded(
                  child: Listener(
                    // handle mouse wheel
                    onPointerSignal: (e) {
                      if (e is PointerScrollEvent) {
                        final sc = _breadCrumbScroller;
                        final scale = isWindows ? 2 : 4;
                        sc.jumpTo(sc.offset + e.scrollDelta.dy / scale);
                      }
                    },
                    child: BreadCrumb(
                      items: items,
                      divider: const Icon(Icons.keyboard_arrow_right_rounded),
                      overflow: ScrollableOverflow(
                        controller: _breadCrumbScroller,
                      ),
                    ),
                  ),
                ),
                ActionIcon(
                  message: "",
                  icon: Icons.keyboard_arrow_down_rounded,
                  onTap: () async {
                    final renderBox = _locationBarKey.currentContext
                        ?.findRenderObject() as RenderBox;
                    _locationBarKey.currentContext?.size;

                    final size = renderBox.size;
                    final offset = renderBox.localToGlobal(Offset.zero);

                    final x = offset.dx;
                    final y = offset.dy + size.height + 1;

                    final isPeerWindows = controller.options.value.isWindows;
                    final List<MenuEntryBase> menuItems = [
                      MenuEntryButton(
                          childBuilder: (TextStyle? style) => isPeerWindows
                              ? buildWindowsThisPC(context, style)
                              : Text(
                                  '/',
                                  style: style,
                                ),
                          proc: () {
                            controller.openDirectory('/');
                          },
                          dismissOnClicked: true),
                      MenuEntryDivider()
                    ];
                    if (isPeerWindows) {
                      var loadingTag = "";
                      if (!isLocal) {
                        loadingTag = _ffi.dialogManager.showLoading("Waiting");
                      }
                      try {
                        final showHidden = controller.options.value.showHidden;
                        final fd = await controller.fileFetcher
                            .fetchDirectory("/", isLocal, showHidden);
                        for (var entry in fd.entries) {
                          menuItems.add(MenuEntryButton(
                              childBuilder: (TextStyle? style) =>
                                  Row(children: [
                                    Image(
                                        image: iconHardDrive,
                                        fit: BoxFit.scaleDown,
                                        color: Theme.of(context)
                                            .iconTheme
                                            .color
                                            ?.withOpacity(0.7)),
                                    SizedBox(width: 10),
                                    Text(
                                      entry.name,
                                      style: style,
                                    )
                                  ]),
                              proc: () {
                                controller.openDirectory('${entry.name}\\');
                              },
                              dismissOnClicked: true));
                        }
                        menuItems.add(MenuEntryDivider());
                      } catch (e) {
                        debugPrint("buildBread fetchDirectory err=$e");
                      } finally {
                        if (!isLocal) {
                          _ffi.dialogManager.dismissByTag(loadingTag);
                        }
                      }
                    }
                    mod_menu.showMenu(
                        context: context,
                        position: RelativeRect.fromLTRB(x, y, x, y),
                        elevation: 4,
                        items: menuItems
                            .map((e) => e.build(
                                context,
                                MenuConfig(
                                    commonColor:
                                        CustomPopupMenuTheme.commonColor,
                                    height: CustomPopupMenuTheme.height,
                                    dividerHeight:
                                        CustomPopupMenuTheme.dividerHeight,
                                    boxWidth: size.width)))
                            .expand((i) => i)
                            .toList());
                  },
                  iconSize: 20,
                )
              ]);
  }

  List<BreadCrumbItem> getPathBreadCrumbItems(
      bool isLocal, void Function(List<String>) onPressed) {
    final path = controller.directory.value.path;
    final breadCrumbList = List<BreadCrumbItem>.empty(growable: true);
    final isWindows = controller.options.value.isWindows;
    if (isWindows && path == '/') {
      breadCrumbList.add(BreadCrumbItem(
          content: TextButton(
                  child: buildWindowsThisPC(context),
                  style: ButtonStyle(
                      minimumSize: MaterialStateProperty.all(Size(0, 0))),
                  onPressed: () => onPressed(['/']))
              .marginSymmetric(horizontal: 4)));
    } else {
      final list = PathUtil.split(path, isWindows);
      breadCrumbList.addAll(
        list.asMap().entries.map(
              (e) => BreadCrumbItem(
                content: TextButton(
                  child: Text(e.value),
                  style: ButtonStyle(
                    minimumSize: MaterialStateProperty.all(
                      Size(0, 0),
                    ),
                  ),
                  onPressed: () => onPressed(
                    list.sublist(0, e.key + 1),
                  ),
                ).marginSymmetric(horizontal: 4),
              ),
            ),
      );
    }
    return breadCrumbList;
  }

  breadCrumbScrollToEnd() {
    Future.delayed(Duration(milliseconds: 200), () {
      if (_breadCrumbScroller.hasClients) {
        _breadCrumbScroller.animateTo(
            _breadCrumbScroller.position.maxScrollExtent,
            duration: Duration(milliseconds: 200),
            curve: Curves.fastLinearToSlowEaseIn);
      }
    });
  }

  Widget buildPathLocation() {
    final text = _locationStatus.value == LocationStatus.pathLocation
        ? controller.directory.value.path
        : _searchText.value;
    final textController = TextEditingController(text: text)
      ..selection = TextSelection.collapsed(offset: text.length);
    return Row(
      children: [
        SvgPicture.asset(
          _locationStatus.value == LocationStatus.pathLocation
              ? "assets/folder.svg"
              : "assets/search.svg",
          colorFilter: svgColor(Theme.of(context).tabBarTheme.labelColor),
        ),
        Expanded(
          child: TextField(
            focusNode: _locationNode,
            decoration: InputDecoration(
              border: InputBorder.none,
              isDense: true,
              prefix: Padding(
                padding: EdgeInsets.only(left: 4.0),
              ),
            ),
            controller: textController,
            onSubmitted: (path) {
              controller.openDirectory(path);
            },
            onChanged: _locationStatus.value == LocationStatus.fileSearchBar
                ? (searchText) => onSearchText(searchText, isLocal)
                : null,
          ).workaroundFreezeLinuxMint(),
        )
      ],
    );
  }

  // openDirectory(String path, {bool isLocal = false}) {
  //   model.openDirectory(path, isLocal: isLocal);
  // }
}

Widget buildWindowsThisPC(BuildContext context, [TextStyle? textStyle]) {
  final color = Theme.of(context).iconTheme.color?.withOpacity(0.7);
  return Row(children: [
    Icon(Icons.computer, size: 20, color: color),
    SizedBox(width: 10),
    Text(translate('This PC'), style: textStyle)
  ]);
}

/// Grid thumbnail for a remote image: downloads a cached copy on demand
/// (throttled) and shows it, falling back to the type icon while loading or on
/// failure.
class _RemoteImageThumb extends StatefulWidget {
  final FileController controller;
  final Entry entry;
  final FileTypeInfo info;
  const _RemoteImageThumb({
    Key? key,
    required this.controller,
    required this.entry,
    required this.info,
  }) : super(key: key);

  @override
  State<_RemoteImageThumb> createState() => _RemoteImageThumbState();
}

class _RemoteImageThumbState extends State<_RemoteImageThumb> {
  String? _path;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    final path = await RemotePreviewCache.instance
        .fetchThumbnail(widget.controller, widget.entry);
    if (!mounted) return;
    if (path != null) setState(() => _path = path);
  }

  @override
  Widget build(BuildContext context) {
    final info = widget.info;
    if (_path == null) {
      return Icon(info.icon, size: 40, color: info.color);
    }
    return ClipRRect(
      borderRadius: BorderRadius.circular(6),
      child: Image.file(
        File(_path!),
        fit: BoxFit.cover,
        width: double.infinity,
        cacheWidth: 200,
        errorBuilder: (_, __, ___) =>
            Icon(info.icon, size: 40, color: info.color),
      ),
    );
  }
}

/// Shows local disk usage (free / used) with a small progress bar in the
/// status bar. Recomputes only when the watched directory path changes.
class _DiskUsageIndicator extends StatefulWidget {
  final FileController controller;
  const _DiskUsageIndicator({Key? key, required this.controller})
      : super(key: key);

  @override
  State<_DiskUsageIndicator> createState() => _DiskUsageIndicatorState();
}

class _DiskUsageIndicatorState extends State<_DiskUsageIndicator> {
  DiskUsage? _usage;
  String? _lastPath;
  StreamSubscription? _sub;

  @override
  void initState() {
    super.initState();
    _refresh();
    _sub = widget.controller.directory.listen((_) => _refresh());
  }

  @override
  void dispose() {
    _sub?.cancel();
    super.dispose();
  }

  Future<void> _refresh() async {
    final path = widget.controller.directory.value.path;
    if (path.isEmpty || path == _lastPath) return;
    _lastPath = path;
    final usage = await getDiskUsage(path);
    if (!mounted) return;
    setState(() => _usage = usage);
  }

  @override
  Widget build(BuildContext context) {
    final usage = _usage;
    if (usage == null) return const SizedBox.shrink();
    final labelColor = Theme.of(context).tabBarTheme.labelColor;
    final freeStr = readableFileSize(usage.free.toDouble());
    final usedStr = readableFileSize(usage.used.toDouble());
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text('$freeStr ${translate('free')} / $usedStr ${translate('used')}',
            style: TextStyle(fontSize: 12, color: labelColor)),
        const SizedBox(width: 8),
        SizedBox(
          width: 90,
          child: ClipRRect(
            borderRadius: BorderRadius.circular(4),
            child: LinearProgressIndicator(
              value: usage.usedFraction,
              minHeight: 6,
              backgroundColor: Theme.of(context).hoverColor,
              valueColor: AlwaysStoppedAnimation(MyTheme.accent),
            ),
          ),
        ),
      ],
    );
  }
}
