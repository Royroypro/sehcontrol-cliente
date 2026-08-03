// El aviso de actualizacion que aparece al frente, como el de inicio de
// sesion.
//
// La tarjeta lateral de la pagina principal sigue existiendo y es el
// recordatorio permanente; esto es lo que se ve al abrir el cliente, porque
// una tarjeta al costado de una barra que ademas se puede cerrar pasaba
// desapercibida durante semanas.
//
// Deliberadamente NO bloquea: tiene un boton de cerrar y el usuario puede
// seguir trabajando. Forzar la actualizacion con un modal sin salida deja el
// equipo inutilizable cuando la descarga falla o el servidor no responde, y
// varios de estos equipos solo se alcanzan por este mismo cliente.

import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/desktop/widgets/update_progress.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:url_launcher/url_launcher_string.dart';

/// Se muestra una vez por version: quien elige "Mas tarde" no vuelve a verlo
/// hasta que el panel publique otra, pero la tarjeta lateral sigue ahi.
/// Recordarlo en memoria y no en disco es a proposito -- reaparece al
/// reabrir la app, que es el recordatorio que se buscaba sin llegar a ser
/// insistente dentro de una misma sesion.
String? _dismissedVersion;

bool _dialogOpen = false;

/// `true` si hay una actualizacion publicada por el panel que este usuario
/// todavia no descarto en esta sesion.
bool shouldOfferUpdate() {
  if (_dialogOpen) return false;
  final version = bind.mainGetNewVersion();
  if (version.isEmpty) return false;
  // Solo las publicadas por el panel traen URL directa. Sin ella, la descarga
  // dependeria de componer una URL estilo GitHub, que este despliegue no usa.
  if (bind.mainGetCommonSync(key: 'update-download-url').isEmpty) return false;
  return _dismissedVersion != version;
}

/// Muestra el aviso si corresponde. Seguro de llamar varias veces.
void maybeShowUpdateDialog() {
  if (!shouldOfferUpdate()) return;
  final version = bind.mainGetNewVersion();
  final notes = bind.mainGetCommonSync(key: 'update-notes');
  _dialogOpen = true;
  gFFI.dialogManager.show((setState, close, context) {
    dismiss() {
      _dismissedVersion = version;
      _dialogOpen = false;
      close();
    }

    return CustomAlertDialog(
      title: Row(
        children: [
          const Icon(Icons.system_update_alt_rounded, size: 26),
          const SizedBox(width: 10),
          Expanded(child: Text(translate('Actualizacion disponible'))),
        ],
      ),
      content: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 420),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              '${translate('Version')} $version',
              style: const TextStyle(fontWeight: FontWeight.w600, fontSize: 15),
            ),
            const SizedBox(height: 12),
            // Las notas las escribe el operador en el panel. Si no puso
            // ninguna se explica igual por que conviene actualizar, en vez de
            // dejar un hueco que no dice nada.
            Text(
              notes.trim().isNotEmpty
                  ? notes.trim()
                  : translate(
                      'Esta version incluye correcciones y mejoras. Se recomienda actualizar.'),
              style: const TextStyle(fontSize: 14, height: 1.4),
            ),
            const SizedBox(height: 14),
            // En Android la instalacion no la hace la app: se descarga el APK y
            // el instalador del sistema pide confirmacion. Prometer que "se
            // instala sola" seria mentir, y el usuario abandonaria pensando
            // que fallo cuando en realidad falta un paso suyo.
            Text(
              translate(isAndroid
                  ? 'Se descargara el instalador. Al abrirlo, Android pedira confirmar la instalacion.'
                  : 'La actualizacion se descarga e instala sola. El equipo se reinicia un momento al terminar.'),
              style: TextStyle(fontSize: 12, color: Theme.of(context).hintColor),
            ),
          ],
        ),
      ),
      actions: [
        dialogButton(translate('Mas tarde'),
            onPressed: dismiss, isOutline: true),
        // El boton principal, ancho, para que la accion recomendada sea la
        // evidente y no una mas entre varias.
        SizedBox(
          width: 200,
          height: 40,
          child: ElevatedButton.icon(
            icon: Icon(isAndroid
                ? Icons.open_in_new_rounded
                : Icons.download_rounded),
            label: Text(translate(
                isAndroid ? 'Descargar actualizacion' : 'Actualizar ahora')),
            onPressed: () {
              final url = bind.mainGetCommonSync(key: 'update-download-url');
              _dialogOpen = false;
              if (isAndroid) {
                // Se delega en el navegador y en el instalador del sistema.
                //
                // La alternativa era descargar el APK dentro de la app y
                // lanzar un intent de instalacion, lo que obliga a pedir
                // REQUEST_INSTALL_PACKAGES y a mandar al usuario a Ajustes a
                // habilitar "instalar apps desconocidas" -- un permiso que
                // asusta y un desvio que mucha gente no completa. Como estos
                // clientes ya se instalan de forma lateral, este es
                // exactamente el mismo camino que el usuario recorrio la
                // primera vez.
                _dismissedVersion = version;
                close();
                launchUrlString(url);
                return;
              }
              // handleUpdate abre su propio dialogo de progreso y hace
              // dismissAll() primero, asi que este se cierra solo.
              handleUpdate(url);
            },
          ),
        ),
      ],
      onCancel: dismiss,
    );
  });
}
