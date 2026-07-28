# Sehcontrol ScreenCam — Auditoría técnica y plan

Convertir la pantalla de una PC Windows en una fuente de video IP (RTSP, luego ONVIF)
que un DVR/NVR de la misma red pueda agregar y grabar como si fuera una cámara.

Este documento es la entrega de la **Fase 0 (Auditoría)** del plan original: el mapa de
qué se reutiliza, qué hay que construir, y qué decisiones hay que tomar **antes** de
escribir código.

> Todo lo que sigue se verificó leyendo el código de este repo (rama
> `feature/machine-id-membership-android`), con referencias `archivo:línea`. No se
> compiló ni se ejecutó nada de ScreenCam todavía — no existe.

---

## Estado actual (26/07)

**Fase 1 implementada y validada contra VLC.** Decisión de la sección 7 tomada: **opción
A** (solo hardware, sin fallback por software) — `Cargo.toml` feature `screencam`
(`screencam = ["hwcodec"]`), gateada a `#[cfg(all(windows, feature = "screencam"))]` en
`src/server.rs`. Compilar con `python3 build.py --flutter --screencam`.

Código: `src/server/screen_cam/{mod,rtp,rtsp}.rs`. Arranca dentro del proceso `--server`
(`src/server.rs`, rama `is_server`), sin depender de la ventana principal.

Confirmado en la práctica:
- VLC reproduce `rtsp://<ip>:8554/live/main` — criterio de aceptación #1 cumplido.
- **Sin audio, a propósito** — el plan original (sección 2.4) ya pedía audio desactivado
  para el MVP. No implementado, evaluar como fase aparte si hace falta más adelante
  (requeriría códec AAC, que hoy no existe en el pipeline, y una segunda pista `m=audio`
  en el SDP).
- **Sin conflicto observado** al conectar una sesión de control remoto mientras ScreenCam
  está transmitiendo, pese a que ambos abren su propio capturador DXGI por separado (la
  simplificación de Fase 1 descrita en la sección 3.4 de este documento — captura
  independiente en vez de fan-out compartido). Esto **no cierra el riesgo**, solo dice que
  no se manifestó en esta prueba puntual — sigue pendiente de validar bajo más carga
  (reconexiones seguidas, cambio de resolución de monitor en vivo, múltiples sesiones)
  antes de dar por resuelto el punto 3.4/criterio de aceptación #8.

**Pendiente:** Fase 2 (prueba contra un NVR/DVR real — Hikvision, Dahua, XVR genérico).
Ver criterios de aceptación en la sección 6.

## Fase 2 — validada con NVR de software; hardware real diferido (26/07 → 27/07)

Prueba contra **ZoneMinder** (Ubuntu Server) como NVR, transporte TCP interleaved: conectó,
decodificó, y grabó eventos reales por detección de movimiento — confirmado el 27/07 con
capturas de la lista de eventos (duración, cantidad de frames, frames de alarma, score,
miniatura por evento — no solo el live view). Esto cierra el punto más importante que
quedaba pendiente de la fase.

**Dado por suficiente para avanzar:**
- ✅ Conecta y decodifica el stream (TCP interleaved).
- ✅ Graba eventos reales, persistidos, con metadata (no solo el preview en vivo).

**Diferido a propósito, no descartado — sin bloquear el resto del proyecto:**
- Reproducir el historial de un evento grabado.
- Sesión larga (8hs) sin cortes.
- Corte y reconexión de red.
- Probar contra un NVR de hardware real (Hikvision/Dahua/XVR genérico) — pendiente porque
  no hay uno disponible todavía. ZoneMinder es software y puede ser más tolerante que un
  firmware de fábrica, así que esto sigue siendo la validación más fuerte que falta antes
  de asumir compatibilidad universal, pero no es urgente mientras no haya hardware a mano
  para probar.

## Fase 3 — en progreso (26/07)

- ✅ **Arranque con Windows**: no requirió código nuevo. El servicio de Windows
  `Sehcontrol` (`run_service` en `src/platform/windows.rs:640`) ya lanza el proceso
  `--server` en cada arranque y de nuevo si cambia la sesión activa — y ahí es
  exactamente donde `screen_cam::start()` está enganchado (`src/server.rs`). ScreenCam
  hereda esto gratis de la arquitectura existente.
- ✅ **Watchdog/reinicio si falla**: implementado en `src/server/screen_cam/mod.rs`.
  Se separó el listener RTSP (arranca una sola vez, vive para siempre — no tiene lógica
  de apagado) del loop de captura+encoder (`capture_loop`, lo que sí puede fallar:
  monitor desconectado, encoder roto, error de conversión de color). Solo se reintenta
  esto último, con backoff exponencial (2s→30s) que se resetea si el intento anterior
  estuvo arriba más de 60s (para no penalizar una falla aislada después de mucho tiempo
  corriendo bien).
- ✅ → 🔄 **Interruptor encendido/apagado protegido por PIN — reemplazado por estado de
  solo lectura (27/07).** Se había implementado en `Ajustes → Seguridad → ScreenCam`
  (`flutter/lib/desktop/pages/desktop_setting_page.dart`, dentro de `_Safety`) como
  checkbox interactivo, aprovechando `mainGetLocalOption`/`mainSetLocalOption` (sin tocar
  el bridge). **Se sacó la interactividad** una vez que la Fase 4b dejó la
  activación/desactivación exclusivamente en manos del panel web del cliente (sección
  12) — un segundo control local habría dejado al usuario "pelear" contra la decisión
  del panel, y no habría quedado claro quién manda. La tarjeta ahora es informativa:
  estado real (`Transmitiendo`/`Iniciando`/`Error: ...`/`Apagado`/`No disponible`, mismas
  claves `LocalConfig` que ya lee el heartbeat) + la URL RTSP con botón de copiar cuando
  está transmitiendo. El mecanismo de Rust (`screencam-enabled`, modos local/managed/
  supervised) se dejó intacto — sigue funcionando como fallback si alguien edita el
  archivo de config a mano, pero ya no tiene una vía de UI para tocarlo.
- ✅ **Modos local / administrado / supervisión permanente**: implementado como una
  clave `LocalConfig` más (`screencam-mode` = `local` (default) | `managed` |
  `supervised`). En modo `supervised`, `is_enabled()` ignora el interruptor local por
  completo — no se puede apagar desde el equipo ni con PIN — y registra (con throttling,
  cada 30s como máximo) si alguien lo intenta, como base para una alerta real en Fase 4.
  El checkbox de la UI se reemplaza por el mensaje "Administrado por tu organización"
  cuando está en este modo, igual que el mockup original. La distinción `local` vs
  `managed` del plan original no tiene efecto propio todavía (ambos ya están protegidos
  por el mismo candado de PIN sin un panel de por medio) — el campo existe para que Fase
  4 tenga dónde persistir "el panel puso este equipo en modo administrado" sin cambiar el
  esquema.
- ✅ **Firewall**: confirmado, no hacía falta nada. La instalación estándar
  (`src/platform/windows.rs:1548-1549`) ya agrega una regla *por programa*
  (`program="{exe}"`, sin restricción de puerto) tanto entrante como saliente. Como
  ScreenCam corre dentro del mismo `sehcontrol.exe`, el puerto 8554 ya queda permitido
  en cualquier instalación normal.
- ⏳ **Configuración de monitor/resolución/FPS/puerto desde la UI**: no implementado.
  Requiere agregar funciones nuevas al bridge Flutter↔Rust (`src/flutter_ffi.rs`), lo
  cual exige correr `flutter_rust_bridge_codegen` para regenerar
  `flutter/lib/generated_bridge.dart` — herramienta que no está disponible del lado de
  quien está verificando esto en este momento. En su lugar, `ScreenCamConfig` persiste
  en un archivo de config local (mismo mecanismo que `HwCodecConfig`, sufijo
  `_screencam`, junto al resto de la config de Sehcontrol —
  `%APPDATA%\Sehcontrol\Sehcontrol_screencam.toml` en Windows). Se puede editar a mano
  (`monitor_index`, `fps`, `rtsp_port`, `quality`) y reiniciar el servicio para aplicar.
  Cuando alguien tenga el toolchain de Flutter completo, migrar esto a una pantalla real
  es agregar el bridge + UI que ya lean/escriban este mismo archivo — no hace falta
  rediseñar el storage. `screencam-mode` (arriba) se setea igual, a mano, en el mismo
  archivo de opciones locales que ya usa `access_token`.

**Fase 3 dada por completa** salvo la pantalla de configuración visual (bloqueada por la
herramienta de codegen, no por falta de diseño — el storage ya está listo para que un
bridge futuro simplemente lea/escriba lo mismo que hoy se edita a mano).

## Fase 4 — lado cliente conectado al backend (27/07)

El desarrollador del servidor entregó el backend de licenciamiento (endpoints, jerarquía
plan→cliente→dispositivo, eventos por WebSocket — ver su mensaje del 27/07 para el detalle
completo). Se conectó el cliente contra ese contrato, **sin tocar el bridge Flutter↔Rust**
otra vez: mismo patrón de `LocalConfig` que ya se usó en Fase 3 para el interruptor y el
modo, extendido para que también sirva de canal servidor→cliente y cliente→servidor.

**Política (servidor → cliente), en `flutter/lib/models/user_model.dart`:**
- `UserModel.fetchForceLogin()` ahora pide `GET /api/client-policy?id=<rustdesk_id>` (antes
  no mandaba `id`) y persiste el bloque `screen_cam` (`licensed`, `desired_state`, `mode`)
  en `LocalConfig` vía `mainSetLocalOption` — las mismas claves que Rust ya leía.
- El evento `screen_cam.update` del WebSocket (ya conectado desde el trabajo de
  membresías) ahora también se maneja: llama al mismo persistidor, así que un cambio de
  licencia desde el panel llega en tiempo real sin esperar al próximo arranque.
- Ambos caminos son **fail-open** por diseño: si `screen_cam` nunca llegó del servidor
  (sin panel configurado, fetch falló), `is_licensed()`/`server_wants_stopped()` en Rust
  tratan la clave ausente como "sin restricción" — mismo criterio que ya se usa para
  `force_login` (ver `docs/CLIENT_INTEGRATION.md` sección 7, punto 2). Un servidor que
  quiere bloquear de verdad tiene que contestar `licensed: false` explícito.

**Gating (Rust), en `src/server/screen_cam/mod.rs`:**
- `is_enabled()` ahora chequea licencia/`desired_state` **antes** que el modo
  `supervised` — una licencia revocada o una orden de stop del panel gana incluso en modo
  supervisado (supervisado solo protege contra manipulación *local*, no reemplaza al panel
  como fuente de verdad).

**Estado (cliente → servidor), en ambos archivos:**
- `capture_loop`/el watchdog en `mod.rs` escriben `screencam-actual-state` (`starting` /
  `running` / `disabled` / `error`), `screencam-encoder`, `screencam-last-error` y
  `screencam-rtsp-clients` (cada 5s) en `LocalConfig`.
- `UserModel._sendHeartbeat()` los lee y los manda como bloque `screen_cam` dentro del
  mismo `POST /api/heartbeat` que ya existía — sin heartbeat nuevo. Se omite el campo
  entero si `screencam-actual-state` nunca se seteó (Linux/macOS, o build sin la feature
  `screencam`), para no mandar un objeto vacío sin sentido.

**Pendiente, tal como lo marcó el servidor como fuera de esta entrega:** el cliente no
actúa todavía ante comandos accionables por WebSocket más allá de releer la política
(no existen todavía del lado servidor tampoco, según su mensaje). Cuando agreguen algo
más granular que "la política cambió", hay que sumar un nuevo `case` al mismo
`_handleRealtimeEvent` que ya maneja `screen_cam.update`.

---

## Fase 4b — nuevo enfoque: módulo administrable por planes y cupos por dispositivo (27/07)

Propuesta nueva del usuario (27/07): en vez de licenciar "el dispositivo sí/no", el plan
define un **cupo de equipos simultáneos**, y el cliente elige desde su propio panel web
en cuáles de sus equipos gastar ese cupo (puede tener 5 equipos y plan para 2, activar 2,
después desactivar uno y pasarle el cupo a otro). El panel admin conserva control general
(forzar apagado, ver estado real, retirar el módulo del plan). Pensado además como el
modelo a reutilizar para futuros módulos, no solo ScreenCam.

### Análisis: qué cambia realmente del lado cliente (poco) vs. servidor (casi todo)

El contrato cliente↔servidor que ya se armó en la Fase 4 (arriba) resulta ser **compatible
con este enfoque casi sin tocar código**, porque desde el punto de vista del cliente nunca
importó *por qué* el servidor dice `licensed: true/false` — solo obedece. El cálculo de "a
cuáles de los 5 equipos de esta cuenta les toca cupo" es una decisión que el servidor ya
puede tomar **antes** de responder `GET /api/client-policy?id=<rustdesk_id>` con el
`licensed` de ESE `id` puntual — el cliente sigue preguntando "¿a mí me toca?", nunca
necesita saber cuántos cupos hay ni cuántos equipos tiene la cuenta.

Repasando los puntos del enfoque nuevo contra lo que ya existe:

| Punto del enfoque nuevo | Ya cubierto por el contrato actual | Falta |
|---|---|---|
| Plan define si incluye el módulo y cuántos equipos simultáneos | El servidor resuelve esto internamente antes de responder `licensed` — el cliente no necesita enterarse del número, solo del resultado para su `id`. | Nada del lado cliente. Es 100% lógica de servidor (contar cuántos `device_screen_cam_settings.enabled=true` tiene la cuenta contra el cupo del plan). |
| Admin agrega/retira del plan, activa/desactiva por cuenta, ve equipos habilitados | Ya llega como `licensed`/`desired_state` vía `client-policy` + heartbeat, y en tiempo real por `screen_cam.update`. | Nada del lado cliente. Es panel admin (fuera del alcance de este repo). |
| Retirar el módulo o vencer la membresía detiene el RTSP automáticamente | Ya funciona: `is_licensed()`/`server_wants_stopped()` en `mod.rs` se chequean *antes* que el modo `supervised` — una licencia revocada gana siempre, sin importar el modo local. | Nada. Ya validado en el diseño de Fase 4. |
| Cliente elige en cuáles equipos activar, dentro de su cupo | Es el panel del cliente (web, cuentas de usuario final) decidiendo qué `id` recibe `licensed:true` — mismo mecanismo que ya usa el panel admin. | Nada del lado del cliente Sehcontrol. Es UI del panel web, fuera de este repo. |
| Decisiones persisten con el equipo offline, aplican al reconectar | Ya es así: `licensed`/`desired_state` se leen de la base del servidor al servir `client-policy`, no de nada que dependa de que el equipo esté online. Al reconectar, el cliente vuelve a pedir la política (arranque) y/o recibe el `screen_cam.update` pendiente por WS si estaba conectado. | Repasar un caso borde: si el equipo estuvo offline **durante** el cambio (WS desconectado), el único momento en que se entera es en el próximo `fetchForceLogin()` (arranque) — no hay una re-sincronización activa al reconectar el WebSocket. Ver "Falta" más abajo. |
| Botón local respeta el plan/selección del panel | Ya implementado — `is_enabled()` chequea licencia antes que el toggle local. | Nada. |
| Panel del cliente muestra: compatibles, activados, cupos, conectado/desconectado, estado real, problemas, **dirección RTSP** | La mayoría ya viaja por heartbeat (`actual_state`, `encoder`, `last_error`, `rtsp_clients`) + el online/offline que ya existía. | **Falta mandar la URL/IP real del stream** (ver abajo) — hoy el servidor no tiene forma de mostrar "rtsp://x.x.x.x:8554/live/main" en el panel del cliente porque el cliente nunca lo reporta. |

### Lo que sí falta implementar del lado cliente — ✅ ambos cerrados (27/07)

El servidor respondió (sección 12 de su documento) confirmando el modelo de cupos sin
cambios al contrato, y los dos nombres de campo que quedaban pendientes. Ambos puntos ya
están implementados:

1. **Dirección RTSP en el heartbeat — ✅ hecho.** Servidor confirmó `local_ip` + `rtsp_port`
   como campos separados (no una URL pre-armada — mismo criterio que `hostname`/`os`, así
   el panel arma `rtsp://{local_ip}:{rtsp_port}/live/main` y no depende de que el cliente
   reconstruya nada si la ruta cambia). Implementado en `src/server/screen_cam/mod.rs`:
   `detect_local_ip()` usa la misma técnica de "UDP connect a una IP fija sin mandar nada"
   que ya usaba `rtsp.rs::local_ip_for_peer`, pero anclada a una dirección fija en vez de
   la de un peer RTSP puntual (para el heartbeat hace falta una IP representativa de toda
   la máquina, no una por-viewer). Se escribe una sola vez al entrar en estado `running`
   (no cambia durante la corrida) en `screencam-local-ip`/`screencam-rtsp-port` vía
   `LocalConfig`, y `UserModel._readScreenCamStatus()` en Dart los lee y los manda.
2. **Resync de política al reconectar el WebSocket — ✅ hecho.** El servidor confirmó que
   no hace falta nada nuevo de su lado — alcanza con volver a pedir
   `GET /api/client-policy?id=X` al reconectar. Se enganchó en el `case 'connected':` de
   `_handleRealtimeEvent` (antes era un no-op), que dispara tanto en la primera conexión
   como en cada reconexión — llama a `UserModel.fetchForceLogin()` (que ya persiste
   `screen_cam` como efecto secundario) sin necesidad de una función separada.
3. **`max_streams`**: confirmado por el servidor — se redefinió como cupo de *cuenta*, no
   de equipo, así que sigue sin ser algo que el cliente necesite consumir. Sin cambios.

### Lo que es 100% servidor/panel (no toca este repo)

- Contador de cupo por cuenta contra el límite del plan.
- Panel del cliente (nueva superficie web, no confundir con el panel admin) donde el
  usuario final ve sus equipos y elige dónde activar el cupo.
- Persistencia de la selección aunque el equipo esté offline (ya es una propiedad natural
  de guardarlo en la base del servidor, no del cliente).
- Runtime, la reutilización del modelo para futuros módulos — es un patrón de diseño de
  base de datos/API del servidor (`plan_modules`/`customer_modules`/`device_*_settings`
  genérico), no algo que el cliente Sehcontrol necesite saber generalizar: el cliente ya
  habla en términos de "una clave `LocalConfig` con prefijo `<módulo>-`" (`screencam-*`),
  que es igual de reusable para un futuro módulo sin cambiar nada de cómo ya funciona.

### Veredicto

No hizo falta rediseñar nada de lo ya construido. El enfoque de cupos fue un cambio de
**cómo decide el servidor** `licensed`/`desired_state` para un `id` dado — el cliente
solo pregunta y obedece, que es exactamente el desacople correcto para que esto funcione
sin tocar Sehcontrol de nuevo cuando cambien las reglas de cupos en el futuro. El servidor
confirmó el modelo completo (sección 12 de su documento, probado end-to-end: sin módulo →
habilitar plan → activar 2 de 3 equipos → rechazo por cupo agotado → liberar/reactivar →
suspender cuenta → todo se apaga solo) y los dos puntos pendientes de este lado (dirección
RTSP en el heartbeat, resync al reconectar el WS) ya están implementados — ver arriba.

**Fase 4b dada por completa** del lado cliente. Lo que sigue (panel del cliente en sí,
`GET/POST /api/screen-cam/devices/*`, pantalla admin) es 100% servidor, fuera de este repo.

## Fase 4c — contrato final confirmado por el servidor, un hueco encontrado y cerrado (27/07)

El servidor mandó el contrato consolidado (política + heartbeat + WebSocket + resync).
Se revisó punto por punto contra lo ya implementado — todo coincidía **salvo uno**:

**`actual_state` mandaba 4 valores, el contrato documenta solo 2.** El servidor especifica
`actual_state: "running" / "stopped"` en el heartbeat. Rust internamente distingue 4 estados
(`starting`/`running`/`disabled`/`error` — necesarios para la tarjeta de solo lectura en
Ajustes, que sí se beneficia de mostrar "Iniciando..."/"Error: ..." por separado). Antes de
esta revisión, esos 4 valores viajaban tal cual al heartbeat — funcionaba en la práctica
(cualquier cosa distinta de `"running"` se lee como "no está transmitiendo"), pero no era lo
acordado, y una validación de enum estricta del lado servidor lo habría rechazado.

**Corregido** en `UserModel._readScreenCamStatus()` (`flutter/lib/models/user_model.dart`):
se separó el estado rico que lee la UI local (sin cambios, sigue leyendo
`screencam-actual-state` crudo para la tarjeta) del valor que se manda por heartbeat, que
ahora colapsa cualquier cosa que no sea `"running"` a `"stopped"` antes de mandarlo —
`last_error` sigue llevando el detalle de diagnóstico para el caso de error.

Resto del contrato (política con `?id=`, heartbeat con el resto de los campos, WS
`screen_cam.update` aplicado directo sin re-pedir, resync al reconectar) — confirmado ya
implementado, sin cambios.

## Fase 4d — 🔴 bug crítico encontrado y corregido: caché por proceso (27/07)

Reportado en pruebas: encender/apagar ScreenCam desde el panel no tenía ningún efecto —
VLC seguía mostrando señal sin importar el estado del panel, y la tarjeta de Ajustes
seguía mostrando "Apagado" aunque el video estuviera transmitiendo. "Cada quien por su
lado."

**Causa raíz:** el proceso `--server` (donde corre `screen_cam`, capturando y sirviendo
RTSP) y el proceso de la ventana/UI (donde corre Dart y desde donde el usuario ve
Ajustes) son **dos procesos de Windows distintos**. `LocalConfig` (`libs/hbb_common/src/
config.rs`) carga su contenido en memoria **una sola vez por proceso**, al arrancar
(`lazy_static! LOCAL_CONFIG: RwLock<LocalConfig> = RwLock::new(LocalConfig::load())`) —
`get_option()`/`set_option()` operan sobre esa copia cacheada, no releen el archivo en
cada llamada. Cuando Dart (proceso UI) escribe `screencam-licensed` vía
`mainSetLocalOption`, actualiza *su propia* copia en memoria y la persiste en disco —
pero el proceso `--server` nunca se entera, porque nunca vuelve a leer ese archivo. Mismo
problema en la dirección contraria: cuando Rust escribe `screencam-actual-state`, el
proceso UI (que lee eso para el heartbeat y para la tarjeta de Ajustes) sigue viendo su
propia copia vieja.

Este bug afectaba **todo** lo construido en Fase 4b/4c — licencia, `desired_state`, modo,
y el reporte de estado — desde el principio. No se había detectado antes porque hasta
ahora no se había probado el ciclo completo panel→cliente con el panel real.

**Corrección (dos direcciones, sin tocar el bridge de Flutter):**

1. **Panel → captura** (`src/server/screen_cam/mod.rs`): se agregó una caché (`PolicyCache`,
   TTL 2s) que relee `licensed`/`desired_state`/`mode`/`enabled` con
   `LocalConfig::get_option_from_file()` (que sí vuelve a parsear el TOML desde disco en
   cada llamada) en vez de `get_option()` (la copia cacheada). 2 segundos de margen para
   no releer el archivo en cada frame (~10 veces/segundo), suficiente para sentirse
   inmediato para una persona tocando un switch en un panel.
2. **Captura → heartbeat/UI** (`src/common.rs::get_local_option`): la función que ya usa
   el bridge existente (`mainGetLocalOption`, sin tocar su firma) ahora, **solo para
   claves con prefijo `screencam-`**, también usa `get_option_from_file()` en vez de la
   ruta cacheada. El resto de las claves (tema, `access_token`, todo lo demás) siguen
   igual que siempre — cambio acotado a un `if key.starts_with("screencam-")`, sin riesgo
   para el resto de la app.

Con esto, tanto la tarjeta de solo lectura en Ajustes como el heartbeat ven el estado real
que el proceso `--server` está reportando, y el proceso `--server` ve la política que el
panel empuja — con como máximo 2 segundos de demora.

## Fase 4e — 🔴 segundo bug encontrado: el heartbeat con `screen_cam` nunca se mandaba en
Windows (27/07)

Investigando la Fase 4d, se encontró un segundo bug, independiente del anterior. El
`screen_cam` que se agrega al heartbeat vivía únicamente en
`UserModel._sendHeartbeat()` (Dart) — pero `_startHeartbeat()` tiene
`if (!isAndroid) return;`, agregado en un trabajo previo de esta sesión porque el
proceso en segundo plano de Android no siempre sigue vivo (Doze/restricciones del SO),
así que Android necesita un heartbeat manejado por el ciclo de vida de la app Flutter en
sí. **En Windows/desktop, el heartbeat real lo manda un loop nativo de Rust**
(`start_hbbs_sync_async` en `src/hbbs_http/sync.rs`, disparado por
`RendezvousMediator::start_all()` en el mismo proceso `--server` donde vive
`screen_cam`) — ese heartbeat nativo nunca supo nada de `screen_cam`, así que el estado
de ScreenCam **nunca llegó al servidor desde ningún cliente Windows**, incluso después
de arreglar la Fase 4d.

**Corregido** en `src/hbbs_http/sync.rs`: nueva función `screen_cam_status()` (gateada a
`#[cfg(all(windows, feature = "screencam"))]`, con un stub que devuelve `None` en
cualquier otra combinación de plataforma/feature) que arma el mismo bloque `screen_cam`
que ya arma `UserModel._readScreenCamStatus()` en Dart — mismas claves, mismas reglas de
"omitir si está vacío", mismo colapso de `actual_state` a `running`/`stopped`. Se agrega
al `Value` del heartbeat justo antes de mandarlo. Como esta función corre en el mismo
proceso `--server` que `screen_cam::mod.rs`, lee `LocalConfig::get_option` directo (sin
necesidad de `get_option_from_file`) — no hay problema de caché entre procesos acá,
porque no hay dos procesos involucrados en este camino.

## Fase 6 — ONVIF mínimo: descubrimiento + conexión (27/07)

Implementado, alcance acordado con el usuario: WS-Discovery + el mínimo de servicios
ONVIF para que un NVR pase de "lo encontré" a "acá está la URL RTSP" sin que nadie
tipee nada a mano. Sin autenticación (mismo criterio que RTSP hoy), sin PTZ/Events/
Imaging, sin anuncios "Hello" al arrancar (solo contesta Probes activos — cubre el caso
común, un listener puramente pasivo no se enteraría hasta que alguien probe).

**Nuevo:** `src/server/screen_cam/onvif.rs`.

- **WS-Discovery** (UDP 3702, grupo multicast 239.255.255.250): escucha `Probe`,
  contesta `ProbeMatch` con la URL del `device_service`. Si el puerto ya está tomado por
  otra app ONVIF del mismo host, se loguea y se deshabilita el descubrimiento
  solamente — no tira abajo el resto de ScreenCam (mismo criterio de degradación que ya
  se usa en todo el módulo).
- **`device_service`** (SOAP sobre HTTP, puerto nuevo `onvif_port`): `GetDeviceInformation`
  (fabricante "Sehcontrol", modelo "ScreenCam"), `GetCapabilities`, `GetSystemDateAndTime`.
- **`media_service`**: `GetProfiles` (un solo perfil fijo, resolución real una vez que la
  captura arrancó, `1920x1080` de relleno antes de eso), `GetStreamUri` (siempre devuelve
  `/live/main`, sin mirar qué token de perfil pidieron — hay un solo perfil, así que no
  hace falta).
- Sin dependencia nueva: igual que RTSP/SDP, las respuestas SOAP son `format!()` a mano y
  el ruteo de acciones es una búsqueda de substring cruda en el cuerpo de la petición
  (`body.contains("GetProfiles")`, etc.) — no se sumó ningún crate de XML/SOAP.
- `ScreenCamConfig` ganó dos campos: `onvif_port` (default `8080`, deliberadamente no 80
  para no chocar con otra cosa del host — el NVR igual lee la dirección real desde
  `XAddrs`, no asume el puerto) y `device_uuid` (generado una vez con el crate `uuid` ya
  existente, persistido, para que el NVR no vea "un equipo nuevo" cada vez que reinicia).
- Firewall: cubierto gratis, misma regla por-programa que ya cubre el puerto RTSP.

**Pendiente, explícitamente fuera de esta entrega:** autenticación WS-Security en las
llamadas SOAP, servicios PTZ/Events/Imaging/Analytics, anuncios "Hello" proactivos,
substream (`/live/sub`).

---

## Fase 7 — autenticación RTSP con credenciales emitidas por el panel (27/07)

Hasta acá el stream RTSP estaba **abierto**: cualquiera en la LAN que supiera IP y puerto
veía la pantalla. Implementada la autenticación; las credenciales las genera el servidor,
nunca este equipo (mismo flujo unidireccional que el resto de la política de Fase 4 — un
usuario local no puede ampliarse el propio acceso).

**Nuevo:** `src/server/screen_cam/auth.rs`.

- **Ambos esquemas de RFC 2617**, ofrecidos juntos en cada `401` y aceptando cualquiera:
  - **Digest (MD5)** primero — es el que eligen los NVR cuando se les ofrece y no manda la
    contraseña por la red.
  - **Basic** como respaldo, para los NVR baratos y builds viejos de VLC que solo
    implementan eso. Manda la contraseña en base64 (o sea, prácticamente en claro), por
    eso va segundo; sacarlo del todo dejaría afuera hardware que el operador ya puede
    tener.
  - MD5 es obligatorio: no existe NVR que haga Digest-SHA256 sobre RTSP, así que el `sha2`
    que ya estaba en el crate no servía. Se agregó `md-5` a `Cargo.toml`; ya estaba en
    `Cargo.lock` como dependencia transitiva de `stun`/`turn`, así que el cambio del lock
    es **una sola línea** y no baja nada nuevo (importa: todos los builds usan `--locked`).
- **Nonce aleatorio por conexión.** Un cliente que reconecta recibe un desafío nuevo y
  rehace el intercambio — dos round-trips extra, ningún problema funcional. Rechaza
  respuestas calculadas contra un nonce viejo (replay de otra conexión).
- **Comparación en tiempo constante** para que una contraseña incorrecta no se pueda
  reconstruir byte a byte midiendo tiempos.
- **Métodos protegidos:** `DESCRIBE`, `SETUP`, `PLAY`. `OPTIONS` queda abierto a propósito
  (los NVR lo usan para descubrir capacidades antes de que se les pidan credenciales, y no
  revela nada). `GET_PARAMETER`/`TEARDOWN` actúan solo sobre una sesión que el llamador ya
  tiene, y que solo pudo obtener autenticándose.
- **Sin credenciales configuradas = sin autenticación** (fail-open), deliberado: el
  servidor todavía no manda los campos, y exigir auth antes rompería todos los equipos
  desplegados. Se loguea un `warn` cuando el panel las borra, para que quede rastro.
- **Rotación en caliente:** mismas 2s de caché que `PolicyCache`, leyendo con
  `get_option_from_file` — porque quien escribe estas claves es el proceso de UI (Dart) y
  quien las lee es `--server`. Misma trampa entre procesos de la Fase 4d; se cayó también
  en `sync.rs`, donde el heartbeat leía `screencam-rtsp-user` con el `get_option` cacheado
  y le habría reportado al panel "sin auth" para siempre.
- **6 tests unitarios** (`cargo test --features screencam --lib screen_cam::auth`): vectores
  RFC 1321 de MD5, round-trip Digest correcto/incorrecto, digest atado al método (no
  replayable a otro), rechazo de nonce ajeno, round-trip Basic con `:` en la contraseña, y
  rechazo de esquema ausente/desconocido.

**Contrato con el servidor** (documentado en `docs/CLIENT_INTEGRATION.md` §12): dos campos
nuevos `rtsp_user`/`rtsp_password` dentro del bloque `screen_cam` de `/api/client-policy` y
del evento WS `screen_cam.update`; y dos de vuelta en el heartbeat, `auth_enabled` (bool) y
`rtsp_user` — **la contraseña nunca vuelve al servidor**.

**Pendiente, explícitamente fuera de esta entrega:** el ONVIF de la Fase 6 sigue sin
autenticación. Expone metadatos y la *URL* del stream, no el video — quien lo consulte sin
credenciales igual no ve nada. Se dejó así para no arriesgar el auto-descubrimiento de NVR,
que es todo el sentido de esa fase; agregar WS-Security UsernameToken ahí reutilizaría
estas mismas credenciales, sin campos nuevos del lado del servidor.

---

## 1. Veredicto rápido

El plan es viable y el orden de fases propuesto es correcto. La buena noticia es que
**la parte que parecía más difícil ya está resuelta en el código**: captura DXGI,
codificación H.264 por hardware con selección automática de GPU, servicio de Windows,
el salto de sesión 0 a la sesión del usuario, PIN, políticas, heartbeat y WebSocket.

Lo que realmente falta construir para el MVP es **una sola pieza**: un servidor RTSP.
Todo lo demás del MVP es cablear cosas que ya existen.

Pero hay **un problema serio que invalida un supuesto del plan** (no hay H.264 por
software) y **una decisión de arquitectura** (captura compartida) que conviene resolver
ahora y no en la fase 3, porque cambia el diseño.

---

## 2. Fase 0 — Mapa de reutilización

### 2.1 Lo que YA existe y se reutiliza tal cual

| Pieza | Dónde | Estado |
|---|---|---|
| Trait de captura | `libs/scrap/src/common/mod.rs:129` (`TraitCapturer`) | Es exactamente el `ScreenFrameSource` que pedía el plan. Ya existe, no hay que inventarlo. |
| Captura DXGI Windows | `libs/scrap/src/dxgi/` | Con fallback a GDI y a Magnification API. |
| Obtener capturador de un monitor | `src/server/video_service.rs:391` (`get_capturer_monitor`) | Devuelve `CapturerInfo`. Maneja reintentos, cambios de resolución, privacy mode. |
| Multi-monitor / índice de pantalla | `src/server/display_service.rs` | Incluye detección de cambios de displays. |
| Encoder H.264 por hardware | `libs/scrap/src/common/hwcodec.rs:52` (`HwRamEncoder`) | Ya elige el mejor codec disponible vía `CodecInfo::prioritized()`. |
| Selección automática NVENC/QSV/AMF/VAAPI | `libs/hwcodec/src/ffmpeg_ram/encode.rs:201-252` | `h264_qsv`, `h264_nvenc`, `h264_amf`, `h264_vaapi`. |
| Salida del encoder | `libs/scrap/src/common/hwcodec.rs:111` | `EncodedVideoFrame { data, pts, key }` — **NAL units H.264 crudos, justo lo que RTSP necesita**. |
| Servicio de Windows | `src/platform/windows.rs:531` (`start_os_service`), `:3186` (`install_service`) | Ya arranca con Windows y se instala/desinstala. |
| **Salto de sesión 0 → sesión de usuario** | `src/platform/windows.rs:543-556` (`LaunchProcessWin`, `GetSessionUserTokenWin`) + `src/server/portable_service.rs` | Un servicio en sesión 0 **no puede** capturar el escritorio del usuario. RustDesk ya resuelve esto lanzando un helper en la sesión activa y pasando frames por shared memory. **Es el "Capture Helper" del plan y ya está construido.** |
| Reglas de firewall | `src/platform/windows.rs:1548-1549` | Las reglas son **por programa** (`program="{exe}"`), no por puerto. Si el servidor RTSP corre dentro del mismo binario, **no hace falta abrir el puerto a mano**. |
| Protección con PIN | `flutter/lib/desktop/pages/desktop_setting_page.dart:1603` (`unlockPin`), `:3002` (`checkUnlockPinDialog`), `_lock()` | Reutilizable tal cual para proteger los ajustes de ScreenCam. |
| Política de cliente | `/api/client-policy` + `UserModel.fetchForceLogin()` | Construido en esta misma sesión. Se extiende agregando la clave `screen_cam`. |
| Heartbeat | `src/hbbs_http/sync.rs:86` (Rust) y `flutter/lib/models/user_model.dart:180` (Dart) | Ya reporta cada ~15s. Se le agrega el bloque de estado de ScreenCam. |
| WebSocket de órdenes | `flutter/lib/models/user_model.dart:59` (`_realtimeChannel`) | Ya maneja eventos push del panel. Se agregan los tipos `screen_cam.*`. |
| Grabación local a archivo | `libs/scrap/src/common/record.rs` | Ya sabe muxear H.264 a MP4. **No sirve para ScreenCam** (el NVR graba, no nosotros) pero confirma que el pipeline de H.264 está probado. |

### 2.2 Lo que NO existe — hay que construirlo

| Pieza | Esfuerzo | Comentario |
|---|---|---|
| **Servidor RTSP** | Alto — es el corazón del MVP | No hay nada. Ni siquiera una dependencia que sirva (los crates RTSP de Rust son casi todos *clientes*, ej. `retina`). Hay que escribirlo. |
| Empaquetado RTP/H.264 (RFC 6184) | Medio | Fragmentación FU-A, SPS/PPS en el SDP (`sprop-parameter-sets`). |
| Autenticación RTSP (Basic/Digest) | Bajo | Casi todos los NVR la piden. |
| ONVIF (WS-Discovery, perfiles, `GetStreamUri`) | Alto | Fase 6, no antes. |
| Substream (`/live/sub`) | Bajo | Segunda instancia del encoder con otros parámetros. |
| Watchdog / reconciliación desired vs actual | Medio | El modelo `desired_state`/`actual_state` del plan es correcto. |
| Tablas y panel del servidor | Medio | Repo aparte (`rustdesk-admin-panel`). |

---

## 3. Problemas encontrados que cambian el plan

### 3.1 🔴 CRÍTICO — No existe H.264 por software

El plan dice:

```
NVIDIA → NVENC
Intel → Quick Sync
AMD → AMF
Sin aceleración → CPU     ← esto NO existe
```

Verificado: los únicos encoders por software del proyecto son **VPX (VP8/VP9)** y
**AOM (AV1)** (`libs/scrap/src/common/codec.rs:51-58`). El H.264 llega **exclusivamente**
por `hwcodec`, y `hwcodec` sólo expone encoders de **hardware** (`h264_nvenc`, `h264_qsv`,
`h264_amf`, `h264_vaapi` — `libs/hwcodec/src/ffmpeg_ram/encode.rs:201-252`). No hay
`libx264` ni `openh264` en ninguna parte del árbol.

Y VP8/VP9/AV1 **no le sirven a un NVR** — los DVR/NVR del mercado esperan H.264
(algunos H.265). Así que hoy, en una PC sin GPU con encoder de video, ScreenCam
simplemente no puede funcionar.

Hay que decidir entre tres caminos, y **no es una decisión técnica menor, es comercial**:

| Opción | Ventaja | Problema |
|---|---|---|
| **A. Exigir hardware** | Cero trabajo extra. Cubre la gran mayoría de PCs modernas (cualquier Intel con iGPU tiene QSV). | En una PC vieja o una VM sin GPU, el módulo no se puede vender. Hay que detectarlo y decirlo claro en el panel. |
| **B. Agregar `libx264`** | Funciona en todo. Calidad excelente. | **Licencia GPL** — incompatible con vender el cliente como producto cerrado sin liberar el código. Riesgo legal real. |
| **C. Agregar `openh264` (Cisco)** | Licencia BSD + Cisco paga las regalías de patente si se distribuye el binario de ellos. | Calidad menor que x264. Integración más incómoda (hay que descargar el binario de Cisco en runtime para que aplique la cobertura de patentes). |

**Recomendación:** arrancar con **A** para el MVP (es lo que ya funciona), detectar la
ausencia de encoder y reportarla como `last_error: "no_h264_encoder"`, y evaluar **C**
recién si aparecen clientes reales con PCs sin GPU. **Evitar B** salvo que el cliente
se libere como GPL.

> Nota aparte: además del encoder, la distribución comercial de H.264 tiene un tema de
> **regalías de patentes (MPEG LA / Via LA)** que conviene revisar con quien corresponda
> antes de vender esto como producto. No es un problema de código.

### 3.2 🟠 Importante — `hwcodec` no es una feature por defecto

`Cargo.toml:` la feature `hwcodec` no está en `default`. Hay que compilar con
`--features hwcodec` (o `python3 build.py --hwcodec`). Los builds de release del
proyecto ya lo hacen (existe `install_windows_hwcodec.ps1` y el commit
"make Windows hwcodec build reproducible"), así que en la práctica ya está cubierto —
pero un build de desarrollo sin esa flag **no tendrá H.264 y ScreenCam no arrancará**.
Conviene que ScreenCam falle con un mensaje explícito en ese caso, y no con un error raro.

### 3.3 🟠 Importante — la captura hoy sólo corre si hay alguien conectado

`GenericService::run` (`src/server/service.rs:294-304`) sólo ejecuta el ciclo de captura
`if sp.has_subscribes()` — es decir, **cuando hay una sesión remota activa**. ScreenCam
necesita capturar 24/7 sin ningún peer conectado.

Hay dos formas de resolverlo, y conviene elegir ahora:

**Opción 1 — Suscriptor sintético.** ScreenCam se registra como un "subscriber" más del
`video_service` existente. Mínimo código nuevo, y automáticamente comparte la captura
con las sesiones remotas.

**Opción 2 — Servicio de captura propio.** ScreenCam llama directo a
`get_capturer_monitor()` y corre su propio loop.

**Recomendación: Opción 1.** El motivo es el punto siguiente.

### 3.4 🔴 CRÍTICO — no se puede capturar el mismo monitor dos veces

Si ScreenCam abre su propio capturador DXGI mientras hay una sesión remota abriendo otro
sobre el mismo monitor, la duplicación de salida DXGI puede fallar
(`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`) o degradar a GDI, que consume mucha más CPU.

El plan ya intuía la solución correcta al proponer el trait `ScreenFrameSource` con dos
consumidores:

```
Captura RustDesk
   ├── sesión remota
   └── ScreenCam
```

Hay que implementarlo literalmente así: **una sola captura, fan-out del frame a N
consumidores**, cada uno con su propio encoder (la sesión remota puede querer VP9 a 30fps
y ScreenCam H.264 a 5fps — los encoders son distintos, la captura es la misma).

Esto es lo que hace que la Opción 1 del punto anterior sea la correcta: el fan-out vive
naturalmente en el servicio de video que ya existe.

**Esta decisión conviene tomarla antes de la Fase 1**, porque rehacerla en la Fase 3 implica
reescribir el pipeline.

### 3.5 🟡 Menor — detalles que rompen compatibilidad con NVRs

Cosas que en la práctica hacen que un NVR rechace un stream que en VLC se ve perfecto:

- **RTSP sobre TCP (interleaved)** — muchos NVR no usan UDP. Hay que soportar los dos.
- **Autenticación Digest** — varios NVR no aceptan un stream sin usuario/clave.
- **GOP corto** — el plan propone GOP de 2s, correcto. Con GOP largo el NVR tarda en
  empezar a grabar y la reproducción por rango de tiempo falla.
- **SPS/PPS en el SDP** (`sprop-parameter-sets`) y repetidos en el stream — sin esto,
  algunos NVR no arrancan nunca.
- **5 FPS puede ser demasiado bajo** para algunos NVR (esperan ≥10). Conviene que el FPS
  sea configurable desde el arranque y probar 10 si 5 falla.
- **Resolución par y estándar** — 1280×720 está bien; resoluciones raras dan problemas.

---

## 4. Arquitectura ajustada

```
Panel Sehcontrol  ──política/órdenes──►  Cliente Windows
                  ◄──estado/heartbeat──

Cliente Windows (mismo binario, ya existente):
  Servicio (sesión 0)
      └── lanza helper en sesión de usuario   [YA EXISTE]
              └── Captura DXGI única           [YA EXISTE]
                      ├── fan-out → sesión remota (VP9/H264)  [YA EXISTE]
                      └── fan-out → ScreenCam                  [NUEVO]
                                   ├── Encoder H.264 hw        [YA EXISTE]
                                   └── Servidor RTSP           [NUEVO ← el trabajo real]
                                            │
                                            ▼
                                     DVR / NVR de la red
```

El video **nunca** pasa por el servidor de Sehcontrol. El panel sólo administra.

---

## 5. Plan por fases

Mantengo el orden del plan original, que es correcto, con las correcciones de arriba.

### Fase 0 — Auditoría ✅ (este documento)

### Fase 1 — MVP: pantalla → RTSP → VLC
El objetivo es **una sola victoria concreta**: abrir en VLC una URL RTSP servida por
Sehcontrol y ver la pantalla.

1. Refactor de fan-out de la captura (punto 3.4) — **hacerlo primero**.
2. Instanciar `HwRamEncoder` con H.264, 1280×720, 10fps, GOP 2s.
3. Servidor RTSP mínimo: `OPTIONS`/`DESCRIBE`/`SETUP`/`PLAY`/`TEARDOWN`, RTP sobre UDP
   **y** TCP interleaved, packetización FU-A, SDP con `sprop-parameter-sets`.
4. Una sola ruta: `/live/main`. Configuración fija, sin panel, sin PIN, sin licencia.
5. Detección de "no hay encoder H.264" con error explícito.

Criterio de salida: VLC reproduce el stream de forma estable.

### Fase 2 — Prueba contra NVR real
Sin agregar features. Probar contra Hikvision, Dahua y un XVR genérico: agregar el canal,
grabar, reproducir el historial, cortar la red y ver que se recupera. Acá es donde
aparecen los problemas del punto 3.5. **No avanzar a la Fase 3 hasta que un NVR real
grabe y reproduzca.**

### Fase 3 — Persistencia y control local
Arranque con Windows, watchdog, reconexión, configuración local en la UI, PIN
(reutilizando `checkUnlockPinDialog`), modos local/administrado/supervisión permanente.

### Fase 4 — Licencia y panel
`screen_cam` en `/api/client-policy`, tablas de módulos por plan/cliente/dispositivo,
reconciliación `desired_state`/`actual_state`, reporte de estado en el heartbeat, órdenes
`screen_cam.*` por WebSocket, auditoría y alertas.

### Fase 5 — Main + substream
`/live/main` y `/live/sub`.

### Fase 6 — ONVIF
WS-Discovery, perfiles de medios, `GetStreamUri`, autenticación.

### Fase 7 — Producto
Instalador, documentación, matriz de compatibilidad por marca de NVR.

---

## 6. Criterios de aceptación del MVP

Los del plan original son buenos. Los dejo con dos agregados (7 y 11):

1. VLC abre el stream durante 8 horas sin cortes.
2. El NVR lo agrega como cámara RTSP.
3. El NVR graba correctamente.
4. Se puede reproducir el historial.
5. Reiniciar Windows recupera el stream solo.
6. Desconectar y reconectar la red recupera el stream.
7. Cerrar la ventana de Sehcontrol **no** detiene el stream.
8. **Una sesión remota activa y ScreenCam funcionan al mismo tiempo, sin degradar la
   captura ni disparar el fallback a GDI.** *(agregado — es el riesgo del punto 3.4)*
9. Un usuario estándar de Windows no puede detenerlo ni reconfigurarlo.
10. Uso de CPU aceptable con 720p/10fps.
11. **En una PC sin encoder H.264 por hardware, el módulo reporta un error claro en el
    panel en vez de fallar en silencio.** *(agregado — es el riesgo del punto 3.1)*

---

## 7. Decisiones pendientes antes de escribir código

1. **Encoder por software**: ¿opción A (exigir hardware), B (x264/GPL) o C (openh264)?
   → Recomendado: **A** para el MVP.
2. **Fan-out de captura**: confirmar que se hace en la Fase 1 y no después.
3. **Regalías de H.264** para distribución comercial: revisar con quien corresponda.
4. **FPS por defecto**: 5 (plan original) o 10 (más compatible con NVRs). Probar ambos
   en la Fase 2.
5. **Autenticación RTSP**: ¿obligatoria desde el MVP o desde la Fase 3? Recomendado:
   dejar el hook en la Fase 1, activarla en la Fase 3.
