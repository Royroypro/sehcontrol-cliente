import 'dart:ui' as ui;

import 'package:bot_toast/bot_toast.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common/widgets/address_book.dart';
import 'package:flutter_hbb/common/widgets/dialog.dart';
import 'package:flutter_hbb/common/widgets/my_group.dart';
import 'package:flutter_hbb/common/widgets/peers_view.dart';
import 'package:flutter_hbb/common/widgets/peer_card.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/desktop/widgets/popup_menu.dart';
import 'package:flutter_hbb/desktop/widgets/material_mod_popup_menu.dart'
    as mod_menu;
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/models/ab_model.dart';
import 'package:flutter_hbb/models/peer_model.dart';

import 'package:flutter_hbb/models/peer_tab_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:get/get.dart';
import 'package:provider/provider.dart';
import 'package:pull_down_button/pull_down_button.dart';

import '../../common.dart';
import '../../models/platform_model.dart';

class PeerTabPage extends StatefulWidget {
  const PeerTabPage({Key? key}) : super(key: key);
  @override
  State<PeerTabPage> createState() => _PeerTabPageState();
}

class _TabEntry {
  final Widget widget;
  final Function({dynamic hint})? load;
  _TabEntry(this.widget, [this.load]);
}

EdgeInsets? _menuPadding() {
  return (isDesktop || isWebDesktop) ? kDesktopMenuPadding : null;
}

class _PeerTabPageState extends State<PeerTabPage>
    with SingleTickerProviderStateMixin {
  final List<_TabEntry> entries = [
    _TabEntry(RecentPeersView(
      menuPadding: _menuPadding(),
    )),
    _TabEntry(FavoritePeersView(
      menuPadding: _menuPadding(),
    )),
    _TabEntry(DiscoveredPeersView(
      menuPadding: _menuPadding(),
    )),
    _TabEntry(
        AddressBook(
          menuPadding: _menuPadding(),
        ),
        ({dynamic hint}) => gFFI.abModel.pullAb(
            force: hint == null ? ForcePullAb.listAndCurrent : null,
            quiet: false)),
    _TabEntry(
      MyGroup(
        menuPadding: _menuPadding(),
      ),
      ({dynamic hint}) => gFFI.groupModel.pull(force: hint == null),
    ),
  ];
  RelativeRect? mobileTabContextMenuPos;

  final isOptVisiableFixed = isOptionFixed(kOptionPeerTabVisible);

  _PeerTabPageState() {
    _loadLocalOptions();
  }

  void _loadLocalOptions() {
    final uiType = bind.getLocalFlutterOption(k: kOptionPeerCardUiType);
    if (uiType != '') {
      peerCardUiType.value = int.parse(uiType) == 0
          ? PeerUiType.grid
          : int.parse(uiType) == 1
              ? PeerUiType.tile
              : PeerUiType.list;
    }
    hideAbTagsPanel.value =
        bind.mainGetLocalOption(key: kOptionHideAbTagsPanel) == 'Y';
  }

  Future<void> handleTabSelection(int tabIndex) async {
    if (tabIndex < entries.length) {
      if (tabIndex != gFFI.peerTabModel.currentTab) {
        gFFI.peerTabModel.setCurrentTabCachedPeers([]);
      }
      gFFI.peerTabModel.setCurrentTab(tabIndex);
      entries[tabIndex].load?.call(hint: false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final model = Provider.of<PeerTabModel>(context);
    Widget selectionWrap(Widget widget) {
      return model.multiSelectionMode ? createMultiSelectionBar(model) : widget;
    }

    return Column(
      textBaseline: TextBaseline.ideographic,
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Obx(() => SizedBox(
              height: 32,
              child: Container(
                padding: stateGlobal.isPortrait.isTrue
                    ? EdgeInsets.symmetric(horizontal: 2)
                    : null,
                child: selectionWrap(Row(
                  crossAxisAlignment: CrossAxisAlignment.center,
                  children: [
                    Expanded(
                      child: stateGlobal.isPortrait.isTrue
                          ? SingleChildScrollView(
                              scrollDirection: Axis.horizontal,
                              child: Row(
                                children: _buildMobileNavigationItems(context),
                              ),
                            )
                          : visibleContextMenuListener(
                              _createSwitchBar(context)),
                    ),
                    if (stateGlobal.isPortrait.isFalse)
                      ..._buildReservedTagChips(context),
                    if (stateGlobal.isPortrait.isTrue) const SizedBox(width: 8),
                    if (stateGlobal.isPortrait.isTrue)
                      ..._portraitRightActions(context)
                    else
                      ..._landscapeRightActions(context)
                  ],
                )),
              ),
            ).paddingOnly(right: stateGlobal.isPortrait.isTrue ? 0 : 12)),
        _createPeersView(),
      ],
    );
  }

  /// Sehcontrol stores managed devices in the address book. Its server does
  /// not expose RustDesk Pro's device-group endpoints.
  String _tabLabel(PeerTabModel model, int t) {
    if (t == PeerTabIndex.recent.index) return translate('All');
    if (t == PeerTabIndex.ab.index) {
      return isMobile ? 'Equipos' : translate('My Devices');
    }
    return model.tabTooltip(t);
  }

  int _tabCount(int t) {
    if (t == PeerTabIndex.recent.index) {
      return gFFI.recentPeersModel.getPeersCount();
    }
    if (t == PeerTabIndex.fav.index)
      return gFFI.favoritePeersModel.getPeersCount();
    if (t == PeerTabIndex.lan.index) return gFFI.lanPeersModel.getPeersCount();
    if (t == PeerTabIndex.ab.index) return gFFI.abModel.currentAbPeers.length;
    if (t == PeerTabIndex.group.index) return gFFI.groupModel.peers.length;
    return 0;
  }

  /// "Cabinas"/"Clientes" are reserved Address Book tags (see
  /// docs/CLIENT_INTEGRATION.md addendum on this): tapping one switches to
  /// the Address Book tab pre-filtered to just that tag, reusing the
  /// existing tag-selection/filtering already implemented there — no new
  /// data model or server endpoint needed.
  static const List<String> reservedTagChips = ['Cabinas', 'Clientes'];

  List<Widget> _buildMobileNavigationItems(BuildContext context) {
    final model = Provider.of<PeerTabModel>(context);
    final items = <Widget>[];

    // Keep the managed-device sections visible first on narrow screens.
    // Secondary history/favorite tabs remain available by scrolling right.
    if (model.visibleEnabledOrderedIndexs.contains(PeerTabIndex.lan.index)) {
      items.add(_buildMobileTabChip(context, model, PeerTabIndex.lan.index));
    }
    items.add(_buildMobileTabChip(
      context,
      model,
      PeerTabIndex.ab.index,
    ));
    items.addAll(reservedTagChips
        .map((tag) => _buildMobileReservedTagChip(context, tag)));

    for (final tab in model.visibleEnabledOrderedIndexs) {
      if (tab == PeerTabIndex.ab.index || tab == PeerTabIndex.lan.index) {
        continue;
      }
      items.add(_buildMobileTabChip(context, model, tab));
    }
    return items;
  }

  Widget _buildMobileTabChip(
    BuildContext context,
    PeerTabModel model,
    int tab,
  ) {
    final selected = model.currentTab == tab &&
        (tab != PeerTabIndex.ab.index || gFFI.abModel.selectedTags.isEmpty);
    final isDark = Theme.of(context).brightness == Brightness.dark;
    final color = selected
        ? (isDark ? Colors.white : const Color(0xFF1565C0))
        : (isDark ? Colors.white70 : const Color(0xFF424242));
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 1),
      child: ChoiceChip(
        label: Text(
          '${_tabLabel(model, tab)} (${_tabCount(tab)})',
          style: TextStyle(color: color, fontSize: 12),
        ),
        labelPadding: EdgeInsets.zero,
        padding: EdgeInsets.zero,
        selected: selected,
        showCheckmark: false,
        visualDensity: const VisualDensity(horizontal: -3, vertical: -4),
        materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
        side: BorderSide.none,
        backgroundColor:
            isDark ? const Color(0xFF303030) : const Color(0xFFF0F1F4),
        selectedColor:
            isDark ? const Color(0xFF424242) : const Color(0xFFE3F2FD),
        onSelected: (_) async {
          if (tab == PeerTabIndex.ab.index) {
            gFFI.abModel.selectedTags.clear();
          }
          await handleTabSelection(tab);
          await bind.setLocalFlutterOption(
              k: kOptionPeerTabIndex, v: tab.toString());
        },
      ),
    );
  }

  Widget _buildMobileReservedTagChip(BuildContext context, String tag) {
    return Obx(() {
      final count = gFFI.abModel.currentAbPeers
          .where((p) => p.tags.contains(tag))
          .length;
      final selected = gFFI.peerTabModel.currentTab == PeerTabIndex.ab.index &&
          gFFI.abModel.selectedTags.length == 1 &&
          gFFI.abModel.selectedTags.first == tag;
      final isDark = Theme.of(context).brightness == Brightness.dark;
      final color = selected
          ? (isDark ? Colors.white : const Color(0xFF1565C0))
          : (isDark ? Colors.white70 : const Color(0xFF424242));
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 1),
        child: ChoiceChip(
          label:
              Text('$tag ($count)', style: TextStyle(color: color, fontSize: 12)),
          labelPadding: EdgeInsets.zero,
          padding: EdgeInsets.zero,
          selected: selected,
          showCheckmark: false,
          visualDensity: const VisualDensity(horizontal: -3, vertical: -4),
          materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
          side: BorderSide.none,
          backgroundColor:
              isDark ? const Color(0xFF303030) : const Color(0xFFF0F1F4),
          selectedColor:
              isDark ? const Color(0xFF424242) : const Color(0xFFE3F2FD),
          onSelected: (_) async {
            gFFI.abModel.selectedTags
              ..clear()
              ..add(tag);
            await handleTabSelection(PeerTabIndex.ab.index);
          },
        ),
      );
    });
  }

  List<Widget> _buildReservedTagChips(BuildContext context) {
    return reservedTagChips.map((tag) {
      return Obx(() {
        final count = gFFI.abModel.currentAbPeers
            .where((p) => p.tags.contains(tag))
            .length;
        final selected =
            gFFI.peerTabModel.currentTab == PeerTabIndex.ab.index &&
                gFFI.abModel.selectedTags.length == 1 &&
                gFFI.abModel.selectedTags.first == tag;
        final isDark = Theme.of(context).brightness == Brightness.dark;
        final color = selected
            ? (isDark ? Colors.white : const Color(0xFF1565C0))
            : (isDark ? Colors.white70 : const Color(0xFF424242));
        return InkWell(
          onTap: () async {
            gFFI.abModel.selectedTags
              ..clear()
              ..add(tag);
            await handleTabSelection(PeerTabIndex.ab.index);
          },
          child: Container(
            decoration: selected
                ? BoxDecoration(
                    border: Border(bottom: BorderSide(width: 2, color: color)))
                : null,
            child: Text('$tag ($count)',
                    style: TextStyle(color: color, fontSize: 13))
                .paddingSymmetric(horizontal: 8),
          ),
        );
      });
    }).toList();
  }

  Widget _createSwitchBar(BuildContext context) {
    final model = Provider.of<PeerTabModel>(context);
    var counter = -1;
    return ReorderableListView(
        buildDefaultDragHandles: false,
        onReorder: model.reorder,
        scrollDirection: Axis.horizontal,
        physics: NeverScrollableScrollPhysics(),
        children: model.visibleEnabledOrderedIndexs.map((t) {
          final hover = false.obs;
          final deco = BoxDecoration(
              color: Theme.of(context).colorScheme.background,
              borderRadius: BorderRadius.circular(6));
          counter += 1;
          return ReorderableDragStartListener(
              key: ValueKey(t),
              index: counter,
              child: Obx(() {
                final selected = model.currentTab == t &&
                    (t != PeerTabIndex.ab.index ||
                        gFFI.abModel.selectedTags.isEmpty);
                final color = selected
                    ? MyTheme.tabbar(context).selectedTextColor
                    : MyTheme.tabbar(context).unSelectedTextColor
                  ?..withOpacity(0.5);
                final decoBorder = BoxDecoration(
                    border: Border(
                  bottom: BorderSide(width: 2, color: color!),
                ));
                return Tooltip(
                  preferBelow: false,
                  message: model.tabTooltip(t),
                  onTriggered: isMobile ? mobileShowTabVisibilityMenu : null,
                  child: InkWell(
                    child: Container(
                      decoration: (hover.value
                          ? (selected ? decoBorder : deco)
                          : (selected ? decoBorder : null)),
                      child: Row(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          Icon(model.tabIcon(t), color: color, size: 16),
                          const SizedBox(width: 6),
                          Text('${_tabLabel(model, t)} (${_tabCount(t)})',
                              style: TextStyle(color: color, fontSize: 13)),
                        ],
                      ).paddingSymmetric(horizontal: 4),
                    ).paddingSymmetric(horizontal: 4),
                    onTap: isOptionFixed(kOptionPeerTabIndex)
                        ? null
                        : () async {
                            // "Equipos" represents the complete set of
                            // devices associated with the signed-in
                            // account. Clear reserved Cabinas/Clientes tag
                            // filters when returning to this tab.
                            if (t == PeerTabIndex.ab.index) {
                              gFFI.abModel.selectedTags.clear();
                            }
                            await handleTabSelection(t);
                            await bind.setLocalFlutterOption(
                                k: kOptionPeerTabIndex, v: t.toString());
                          },
                    onHover: (value) => hover.value = value,
                  ),
                );
              }));
        }).toList());
  }

  Widget _createPeersView() {
    final model = Provider.of<PeerTabModel>(context);
    Widget child;
    if (model.visibleEnabledOrderedIndexs.isEmpty) {
      child = visibleContextMenuListener(Row(
        children: [Expanded(child: InkWell())],
      ));
    } else {
      if (model.visibleEnabledOrderedIndexs.contains(model.currentTab)) {
        child = entries[model.currentTab].widget;
      } else {
        debugPrint("should not happen! currentTab not in visibleIndexs");
        Future.delayed(Duration.zero, () {
          model.setCurrentTab(model.visibleEnabledOrderedIndexs[0]);
        });
        child = entries[0].widget;
      }
    }
    return Expanded(
        child: child.marginSymmetric(
            vertical: (isDesktop || isWebDesktop) ? 12.0 : 6.0));
  }

  Widget _createRefresh(
      {required PeerTabIndex index, required RxBool loading}) {
    final model = Provider.of<PeerTabModel>(context);
    final textColor = Theme.of(context).textTheme.titleLarge?.color;
    return Offstage(
      offstage: model.currentTab != index.index,
      child: Tooltip(
        message: translate('Refresh'),
        child: RefreshWidget(
            onPressed: () {
              if (gFFI.peerTabModel.currentTab < entries.length) {
                entries[gFFI.peerTabModel.currentTab].load?.call();
              }
            },
            spinning: loading,
            child: RotatedBox(
                quarterTurns: 2,
                child: Icon(
                  Icons.refresh,
                  size: 18,
                  color: textColor,
                ))),
      ),
    );
  }

  Widget _createPeerViewTypeSwitch(BuildContext context) {
    return PeerViewDropdown();
  }

  Widget _createMultiSelection() {
    final textColor = Theme.of(context).textTheme.titleLarge?.color;
    final model = Provider.of<PeerTabModel>(context);
    return _hoverAction(
      toolTip: translate('Select'),
      context: context,
      onTap: () {
        model.setMultiSelectionMode(true);
        if (isMobile && Navigator.canPop(context)) {
          Navigator.pop(context);
        }
      },
      child: SvgPicture.asset(
        "assets/checkbox-outline.svg",
        width: 18,
        height: 18,
        colorFilter: svgColor(textColor),
      ),
    );
  }

  void mobileShowTabVisibilityMenu() {
    final model = gFFI.peerTabModel;
    final items = List<PopupMenuItem>.empty(growable: true);
    for (int i = 0; i < PeerTabModel.maxTabCount; i++) {
      if (!model.isEnabled[i]) continue;
      items.add(PopupMenuItem(
        height: kMinInteractiveDimension * 0.8,
        onTap: isOptVisiableFixed
            ? null
            : () => model.setTabVisible(i, !model.isVisibleEnabled[i]),
        enabled: !isOptVisiableFixed,
        child: Row(
          children: [
            Checkbox(
                value: model.isVisibleEnabled[i],
                onChanged: isOptVisiableFixed
                    ? null
                    : (_) {
                        model.setTabVisible(i, !model.isVisibleEnabled[i]);
                        if (Navigator.canPop(context)) {
                          Navigator.pop(context);
                        }
                      }),
            Expanded(child: Text(model.tabTooltip(i))),
          ],
        ),
      ));
    }
    if (mobileTabContextMenuPos != null) {
      showMenu(
          context: context, position: mobileTabContextMenuPos!, items: items);
    }
  }

  Widget visibleContextMenuListener(Widget child) {
    if (!(isDesktop || isWebDesktop)) {
      return GestureDetector(
        onLongPressDown: (e) {
          final x = e.globalPosition.dx;
          final y = e.globalPosition.dy;
          mobileTabContextMenuPos = RelativeRect.fromLTRB(x, y, x, y);
        },
        onLongPressUp: () {
          mobileShowTabVisibilityMenu();
        },
        child: child,
      );
    } else {
      return Listener(
          onPointerDown: (e) {
            if (e.kind != ui.PointerDeviceKind.mouse) {
              return;
            }
            if (e.buttons == 2) {
              showRightMenu(
                (CancelFunc cancelFunc) {
                  return visibleContextMenu(cancelFunc);
                },
                target: e.position,
              );
            }
          },
          child: child);
    }
  }

  Widget visibleContextMenu(CancelFunc cancelFunc) {
    final model = Provider.of<PeerTabModel>(context);
    final menu = List<MenuEntrySwitchSync>.empty(growable: true);
    for (int i = 0; i < model.orders.length; i++) {
      int tabIndex = model.orders[i];
      if (tabIndex < 0 || tabIndex >= PeerTabModel.maxTabCount) continue;
      if (!model.isEnabled[tabIndex]) continue;
      menu.add(MenuEntrySwitchSync(
          switchType: SwitchType.scheckbox,
          text: model.tabTooltip(tabIndex),
          currentValue: model.isVisibleEnabled[tabIndex],
          setter: (show) async {
            model.setTabVisible(tabIndex, show);
            // Do not hide the current menu (checkbox)
            // cancelFunc();
          },
          enabled: (!isOptVisiableFixed).obs));
    }
    return mod_menu.PopupMenu(
        items: menu
            .map((entry) => entry.build(
                context,
                const MenuConfig(
                  commonColor: MyTheme.accent,
                  height: 20.0,
                  dividerHeight: 12.0,
                )))
            .expand((i) => i)
            .toList());
  }

  Widget createMultiSelectionBar(PeerTabModel model) {
    return Row(
      mainAxisAlignment: MainAxisAlignment.spaceBetween,
      children: [
        Offstage(
          offstage: model.selectedPeers.isEmpty,
          child: Row(
            children: [
              deleteSelection(),
              addSelectionToFav(),
              addSelectionToAb(),
              editSelectionTags(),
            ],
          ),
        ),
        Row(
          children: [
            selectionCount(model.selectedPeers.length),
            selectAll(model),
            closeSelection(),
          ],
        )
      ],
    );
  }

  Widget deleteSelection() {
    final model = Provider.of<PeerTabModel>(context);
    if (model.currentTab == PeerTabIndex.group.index) {
      return Offstage();
    }
    return _hoverAction(
        context: context,
        toolTip: translate('Delete'),
        onTap: () {
          onSubmit() async {
            final peers = model.selectedPeers;
            switch (model.currentTab) {
              case 0:
                for (var p in peers) {
                  await bind.mainRemovePeer(id: p.id);
                }
                bind.mainLoadRecentPeers();
                break;
              case 1:
                final favs = (await bind.mainGetFav()).toList();
                peers.map((p) {
                  favs.remove(p.id);
                }).toList();
                await bind.mainStoreFav(favs: favs);
                bind.mainLoadFavPeers();
                break;
              case 2:
                for (var p in peers) {
                  await bind.mainRemoveDiscovered(id: p.id);
                }
                bind.mainLoadLanPeers();
                break;
              case 3:
                await gFFI.abModel.deletePeers(peers.map((p) => p.id).toList());
                break;
              default:
                break;
            }
            gFFI.peerTabModel.setMultiSelectionMode(false);
            if (model.currentTab != 3) showToast(translate('Successful'));
          }

          deleteConfirmDialog(onSubmit, translate('Delete'));
        },
        child: Icon(Icons.delete, color: Colors.red));
  }

  Widget addSelectionToFav() {
    final model = Provider.of<PeerTabModel>(context);
    return Offstage(
      offstage:
          model.currentTab != PeerTabIndex.recent.index, // show based on recent
      child: _hoverAction(
        context: context,
        toolTip: translate('Add to Favorites'),
        onTap: () async {
          final peers = model.selectedPeers;
          final favs = (await bind.mainGetFav()).toList();
          for (var p in peers) {
            if (!favs.contains(p.id)) {
              favs.add(p.id);
            }
          }
          await bind.mainStoreFav(favs: favs);
          model.setMultiSelectionMode(false);
          showToast(translate('Successful'));
        },
        child: Icon(PeerTabModel.icons[PeerTabIndex.fav.index]),
      ).marginOnly(left: !(isDesktop || isWebDesktop) ? 11 : 6),
    );
  }

  Widget addSelectionToAb() {
    final model = Provider.of<PeerTabModel>(context);
    final addressbooks = gFFI.abModel.addressBooksCanWrite();
    if (model.currentTab == PeerTabIndex.ab.index) {
      addressbooks.remove(gFFI.abModel.currentName.value);
    }
    return Offstage(
      offstage: !gFFI.userModel.isLogin || addressbooks.isEmpty,
      child: _hoverAction(
        context: context,
        toolTip: translate('Add to address book'),
        onTap: () {
          final peers = model.selectedPeers.map((e) => Peer.copy(e)).toList();
          addPeersToAbDialog(peers);
          model.setMultiSelectionMode(false);
        },
        child: Icon(PeerTabModel.icons[PeerTabIndex.ab.index]),
      ).marginOnly(left: !(isDesktop || isWebDesktop) ? 11 : 6),
    );
  }

  Widget editSelectionTags() {
    final model = Provider.of<PeerTabModel>(context);
    return Offstage(
      offstage: !gFFI.userModel.isLogin ||
          model.currentTab != PeerTabIndex.ab.index ||
          gFFI.abModel.currentAbTags.isEmpty,
      child: _hoverAction(
              context: context,
              toolTip: translate('Edit Tag'),
              onTap: () {
                editAbTagDialog(List.empty(), (selectedTags) async {
                  final peers = model.selectedPeers;
                  await gFFI.abModel.changeTagForPeers(
                      peers.map((p) => p.id).toList(), selectedTags);
                  model.setMultiSelectionMode(false);
                  showToast(translate('Successful'));
                });
              },
              child: Icon(Icons.tag))
          .marginOnly(left: !(isDesktop || isWebDesktop) ? 11 : 6),
    );
  }

  Widget selectionCount(int count) {
    return Align(
      alignment: Alignment.center,
      child: Text('$count ${translate('Selected')}'),
    );
  }

  Widget selectAll(PeerTabModel model) {
    return Offstage(
      offstage:
          model.selectedPeers.length >= model.currentTabCachedPeers.length,
      child: _hoverAction(
        context: context,
        toolTip: translate('Select All'),
        onTap: () {
          model.selectAll();
        },
        child: Icon(Icons.select_all),
      ).marginOnly(left: 6),
    );
  }

  Widget closeSelection() {
    final model = Provider.of<PeerTabModel>(context);
    return _hoverAction(
            context: context,
            toolTip: translate('Close'),
            onTap: () {
              model.setMultiSelectionMode(false);
            },
            child: Icon(Icons.clear))
        .marginOnly(left: 6);
  }

  Widget _toggleTags() {
    return _hoverAction(
        context: context,
        toolTip: translate('Toggle Tags'),
        hoverableWhenfalse: hideAbTagsPanel,
        child: Icon(
          Icons.tag_rounded,
          size: 18,
        ),
        onTap: () async {
          await bind.mainSetLocalOption(
              key: kOptionHideAbTagsPanel,
              value: hideAbTagsPanel.value ? defaultOptionNo : "Y");
          hideAbTagsPanel.value = !hideAbTagsPanel.value;
        });
  }

  List<Widget> _landscapeRightActions(BuildContext context) {
    final model = Provider.of<PeerTabModel>(context);
    return [
      const PeerSearchBar().marginOnly(right: 13),
      _createRefresh(
          index: PeerTabIndex.ab, loading: gFFI.abModel.currentAbLoading),
      _createRefresh(
          index: PeerTabIndex.group, loading: gFFI.groupModel.groupLoading),
      Offstage(
        offstage: model.currentTabCachedPeers.isEmpty,
        child: _createMultiSelection(),
      ),
      _createPeerViewTypeSwitch(context),
      Offstage(
        offstage: model.currentTab == PeerTabIndex.recent.index,
        child: PeerSortDropdown(),
      ),
      Offstage(
        offstage: model.currentTab != PeerTabIndex.ab.index,
        child: _toggleTags(),
      ),
    ];
  }

  List<Widget> _portraitRightActions(BuildContext context) {
    final model = Provider.of<PeerTabModel>(context);
    final screenWidth = MediaQuery.of(context).size.width;
    final leftIconSize = Theme.of(context).iconTheme.size ?? 24;
    final leftActionsSize =
        (leftIconSize + (4 + 4) * 2) * model.visibleEnabledOrderedIndexs.length;
    final availableWidth = screenWidth - 10 * 2 - leftActionsSize - 2 * 2;
    final searchWidth = 120;
    final otherActionWidth = 18 + 10;

    dropDown(List<Widget> menus) {
      final padding = 6.0;
      final textColor = Theme.of(context).textTheme.titleLarge?.color;
      return PullDownButton(
        buttonBuilder:
            (BuildContext context, Future<void> Function() showMenu) {
          return _hoverAction(
            context: context,
            toolTip: translate('More'),
            child: SvgPicture.asset(
              "assets/chevron_up_chevron_down.svg",
              width: 18,
              height: 18,
              colorFilter: svgColor(textColor),
            ),
            onTap: showMenu,
          );
        },
        routeTheme: PullDownMenuRouteTheme(
            width: menus.length * (otherActionWidth + padding * 2) * 1.0),
        itemBuilder: (context) => [
          PullDownMenuEntryImpl(
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: menus
                  .map((e) =>
                      Material(child: e.paddingSymmetric(horizontal: padding)))
                  .toList(),
            ),
          )
        ],
      );
    }

    // Always show search, refresh
    List<Widget> actions = [
      const PeerSearchBar(),
      if (model.currentTab == PeerTabIndex.ab.index)
        _createRefresh(
            index: PeerTabIndex.ab, loading: gFFI.abModel.currentAbLoading),
      if (model.currentTab == PeerTabIndex.group.index)
        _createRefresh(
            index: PeerTabIndex.group, loading: gFFI.groupModel.groupLoading),
    ];
    final List<Widget> dynamicActions = [
      if (model.currentTabCachedPeers.isNotEmpty) _createMultiSelection(),
      if (model.currentTab != PeerTabIndex.recent.index) PeerSortDropdown(),
      if (model.currentTab == PeerTabIndex.ab.index) _toggleTags()
    ];
    final rightWidth = availableWidth -
        searchWidth -
        (actions.length == 2 ? otherActionWidth : 0);
    final availablePositions = rightWidth ~/ otherActionWidth;

    if (availablePositions < dynamicActions.length &&
        dynamicActions.length > 1) {
      if (availablePositions < 2) {
        actions.addAll([
          dropDown(dynamicActions),
        ]);
      } else {
        actions.addAll([
          ...dynamicActions.sublist(0, availablePositions - 1),
          dropDown(dynamicActions.sublist(availablePositions - 1)),
        ]);
      }
    } else {
      actions.addAll(dynamicActions);
    }
    return actions;
  }
}

class PeerSearchBar extends StatefulWidget {
  const PeerSearchBar({Key? key}) : super(key: key);

  @override
  State<StatefulWidget> createState() => _PeerSearchBarState();
}

class _PeerSearchBarState extends State<PeerSearchBar> {
  var drawer = false;

  @override
  Widget build(BuildContext context) {
    return drawer
        ? _buildSearchBar()
        : _hoverAction(
            context: context,
            toolTip: translate('Search'),
            padding: const EdgeInsets.only(right: 2),
            onTap: () {
              setState(() {
                drawer = true;
              });
            },
            child: Icon(
              Icons.search_rounded,
              color: Theme.of(context).hintColor,
            ));
  }

  Widget _buildSearchBar() {
    RxBool focused = false.obs;
    FocusNode focusNode = FocusNode();
    focusNode.addListener(() {
      focused.value = focusNode.hasFocus;
      peerSearchTextController.selection = TextSelection(
          baseOffset: 0,
          extentOffset: peerSearchTextController.value.text.length);
    });
    return Obx(() => Container(
          width: stateGlobal.isPortrait.isTrue ? 120 : 140,
          decoration: BoxDecoration(
            color: Theme.of(context).colorScheme.background,
            borderRadius: BorderRadius.circular(6),
          ),
          child: Row(
            children: [
              Expanded(
                child: Row(
                  children: [
                    Icon(
                      Icons.search_rounded,
                      color: Theme.of(context).hintColor,
                    ).marginSymmetric(horizontal: 4),
                    Expanded(
                      child: TextField(
                        autofocus: true,
                        controller: peerSearchTextController,
                        onChanged: (searchText) {
                          peerSearchText.value = searchText;
                        },
                        focusNode: focusNode,
                        textAlign: TextAlign.start,
                        maxLines: 1,
                        cursorColor: Theme.of(context)
                            .textTheme
                            .titleLarge
                            ?.color
                            ?.withOpacity(0.5),
                        cursorHeight: 18,
                        cursorWidth: 1,
                        style: const TextStyle(fontSize: 14),
                        decoration: InputDecoration(
                          contentPadding:
                              const EdgeInsets.symmetric(vertical: 6),
                          hintText:
                              focused.value ? null : translate("Search ID"),
                          hintStyle: TextStyle(
                              fontSize: 14, color: Theme.of(context).hintColor),
                          border: InputBorder.none,
                          isDense: true,
                        ),
                      ).workaroundFreezeLinuxMint(),
                    ),
                    // Icon(Icons.close),
                    IconButton(
                      alignment: Alignment.centerRight,
                      padding: const EdgeInsets.only(right: 2),
                      onPressed: () {
                        setState(() {
                          peerSearchTextController.clear();
                          peerSearchText.value = "";
                          drawer = false;
                        });
                      },
                      icon: Tooltip(
                          message: translate('Close'),
                          child: Icon(
                            Icons.close,
                            color: Theme.of(context).hintColor,
                          )),
                    ),
                  ],
                ),
              )
            ],
          ),
        ));
  }
}

class PeerViewDropdown extends StatefulWidget {
  const PeerViewDropdown({super.key});

  @override
  State<PeerViewDropdown> createState() => _PeerViewDropdownState();
}

class _PeerViewDropdownState extends State<PeerViewDropdown> {
  @override
  Widget build(BuildContext context) {
    final List<PeerUiType> types = [
      PeerUiType.grid,
      PeerUiType.tile,
      PeerUiType.list
    ];
    final style = TextStyle(
        color: Theme.of(context).textTheme.titleLarge?.color,
        fontSize: MenuConfig.fontSize,
        fontWeight: FontWeight.normal);
    List<PopupMenuEntry> items = List.empty(growable: true);
    items.add(PopupMenuItem(
        height: 36,
        enabled: false,
        child: Text(translate("Change view"), style: style)));
    for (var e in PeerUiType.values) {
      items.add(PopupMenuItem(
          height: 36,
          child: Obx(() => Center(
                child: SizedBox(
                  height: 36,
                  child: getRadio<PeerUiType>(
                      Tooltip(
                          message: translate(types.indexOf(e) == 0
                              ? 'Big tiles'
                              : types.indexOf(e) == 1
                                  ? 'Small tiles'
                                  : 'List'),
                          child: Icon(
                            e == PeerUiType.grid
                                ? Icons.grid_view_rounded
                                : e == PeerUiType.list
                                    ? Icons.view_list_rounded
                                    : Icons.view_agenda_rounded,
                            size: 18,
                          )),
                      e,
                      peerCardUiType.value,
                      dense: true,
                      isOptionFixed(kOptionPeerCardUiType)
                          ? null
                          : (PeerUiType? v) async {
                              if (v != null) {
                                peerCardUiType.value = v;
                                setState(() {});
                                await bind.setLocalFlutterOption(
                                  k: kOptionPeerCardUiType,
                                  v: peerCardUiType.value.index.toString(),
                                );
                                if (Navigator.canPop(context)) {
                                  Navigator.pop(context);
                                }
                              }
                            }),
                ),
              ))));
    }

    var menuPos = RelativeRect.fromLTRB(0, 0, 0, 0);
    return _hoverAction(
        context: context,
        toolTip: translate('Change view'),
        child: Icon(
          peerCardUiType.value == PeerUiType.grid
              ? Icons.grid_view_rounded
              : peerCardUiType.value == PeerUiType.list
                  ? Icons.view_list_rounded
                  : Icons.view_agenda_rounded,
          size: 18,
        ),
        onTapDown: (details) {
          final x = details.globalPosition.dx;
          final y = details.globalPosition.dy;
          menuPos = RelativeRect.fromLTRB(x, y, x, y);
        },
        onTap: () => showMenu(
              context: context,
              position: menuPos,
              items: items,
              elevation: 8,
            ));
  }
}

class PeerSortDropdown extends StatefulWidget {
  const PeerSortDropdown({super.key});

  @override
  State<PeerSortDropdown> createState() => _PeerSortDropdownState();
}

class _PeerSortDropdownState extends State<PeerSortDropdown> {
  _PeerSortDropdownState() {
    if (!PeerSortType.values.contains(peerSort.value)) {
      _loadLocalOptions();
    }
  }

  void _loadLocalOptions() {
    peerSort.value = PeerSortType.remoteId;
    bind.setLocalFlutterOption(
      k: kOptionPeerSorting,
      v: peerSort.value,
    );
  }

  @override
  Widget build(BuildContext context) {
    final style = TextStyle(
        color: Theme.of(context).textTheme.titleLarge?.color,
        fontSize: MenuConfig.fontSize,
        fontWeight: FontWeight.normal);
    List<PopupMenuEntry> items = List.empty(growable: true);
    items.add(PopupMenuItem(
        height: 36,
        enabled: false,
        child: Text(translate("Sort by"), style: style)));
    for (var e in PeerSortType.values) {
      items.add(PopupMenuItem(
          height: 36,
          child: Obx(() => Center(
                child: SizedBox(
                  height: 36,
                  child: getRadio(
                      Text(translate(e), style: style), e, peerSort.value,
                      dense: true, (String? v) async {
                    if (v != null) {
                      peerSort.value = v;
                      await bind.setLocalFlutterOption(
                        k: kOptionPeerSorting,
                        v: peerSort.value,
                      );
                    }
                  }),
                ),
              ))));
    }

    var menuPos = RelativeRect.fromLTRB(0, 0, 0, 0);
    return _hoverAction(
      context: context,
      toolTip: translate('Sort by'),
      child: Icon(
        Icons.sort_rounded,
        size: 18,
      ),
      onTapDown: (details) {
        final x = details.globalPosition.dx;
        final y = details.globalPosition.dy;
        menuPos = RelativeRect.fromLTRB(x, y, x, y);
      },
      onTap: () => showMenu(
        context: context,
        position: menuPos,
        items: items,
        elevation: 8,
      ),
    );
  }
}

class RefreshWidget extends StatefulWidget {
  final VoidCallback onPressed;
  final Widget child;
  final RxBool? spinning;
  const RefreshWidget(
      {super.key, required this.onPressed, required this.child, this.spinning});

  @override
  State<RefreshWidget> createState() => RefreshWidgetState();
}

class RefreshWidgetState extends State<RefreshWidget> {
  double turns = 0.0;
  bool hover = false;

  @override
  void initState() {
    super.initState();
    widget.spinning?.listen((v) {
      if (v && mounted) {
        setState(() {
          turns += 1;
        });
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    final deco = BoxDecoration(
      color: Theme.of(context).colorScheme.background,
      borderRadius: BorderRadius.circular(6),
    );
    return AnimatedRotation(
        turns: turns,
        duration: const Duration(milliseconds: 200),
        onEnd: () {
          if (widget.spinning?.value == true && mounted) {
            setState(() => turns += 1.0);
          }
        },
        child: Container(
          padding: EdgeInsets.all(4.0),
          margin: EdgeInsets.symmetric(horizontal: 1),
          decoration: hover ? deco : null,
          child: InkWell(
              onTap: () {
                if (mounted) setState(() => turns += 1.0);
                widget.onPressed();
              },
              onHover: (value) {
                if (mounted) {
                  setState(() {
                    hover = value;
                  });
                }
              },
              child: widget.child),
        ));
  }
}

Widget _hoverAction(
    {required BuildContext context,
    required Widget child,
    required Function() onTap,
    required String toolTip,
    GestureTapDownCallback? onTapDown,
    RxBool? hoverableWhenfalse,
    EdgeInsetsGeometry padding = const EdgeInsets.all(4.0)}) {
  final hover = false.obs;
  final deco = BoxDecoration(
    color: Theme.of(context).colorScheme.background,
    borderRadius: BorderRadius.circular(6),
  );
  return Tooltip(
    message: toolTip,
    child: Obx(
      () => Container(
          margin: EdgeInsets.symmetric(horizontal: 1),
          decoration:
              (hover.value || hoverableWhenfalse?.value == false) ? deco : null,
          child: InkWell(
              onHover: (value) => hover.value = value,
              onTap: onTap,
              onTapDown: onTapDown,
              child: Container(padding: padding, child: child))),
    ),
  );
}

class PullDownMenuEntryImpl extends StatelessWidget
    implements PullDownMenuEntry {
  final Widget child;
  const PullDownMenuEntryImpl({super.key, required this.child});

  @override
  Widget build(BuildContext context) {
    return child;
  }
}
