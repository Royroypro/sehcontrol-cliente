# Sehcontrol ScreenCam - Entrega 4

## Objetivo

La Entrega 4 cierra la vista previa en vivo de ScreenCam desde el cliente
Sehcontrol hasta el panel, usando el flujo validado de extremo a extremo:

panel -> WebSocket autenticado -> cliente Sehcontrol -> captura de pantalla ->
H.264 -> MPEG-TS -> SRT -> MediaMTX -> WHEP/WebRTC -> navegador.

## Arquitectura final

- El daemon mantiene la captura ScreenCam y toma un snapshot seguro del estado
  del publisher sin exponer credenciales.
- `PreviewControl` gobierna una única sesión activa por `session_id`,
  `rustdesk_id` y `generation`.
- El publisher recibe access units H.264 del tap de captura, empaqueta MPEG-TS y
  publica por SRT hacia MediaMTX.
- Flutter consulta el daemon por IPC con `screencam-preview-status` y reenvía
  eventos seguros al panel mediante el WebSocket autenticado existente.
- El panel consume los eventos de lifecycle y muestra el stream publicado vía
  WHEP/WebRTC. Los cambios del panel web pertenecen a otro repositorio.

## Archivos principales

- `src/server/screen_cam/preview/`: control, tap, MPEG-TS y publisher SRT.
- `src/server/screen_cam/mod.rs`: integración de captura, tap y control.
- `src/server/screen_cam/rtsp.rs`: ajustes locales para coexistencia con preview.
- `src/ipc.rs`: comandos IPC de start, stop y status.
- `src/flutter_ffi.rs`: puente Rust -> Flutter y emisión por
  `GLOBAL_EVENT_STREAM` / `APP_TYPE_MAIN`.
- `flutter/lib/common/screen_cam_preview_protocol.dart`: validación y whitelist.
- `flutter/lib/common/screen_cam_preview_lifecycle.dart`: retry y deduplicación.
- `flutter/lib/common/realtime_channel.dart`: canal WebSocket con reconexión.
- `flutter/lib/models/user_model.dart`: integración con sesión autenticada.
- `libs/srt-protocol-patched/`: fork local mínimo de `srt-protocol`.

## Problemas SRT encontrados

- MediaMTX rechazaba el handshake cuando el cliente anunciaba una versión SRT
  inferior a la mínima aceptada por su implementación.
- Los flags del handshake debían ser compatibles con el modo usado por
  MediaMTX, sin declarar capacidades que el crate no implementa.
- La retransmisión debía respetar el deadline para evitar que paquetes tardíos
  degradaran el cierre o prolongaran la sesión.
- Había conclusiones duplicadas en caminos de cierre, lo que dificultaba
  distinguir un stop real de un cierre repetido.

## Solución aplicada

- Se agregó un fork local de `srt-protocol` 0.4.4 con el cambio mínimo de
  versión anunciada en el handshake.
- `Cargo.toml` y `Cargo.lock` fijan `srt-tokio` / `srt-protocol` y aplican el
  patch local sin agregar dependencias nativas.
- El publisher H.264/MPEG-TS/SRT usa el tap de captura y mantiene el lifecycle
  desacoplado del camino RTSP existente.
- La retransmisión respeta el deadline y el cierre deduplica conclusiones.

## Lifecycle

El lifecycle real se publica como:

- `Connecting`
- `Started`
- `Failed`
- `Stopped`

El camino completo es:

daemon -> snapshot seguro -> IPC `screencam-preview-status` -> poller único de
Flutter -> `GLOBAL_EVENT_STREAM` / `APP_TYPE_MAIN` -> dispatcher Dart ->
WebSocket autenticado -> panel.

Flutter reintenta cuando el runtime o el WebSocket todavía no están listos. La
deduplicación usa `session_id + generation + sequence + event`, por lo que un
reintento no duplica eventos ya confirmados y un cambio real de generación sí
se reenvía.

## Campos seguros enviados

Los eventos hacia Flutter y el panel transportan únicamente datos seguros como:

- `event`
- `state`
- `session_id`
- `rustdesk_id`
- `generation`
- `sequence`
- metadata de cierre o error no sensible

No se reenvían tokens, URLs firmadas, credenciales SRT ni stream ids completos.

## Pruebas automatizadas

La entrega queda cubierta por pruebas Rust y Flutter enfocadas en:

- lifecycle del publisher;
- start/stop/status por IPC;
- snapshot seguro;
- whitelist y validación Dart;
- retry del canal realtime;
- deduplicación de eventos;
- integración del poller Flutter.

Los cambios de `flutter/pubspec.yaml` y `flutter/pubspec.lock` declaran
`fake_async` como dependencia directa de desarrollo porque los tests del canal
realtime la importan para manejar timers y reintentos de forma determinística,
sin sleeps reales.

## Validación real

Se validó de extremo a extremo:

panel -> WebSocket autenticado -> cliente Sehcontrol -> captura de pantalla ->
codificación H.264 -> MPEG-TS -> publicación SRT -> MediaMTX -> WHEP/WebRTC ->
video visible en navegador.

También se validó el cierre:

- el panel envió `STOP`;
- la conexión SRT fue expulsada;
- MediaMTX cerró el publicador;
- WebRTC pasó a `closed`;
- no quedaron conexiones colgadas.

## Fuera de alcance

- Los cambios del panel web se administran en otro repositorio y no forman parte
  de estos commits del cliente.
- El sistema de actualización automática queda para otra entrega.
- El control administrativo de duración queda para otra entrega.

## Deuda técnica conocida

El test completo `cargo test --lib` mantiene tres fallos preexistentes en
`src/common.rs`:

- `common::tests::test_is_public`
- `common::tests::test_should_use_tcp_proxy_for_api_url`
- `common::tests::test_get_tcp_proxy_addr_normalizes_bare_ipv6_host`

La causa conocida es que esos tests todavía esperan `rustdesk.com` mientras el
fork usa `sehcontrol.com`. Esta entrega no modifica `src/common.rs` ni corrige
esa deuda técnica.
