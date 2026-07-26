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
