# Diagnóstico de ScreenCam SRT → MediaMTX → WHEP

Fecha: 2026-08-01  
Equipo observado: Windows, ScreenCam 1920×1080, `h264_nvenc`, 10 fps configurados  
Servidor observado: MediaMTX v1.9.3  
Alcance: diagnóstico solamente; no se modificó producción ni el cliente instalado.

## Conclusión

La pantalla negra no se origina en SRT, MPEG-TS, ICE ni en la conectividad del
navegador. La publicación llega a MediaMTX y la sesión WHEP se establece.

La causa primaria exacta es una incompatibilidad entre el perfil H.264 real y
el perfil anunciado por WebRTC:

1. El SPS producido por ScreenCam empieza por `67 4D 40 28`.
2. Sin el byte de cabecera NAL (`67`), el `profile-level-id` real es `4D4028`:
   H.264 Main, nivel 4.0, 1920×1080.
3. MediaMTX v1.9.3 crea su pista WebRTC con
   `profile-level-id=42e01f` fijo: Constrained Baseline, nivel 3.1.
4. En la negociación real, Chrome ofreció Main (`4d001f`), pero la respuesta
   normal de MediaMTX seleccionó Constrained Baseline (`42e01f`).
5. MediaMTX no transcodifica el H.264. Por tanto, el RTP conserva el bitstream
   Main mientras el SDP le promete al decodificador que recibirá Constrained
   Baseline. La conexión y los contadores RTP pueden avanzar, pero Chrome no
   obtiene cuadros decodificados.

El código oficial exacto de MediaMTX v1.9.3 confirma el valor fijo en
[`internal/protocols/webrtc/from_stream.go`](https://github.com/bluenviron/mediamtx/blob/v1.9.3/internal/protocols/webrtc/from_stream.go#L192-L204).

Hay un segundo defecto confirmado que agrava el primero y por sí solo puede
dejar negro a un lector que se conecte tarde: ScreenCam no garantiza un IDR
periódico en tiempo de reloj cuando la pantalla permanece quieta.

## Evidencia reproducida

### Transporte y servidor

- El cliente recibió el comando, abrió SRT y publicó en la ruta temporal.
- MediaMTX declaró la ruta `ready: true` y detectó una pista `H264`.
- Se crearon lectores WHEP y MediaMTX informó que estaban leyendo esa pista.
- La respuesta WHEP fue HTTP 201 y la PeerConnection llegó a `connected`.
- El elemento `<video>` permaneció sin dimensiones y sin primer cuadro.
- El panel cerró y recreó la sesión WHEP aproximadamente cada 6 segundos,
  conforme a `DEFAULT_FIRST_FRAME_TIMEOUT_MS = 6000`.

Esto descarta como causa primaria:

- fallo de DNS actual;
- rechazo de autenticación;
- ausencia de conexión SRT;
- ausencia de pista H.264 en MediaMTX;
- fallo de ICE/DTLS;
- falta de respuesta WHEP.

### SPS real

SPS:

```text
67 4D 40 28 95 90 07 80 22 7E 5C 05 A8 30 30 32
00 00 07 D0 00 01 D4 C1 C0 00 00 FD 0C 00 00 FD 0D
77 79 70 50
```

PPS:

```text
68 EB 8F 20
```

Campos decodificados:

| Campo | Valor |
|---|---:|
| `profile_idc` | 77 / `0x4D` (Main) |
| restricciones | `0x40` |
| `level_idc` | 40 / `0x28` (nivel 4.0) |
| ancho | 1920 |
| alto | 1080 |
| `frame_mbs_only_flag` | 1 |
| recorte inferior | 4 unidades, hasta 1080 |

### SDP real

Chrome ofreció, entre otros, estos perfiles H.264 con
`packetization-mode=1`:

```text
42001f
42e01f
4d001f
64001f
```

Respuesta normal de MediaMTX:

```text
m=video 9 UDP/TLS/RTP/SAVPF 109
a=rtpmap:109 H264/90000
a=fmtp:109 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f
a=sendonly
```

Se hizo una negociación de diagnóstico ofreciendo únicamente Main. MediaMTX
respondió correctamente:

```text
m=video 9 UDP/TLS/RTP/SAVPF 117
a=rtpmap:117 H264/90000
a=fmtp:117 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f
a=sendonly
```

Esto demuestra que el navegador y MediaMTX pueden negociar Main y que una
preferencia de codecs bien ordenada evita la selección Baseline incorrecta.

## Defecto adicional: cadencia e IDR no acotados

ScreenCam configura:

```text
fps = 10
gop_size = fps * 2 = 20 imágenes codificadas
```

Sin embargo, el bucle solo codifica cuando DXGI entrega una imagen nueva. Si
`Capturer::frame()` devuelve `WouldBlock`, no se vuelve a codificar el último
fotograma. La documentación de Microsoft confirma que
`IDXGIOutputDuplication::AcquireNextFrame` entrega un cuadro cuando cambia la
imagen del escritorio o el puntero; si no hay una actualización, vence el
tiempo de espera:

- [AcquireNextFrame](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe)
- [Desktop Duplication API](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)

El GOP se mide en imágenes codificadas, no en segundos de reloj. FFmpeg define
`gop_size` como el número de imágenes de un GOP:

- [AVCodecContext::gop_size](https://www.ffmpeg.org/doxygen/trunk/structAVCodecContext.html)

Evidencia en esta reproducción:

- durante un muestreo de 20 segundos, `bytesReceived` permaneció idéntico
  durante 13 segundos;
- luego llegaron solo dos ráfagas, de 9.104 y 1.584 bytes;
- un lector RTSP local no recibió ninguna unidad durante más de 80 segundos
  con la pantalla quieta;
- un lector SRT aceptado por MediaMTX tampoco recibió una unidad antes del
  timeout;
- el panel abandona cada lector a los 6 segundos si no existe primer cuadro.

Así, el primer IDR que vuelve lista la ruta puede ocurrir antes de que Chrome
termine de conectarse. MediaMTX no conserva ese cuadro para el siguiente
lector y no puede convertir el PLI de WebRTC en una solicitud de IDR al
publicador SRT. Con una pantalla estática, el siguiente IDR puede tardar un
tiempo indefinido.

El propio proyecto MediaMTX recomienda I-frames periódicos para este síntoma:
[discusión oficial #2538](https://github.com/bluenviron/mediamtx/discussions/2538).

## Corrección implementada para validación

Las correcciones siguientes están aplicadas únicamente en ramas/worktrees
limpios. No se modificaron los contenedores, la configuración ni el cliente
instalado de producción.

### 1. Corregir primero la negociación H.264

Cambio mínimo para el despliegue actual:

- conservar la referencia devuelta por `pc.addTransceiver('video', ...)`;
- antes de `createOffer()`, usar `RTCRtpReceiver.getCapabilities('video')`;
- llamar a `transceiver.setCodecPreferences()` priorizando H.264 Main,
  `packetization-mode=1`, `profile-level-id=4d001f`.

La prueba aislada demuestra que MediaMTX responde entonces con Main. Esta
corrección es pequeña y está localizada en el reproductor del panel.

Estado: implementado en `feature/screencam-whep-profile-fix`. El helper conserva
todos los codecs de respaldo y solo cambia el orden cuando el navegador anuncia
H.264 Main con `packetization-mode=1`. Pruebas Node: 3/3 aprobadas.

Corrección estructural preferible para equipos heterogéneos:

- hacer que el cliente informe el `profile-level-id` real extraído del SPS y
  que el panel ofrezca primero ese perfil; o
- modificar/actualizar MediaMTX para derivar su capacidad H.264 del SPS en vez
  de anunciar `42e01f` fijo; o
- imponer en todos los encoders ScreenCam un perfil WebRTC único y comprobado.

No conviene fijar Main globalmente sin probar QSV/AMF/VAAPI, porque otro equipo
podría producir Baseline o High.

### 2. Garantizar acceso aleatorio en tiempo de reloj

Mientras exista una previsualización:

- mantener la última imagen capturada;
- entregarla al encoder a la cadencia configurada aunque DXGI no reporte
  cambios;
- garantizar SPS + PPS + IDR como máximo cada 1–2 segundos de reloj.

Solo reducir `gop_size` no basta: si no se codifican imágenes, tampoco se
alcanza el siguiente GOP.

Estado: implementado en `feature/screencam-srt-preview`. En `WouldBlock`, el
bucle vuelve a codificar la última imagen YUV a la cadencia configurada
únicamente mientras el tap de preview está activo. Sin preview, el
comportamiento anterior permanece intacto.

### 3. No declarar el publicador listo demasiado pronto

Actualmente el cliente emite `screen_cam.preview.started` inmediatamente
después del handshake SRT, antes de enviar PAT/PMT y el primer SPS/PPS/IDR.
Debe existir un estado distinto:

```text
SRT conectado → primer AU decodificable enviado → ruta multimedia lista
```

El panel debería empezar WHEP en el último estado. Esto elimina reintentos
prematuros, aunque no reemplaza las dos correcciones anteriores.

## Errores y riesgos secundarios documentados

### RTSP anuncia un `profile-level-id` incorrecto

`src/server/screen_cam/rtsp.rs` toma `sps[0..3]`, incluyendo el byte de cabecera
NAL `0x67`. Produce:

```text
profile-level-id=674D40
```

Debe tomar los tres bytes posteriores a la cabecera NAL:

```text
profile-level-id=4D4028
```

Este defecto afecta SDP/RTSP y NVR; no explica por sí solo la negociación WHEP,
que MediaMTX genera por separado.

### Los logs de MediaMTX en nivel debug contienen secretos

El log de producción incluye cabeceras HTTP, cookies y tokens WHEP completos.
Durante este diagnóstico se redactaron antes de mostrarlos. Debe bajarse el
nivel de log o incorporarse redacción de:

- `Cookie`;
- `Authorization`;
- parámetros `token`;
- URLs de reproducción firmadas.

### El contador de cupo está incoherente

El panel mostró:

```text
ScreenCam: 3/2 en uso
```

Debe revisarse el cálculo/limpieza de usos activos y sesiones expiradas.

### La sesión web y la sesión del cliente son independientes

El primer intento quedó en `waiting_client` porque la aplicación de escritorio
tenía un JWT vencido, aunque el panel web estaba autenticado y el heartbeat del
servicio seguía activo. El sistema no distingue claramente “equipo conectado”
de “canal de usuario capaz de recibir comandos”.

Conviene:

- mostrar estado separado del canal de comandos;
- renovar la sesión del cliente;
- no crear una sesión de preview si `pushToUser` no tiene destinatario activo.

### Errores DNS transitorios

Los logs locales registraron `Host desconocido` para las consultas de membresía
y mensajes. No estaban presentes durante la reproducción exitosa de SRT, por
lo que no son la causa de la pantalla negra.

### Configuración de GOP contradictoria para algunos encoders

La utilidad común establece un `gop_size` finito y, simultáneamente,
`keyint_min = INT_MAX`. FFmpeg define `keyint_min` como el GOP mínimo. NVENC
puede ignorarlo, pero debe verificarse por encoder; no se debe cambiar sin una
prueba controlada porque el código es compartido con otros flujos.

## Estado de implementación y pruebas

- Cliente ScreenCam: commit `43928c649` en
  `feature/screencam-srt-preview`.
- Panel/WHEP y recuperación del código avanzado desplegado: commit
  `6be236d` en `feature/screencam-whep-profile-fix`, worktree limpio
  `/home/ubuntu/server-sehcontrol-screencam-fix`.
- Pruebas Node del orden de codecs y su integración antes de `createOffer()`:
  4 aprobadas, 0 fallidas.
- Suite Rust `server::screen_cam`: 289 aprobadas, 0 fallidas, 1 ignorada
  porque abre un socket UDP real.
- `rustfmt --check` del archivo modificado y `git diff --check`: aprobados.
- Build de prueba Windows con `--features screencam`: aprobado.
- Ejecutable de prueba:
  `artifacts/screencam-build/sehcontrol-screencam-cadence-debug.exe`.
- SHA-256 del ejecutable:
  `BB632DD63BF992EF7F9B37D9D83BF16312C915CC7770B579B2B898D589938A2F`.

En ese punto producción seguía sin cambios y faltaba la validación end-to-end
controlada. El despliegue autorizado posteriormente se registra a continuación.

## Despliegue de producción (2026-08-01)

El usuario autorizó posteriormente instalar las correcciones.

- Panel desplegado:
  `sehcontrol-panel:2026.08.01.1-screencam-amd64`.
- Imagen anterior conservada:
  `sehcontrol-panel:2026.07.30.1-amd64`,
  ID `sha256:ac75ec81393835ed534f285840f94666a291349fe9a865041edf49299b9532e5`.
- Respaldo del compose:
  `/home/ubuntu/server-sehcontrol/compose.yaml.bak-before-screencam-fix-20260801`.
- El panel quedó `running/healthy`. HBBS, HBBR y MediaMTX no fueron
  reiniciados.
- DLL ScreenCam instalado:
  SHA-256 `1E6897BA6C169001548524AD45DF07586288BA3E4EB1C6BEDFC7DF3CB9D8084B`.
- Respaldo del DLL anterior:
  `C:\Program Files\Sehcontrol\sehcontrol.dll.bak-before-screencam-20260801`,
  SHA-256 `1FDB0A3C828EAFA79B08635F845438AB46B240803C64ED70BE3ACB07CB573A8C`.
- Servicio Sehcontrol: `RUNNING`.

La primera validación creó dos sesiones que quedaron en `waiting_client`
porque, tras reiniciar el cliente, el canal WebSocket del usuario no estaba
autenticado. Ambas sesiones se finalizaron y MediaMTX quedó con cero rutas.
Hace falta iniciar sesión una vez en la aplicación de escritorio y repetir la
validación de cuadros.

## Pruebas de aceptación para la corrección

1. El `profile-level-id` de la respuesta WHEP pertenece al mismo perfil que el
   SPS real.
2. `getStats()` muestra `framesDecoded > 0` y `framesRendered > 0`.
3. El primer cuadro aparece en menos de 2 segundos.
4. Un lector que se conecta tras 60 segundos de pantalla estática también
   obtiene un cuadro en menos de 2 segundos.
5. El flujo mantiene SPS/PPS/IDR periódicos en tiempo de reloj.
6. SRT, RTSP y el NVR existente no sufren regresiones.
7. Los logs no contienen cookies, JWT ni tokens de publicación/lectura.
8. La UI no puede mostrar un uso superior al cupo sin explicar el exceso.
