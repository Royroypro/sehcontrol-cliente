import 'dart:convert';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:get/get.dart';
import 'package:path/path.dart';
import 'package:url_launcher/url_launcher_string.dart';
import 'package:window_manager/window_manager.dart';

/// Los documentos legales viven en el servidor del proveedor, no en
/// rustdesk.com: quien instala esto acepta las condiciones de Sehcontrol, y el
/// acuerdo de RustDesk describe otro servicio y otro responsable.
///
/// Es una constante y no el api-server configurado a proposito: en una
/// instalacion nueva todavia no hay servidor configurado, y esta pantalla es
/// justamente la primera que se ve.
const String _kSiteBase = 'https://sehcontrol.sehuacho.com';

class InstallPage extends StatefulWidget {
  const InstallPage({Key? key}) : super(key: key);

  @override
  State<InstallPage> createState() => _InstallPageState();
}

class _InstallPageState extends State<InstallPage> {
  final tabController = DesktopTabController(tabType: DesktopTabType.main);

  _InstallPageState() {
    Get.put<DesktopTabController>(tabController);
    const label = "install";
    tabController.add(TabInfo(
        key: label,
        label: label,
        closable: false,
        page: _InstallPageBody(
          key: const ValueKey(label),
        )));
  }

  @override
  void dispose() {
    super.dispose();
    Get.delete<DesktopTabController>();
  }

  @override
  Widget build(BuildContext context) {
    return DragToResizeArea(
      resizeEdgeSize: stateGlobal.resizeEdgeSize.value,
      enableResizeEdges: windowManagerEnableResizeEdges,
      child: Container(
        child: Scaffold(
            backgroundColor: Theme.of(context).colorScheme.background,
            body: DesktopTab(controller: tabController)),
      ),
    );
  }
}

class _InstallPageBody extends StatefulWidget {
  const _InstallPageBody({Key? key}) : super(key: key);

  @override
  State<_InstallPageBody> createState() => _InstallPageBodyState();
}

class _InstallPageBodyState extends State<_InstallPageBody>
    with WindowListener {
  late final TextEditingController controller;
  final RxBool startmenu = true.obs;
  final RxBool desktopicon = true.obs;
  final RxBool printer = false.obs;
  final RxBool showProgress = false.obs;
  final RxBool btnEnabled = true.obs;

  // todo move to theme.
  final buttonStyle = OutlinedButton.styleFrom(
    textStyle: TextStyle(fontSize: 14, fontWeight: FontWeight.normal),
    padding: EdgeInsets.symmetric(vertical: 15, horizontal: 12),
  );

  _InstallPageBodyState() {
    controller = TextEditingController(text: bind.installInstallPath());
    final installOptions = jsonDecode(bind.installInstallOptions());
    startmenu.value = installOptions['STARTMENUSHORTCUTS'] != '0';
    desktopicon.value = installOptions['DESKTOPSHORTCUTS'] != '0';
    printer.value = installOptions['PRINTER'] == '1';
  }

  @override
  void initState() {
    windowManager.addListener(this);
    super.initState();
  }

  @override
  void dispose() {
    windowManager.removeListener(this);
    super.dispose();
  }

  @override
  void onWindowClose() {
    gFFI.close();
    super.onWindowClose();
    windowManager.setPreventClose(false);
    windowManager.close();
  }

  /// Un enlace legal con su URL visible en el tooltip: quien esta por instalar
  /// algo que controla su equipo tiene derecho a ver a donde lo mandan antes
  /// de hacer clic.
  Widget _legalLink(BuildContext context, String label, String url) {
    return InkWell(
      hoverColor: Colors.transparent,
      onTap: () => launchUrlString(url),
      child: Tooltip(
        message: url,
        child: Row(mainAxisSize: MainAxisSize.min, children: [
          Icon(Icons.launch_outlined, size: 15).marginOnly(right: 5),
          Text(
            translate(label),
            style: TextStyle(
              decoration: TextDecoration.underline,
              color: Theme.of(context).colorScheme.primary,
            ),
          )
        ]),
      ),
    );
  }

  InkWell Option(RxBool option, {String label = ''}) {
    return InkWell(
      // todo mouseCursor: "SystemMouseCursors.forbidden" or no cursor on btnEnabled == false
      borderRadius: BorderRadius.circular(6),
      onTap: () => btnEnabled.value ? option.value = !option.value : null,
      child: Row(
        children: [
          Obx(
            () => Checkbox(
              visualDensity: VisualDensity(horizontal: -4, vertical: -4),
              value: option.value,
              onChanged: (v) =>
                  btnEnabled.value ? option.value = !option.value : null,
            ).marginOnly(right: 8),
          ),
          Expanded(
            child: Text(translate(label)),
          ),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final double em = 13;
    final isDarkTheme = MyTheme.currentThemeMode() == ThemeMode.dark;
    return Scaffold(
        backgroundColor: null,
        body: SingleChildScrollView(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              // Encabezado con el nombre del producto: el titulo suelto
              // "Instalacion" no decia que se estaba instalando.
              Row(
                crossAxisAlignment: CrossAxisAlignment.center,
                children: [
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text('${translate('Installation')} $appName',
                            style:
                                Theme.of(context).textTheme.headlineMedium),
                        Text(
                          '${translate('Version')} ${bind.mainGetVersion()}',
                          style: TextStyle(
                              fontSize: 13,
                              color: Theme.of(context).hintColor),
                        ).marginOnly(top: 4),
                      ],
                    ),
                  ),
                ],
              ),
              Row(
                children: [
                  Text('${translate('Installation Path')}:')
                      .marginOnly(right: 10),
                  Expanded(
                    child: TextField(
                      controller: controller,
                      readOnly: true,
                      decoration: InputDecoration(
                        contentPadding: EdgeInsets.all(0.75 * em),
                      ),
                    ).workaroundFreezeLinuxMint().marginOnly(right: 10),
                  ),
                  Obx(
                    () => OutlinedButton.icon(
                      icon: Icon(Icons.folder_outlined, size: 16),
                      onPressed: btnEnabled.value ? selectInstallPath : null,
                      style: buttonStyle,
                      label: Text(translate('Change Path')),
                    ),
                  )
                ],
              ).marginSymmetric(vertical: 2 * em),
              // Las casillas quedaban sueltas contra el fondo, a la misma
              // altura visual que el resto; agrupadas se leen como lo que son:
              // opciones de la instalacion.
              Text(
                translate('Options'),
                style: TextStyle(
                    fontSize: 12,
                    fontWeight: FontWeight.w600,
                    letterSpacing: 0.4,
                    color: Theme.of(context).hintColor),
              ).marginOnly(bottom: 8),
              Option(startmenu, label: 'Create start menu shortcuts')
                  .marginOnly(bottom: 4),
              Option(desktopicon, label: 'Create desktop icon'),
              //Option(printer, label: 'Install {$appName} Printer'),
              Container(
                  padding: EdgeInsets.all(14),
                  decoration: BoxDecoration(
                    color: isDarkTheme
                        ? Color.fromARGB(135, 87, 87, 90)
                        : Colors.grey[100],
                    borderRadius: BorderRadius.circular(8),
                    border: Border.all(
                        color: isDarkTheme
                            ? Colors.grey.withOpacity(0.4)
                            : Colors.grey.shade300),
                  ),
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Icon(Icons.info_outline_rounded, size: 26)
                          .marginOnly(right: 14, top: 2),
                      Expanded(
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          children: [
                            Text(translate('agreement_tip'))
                                .marginOnly(bottom: 0.7 * em),
                            // Los dos documentos juntos: el acuerdo remite a la
                            // privacidad y quien va a instalar suele querer
                            // leer esa antes que nada.
                            Wrap(
                              spacing: 18,
                              runSpacing: 6,
                              children: [
                                _legalLink(
                                    context, 'End-user license agreement',
                                    '$_kSiteBase/eula'),
                                _legalLink(context, 'Privacy Statement',
                                    '$_kSiteBase/privacy.html'),
                              ],
                            ),
                          ],
                        ),
                      )
                    ],
                  )).marginSymmetric(vertical: 1.6 * em),
              Row(
                children: [
                  Expanded(
                    // NOT use Offstage to wrap LinearProgressIndicator
                    child: Obx(() => showProgress.value
                        ? LinearProgressIndicator().marginOnly(right: 10)
                        : Offstage()),
                  ),
                  // Orden: primero las salidas, y la accion recomendada al
                  // final. Antes "Aceptar e instalar" quedaba en el medio,
                  // entre dos botones que hacen otra cosa, y no se leia como
                  // el camino principal.
                  Obx(
                    () => OutlinedButton.icon(
                      icon: Icon(Icons.close_rounded, size: 16),
                      label: Text(translate('Cancel')),
                      onPressed:
                          btnEnabled.value ? () => windowManager.close() : null,
                      style: buttonStyle,
                    ).marginOnly(right: 10),
                  ),
                  Offstage(
                    offstage: bind.installShowRunWithoutInstall(),
                    child: Obx(
                      () => OutlinedButton.icon(
                        icon: Icon(Icons.screen_share_outlined, size: 16),
                        label: Text(translate('Run without install')),
                        onPressed: btnEnabled.value
                            ? () => bind.installRunWithoutInstall()
                            : null,
                        style: buttonStyle,
                      ).marginOnly(right: 10),
                    ),
                  ),
                  Obx(
                    () => ElevatedButton.icon(
                      icon: Icon(Icons.done_rounded, size: 18),
                      label: Text(translate('Accept and Install')),
                      onPressed: btnEnabled.value ? install : null,
                      style: ElevatedButton.styleFrom(
                        padding: EdgeInsets.symmetric(
                            horizontal: 1.6 * em, vertical: 1.1 * em),
                        textStyle: const TextStyle(
                            fontSize: 14, fontWeight: FontWeight.w600),
                      ),
                    ),
                  ),
                ],
              )
            ],
          ).paddingSymmetric(horizontal: 4 * em, vertical: 3 * em),
        ));
  }

  void install() {
    do_install() {
      btnEnabled.value = false;
      showProgress.value = true;
      String args = '';
      if (startmenu.value) args += ' startmenu';
      if (desktopicon.value) args += ' desktopicon';
      if (printer.value) args += ' printer';
      bind.installInstallMe(options: args, path: controller.text);
    }

    do_install();
  }

  void selectInstallPath() async {
    String? install_path = await FilePicker.platform
        .getDirectoryPath(initialDirectory: controller.text);
    if (install_path != null) {
      controller.text = join(install_path, await bind.mainGetAppName());
    }
  }
}
