# Integración del cliente RustDesk con el panel de membresías

Mensaje para quien vaya a tocar el fork del cliente (`rustdesk/rustdesk`, Flutter + Rust).
Este documento describe qué implementar del lado del cliente para que la app:

1. Al abrir por primera vez, muestre un modal obligatorio de login (no se puede usar la app sin loguearse, si el servidor lo pide).
2. Una vez logueado, revise periódicamente el estado de la membresía (activa / por vencer / suspendida / vencida) y muestre avisos o bloquee funciones en consecuencia.

El servidor y el panel de administración (Node/Express) ya están implementados y probados —
lo que falta es **exclusivamente del lado del cliente**.

> Nota de honestidad: no compilamos ni tocamos el código del cliente Flutter/Rust en este
> trabajo (es un proyecto aparte, con su propio toolchain). Todo lo de abajo se investigó
> leyendo el código fuente público de `rustdesk/rustdesk` (rama `master`) para dar referencias
> exactas de archivo/línea, pero no fue compilado ni ejecutado.

---

## 1. Qué ya existe en el servidor (no toca implementar esto, ya funciona)

Base URL: la que el usuario configura como **API Server** en el cliente (`Settings → Network`).

### 1.1 Protocolo estándar de RustDesk (ya lo habla el cliente stock, sin cambios)

| Endpoint | Método | Auth | Uso |
|---|---|---|---|
| `/api/login-options` | GET | no | Lista de opciones SSO/OIDC. Devuelve `[]` (solo login usuario/clave). |
| `/api/login` | POST | no | Login con usuario/clave. Body: `{username, password, id, uuid, autoLogin, type, deviceInfo}`. Devuelve `{access_token, type, tfa_type, secret, user}`. Si la cuenta está suspendida, responde `400 {"error":"Cuenta suspendida"}`. |
| `/api/logout` | POST | Bearer | No-op (JWT sin estado). |
| `/api/currentUser` | POST | Bearer | Devuelve el `UserPayload` actual. 401 si el token es inválido → el cliente resetea la sesión local (`UserModel.reset`). |
| `/api/ab` | GET/POST | Bearer | Libreta de direcciones "legacy" (lista plana de equipos reclamados por el usuario). |
| `/api/sysinfo` | POST | no (por id/uuid) | Info del equipo: hostname, os, cpu, memoria, usuario del sistema. |
| `/api/heartbeat` | POST | no (por id/uuid) | Cada ~15s mientras el cliente esta abierto (con o sin conexiones activas). |

Durante `POST /api/login`, el servidor también registra la identidad del
dispositivo enviada por Android:

- `id` se usa como ID RustDesk y para asociar el dispositivo a la cuenta.
- `uuid` se conserva como identificador persistente adicional.
- `deviceInfo.name` se guarda como `hostname`.
- `deviceInfo.os` se guarda como `platform`.
- El email autenticado se guarda como `username`.

Estos valores completan posteriormente los peers devueltos por `GET /api/ab`,
aunque Android no haya llamado todavía a `/api/sysinfo`.

`POST /api/sysinfo` acepta tanto los nombres estándar como las variantes de
Android: `hostname`/`device_name`/`name`, `username`/`user_name` y
`os`/`platform`. Los campos ausentes no borran información ya guardada.
En `GET /api/ab`, `platform` se normaliza a `Android`, `Windows`, `Linux` o
`Mac OS`; `username` usa como respaldo el email propietario de la cuenta.

El login también actualiza `last_heartbeat_at` como primera señal de presencia.
Para mantener el estado **En línea**, el cliente debe continuar enviando
`POST /api/heartbeat` aproximadamente cada 15 segundos mientras esté abierto.
Sin esos heartbeats el servidor mostrará el dispositivo como **Offline**,
no como **No visto**.

Referencia de implementación real que ya usa el cliente stock, sin ningún cambio:
`flutter/lib/models/user_model.dart` (`login()`, `refreshCurrentUser()`, `logOut()`) y
`flutter/lib/common/hbbs/hbbs.dart` (`LoginRequest`, `LoginResponse`, `UserPayload`).

**Novedad — esto tampoco requiere ningún cambio en el cliente, y no lo sabíamos hasta ahora:**
el cliente stock, SIN NINGÚN parche, ya llama solo a `/api/sysinfo` y `/api/heartbeat` en cuanto
detecta un `api-server` configurado que no sea `rustdesk.com` (ver `src/common.rs::is_public` y
`src/hbbs_http/sync.rs::start_hbbs_sync_async` del repo `rustdesk/rustdesk`). Nunca los
escuchábamos del lado servidor. Ya los implementamos (`src/routes/hbbsHttp.js` del panel) y
ahora el panel muestra hostname, sistema operativo, CPU, memoria, usuario del sistema, y un
estado "En línea/Offline" real (basado en el heartbeat, no en lo que hbbs sabe — hbbs por sí
solo no guarda eso). Probado con datos simulados end-to-end. **No hay ningún checklist nuevo
para el cliente por esto** — es puramente del lado servidor.

**Truco ya activo, sin tocar el cliente:** el campo `user.display_name` se usa para inyectar
un aviso de texto (`⚠ Plan vence en 3 dia(s)`, `⚠ Cuenta suspendida`, `⚠ Plan vencido`). Ese
texto ya aparece hoy en **Ajustes → Account** (`flutter/lib/desktop/pages/desktop_setting_page.dart:2118`,
widget `_Account`, texto `gFFI.userModel.displayNameOrUserName`) porque ese campo ya existe en
el protocolo estándar. Es un parche de bajo esfuerzo; lo que sigue abajo es la versión "real"
con UI dedicada.

### 1.2 Endpoints nuevos, NO estándar (agregados para este proyecto)

El cliente stock nunca los llama; agregarlos no rompe nada. Son los que el cliente modificado
debe empezar a usar.

**`GET /api/client-policy`** — sin autenticación. Se consulta al arrancar la app, antes de
saber si el usuario está logueado.

```json
{
  "force_login": true,
  "server_key": {
    "algorithm": "Ed25519",
    "public_key": "yWgYPUl5u5oWdp2CpxBIyTm1MS8s++A4icIWDbpnJlo=",
    "fingerprint_sha256": "<sha256-hex>",
    "updated_at": "2026-07-23T00:00:00.000Z"
  }
}
```

Si `force_login` es `true` y el usuario no tiene sesión activa, la app debe mostrar el modal
de login de forma bloqueante antes de dejar usar cualquier otra función.

**`GET /api/public/server-key`** — sin autenticación. Devuelve solamente `server_key` con
el mismo formato, para que el cliente pueda volver a consultarla sin descargar toda la
política.

> Mensaje para el desarrollador del cliente: la primera clave debe venir incluida en la
> aplicación o descargarse mediante HTTPS con certificado validado. No reemplaces
> silenciosamente una clave guardada usando una respuesta HTTP no autenticada, porque un
> atacante podría sustituir tanto el servidor como la clave. En una LAN confiable se puede
> permitir el alta inicial por HTTP con confirmación visible del usuario. Guarda
> `public_key` en la opción RustDesk `key`; usa `fingerprint_sha256` para detectar cambios.
> Cuando llegue `server_key_changed`, vuelve a consultar `/api/public/server-key`, valida
> la confianza y aplica la clave antes de reconectar. El cliente oficial de RustDesk no
> consume este endpoint: esta lógica debe agregarse al cliente personalizado.

**`GET /api/membership/status`** — requiere `Authorization: Bearer <access_token>`. Pensado
para sondeo periódico (polling) mientras la app está abierta.

```json
{
  "blocked": false,
  "reason": null,
  "message": "Cuenta activa",
  "plan_name": "Pro",
  "plan_expires_at": "2026-07-27T21:44:42.463Z",
  "days_left": 5,
  "device_count": 2,
  "max_devices": 5,
  "plan_amount": 1500000,
  "plan_currency": "ARS",
  "plan_amount_formatted": "$15.000 ARS",
  "last_payment": {
    "amount": 1500000,
    "currency": "ARS",
    "amount_formatted": "$15.000 ARS",
    "concept": "Primer pago",
    "paid_at": "2026-07-23 03:49:44"
  }
}
```

`reason` es `"suspended"`, `"expired"` o `null`. `blocked: true` significa que el servidor
(`hbbs`, parchado) ya está rechazando activamente cualquier intento de conectarse a los
equipos de este usuario — este endpoint es solo para que la UI se entere y avise, la
aplicación real del bloqueo ya ocurre en el servidor de señalización pase lo que pase en
el cliente (ver sección 4, "Importante: qué es UX y qué es seguridad real").

**Nuevo — precio y último pago (respuesta a la consulta del 23/07):** decidimos exponer
**ambos**, con nombres separados, en vez de uno solo:

- `plan_amount` / `plan_currency` / `plan_amount_formatted`: precio del **plan vigente**
  (lo que cuesta la membresía que tiene activa ahora mismo). `plan_amount_formatted` ya viene
  con símbolo y separador de miles armado del lado servidor (`$15.000 ARS`) — mostrarlo tal
  cual, no reimplementar formato de moneda/locale en el cliente.
- `last_payment`: el **último pago realmente registrado** (puede no coincidir con el precio
  del plan — adelantos parciales, descuentos, etc.). Viene `null` si el usuario nunca tuvo
  un pago registrado.

Si el usuario no tiene plan asignado, los tres vienen `null`. Si el plan existe pero su precio
es `0` (ej. un plan gratuito), `plan_amount_formatted` viene poblado igual (`"$0 USD"`) — el
único caso donde `plan_amount_formatted` es `null` es "no hay plan asignado". Tratar
`plan_amount_formatted: null` como "no mostrar la línea de precio".

**`GET /api/messages`** — requiere Bearer. Devuelve avisos automáticos de vencimiento y
mensajes manuales que mande el administrador (nuevo: el panel ahora tiene una sección
"Alertas / Mensajes" donde el admin puede escribirle a un cliente puntual o a todos).
Query opcional `?unread=1` para traer solo los no leídos.

```json
[
  {
    "id": 12,
    "type": "custom",
    "title": "Mantenimiento programado",
    "message": "El servicio estará caído el sábado de 2am a 4am.",
    "created_at": "2026-07-22 22:36:15",
    "read": 0
  }
]
```

`type` es uno de `expiry_warning | expired | suspended | payment_received | custom`.
Los primeros tres son automáticos (el servidor corre un chequeo cada hora); `payment_received`
se genera solo cuando el admin registra un pago; `custom` es un mensaje manual del admin.

**`POST /api/messages/:id/ack`** — requiere Bearer, sin body. Marca ese mensaje como leído
para el usuario logueado (si es un mensaje broadcast a todos los clientes, marcarlo leído
solo afecta a quien lo marcó, no a los demás destinatarios).

---

## 2. Qué implementar en el cliente

### 2.1 Login obligatorio al abrir la app (primera pantalla)

**Punto de enganche exacto:** `flutter/lib/main.dart:136`, función `runMainApp(bool startService)`
(la usa el desktop) y su equivalente `runMobileApp()` un poco más abajo en el mismo archivo.
Ambas ya llaman a `gFFI.userModel.refreshCurrentUser();` justo antes de `runApp(App());`.

Ese es el lugar natural para insertar la lógica nueva:

```dart
// pseudo-código, dentro de runMainApp() antes de runApp(App())
final policy = await fetchClientPolicy(); // GET {api_server}/api/client-policy
await gFFI.userModel.refreshCurrentUser(); // ya existe
if (policy.forceLogin && !gFFI.userModel.isLogin) {
  // bloquear hasta que el login resuelva en exito
  bool loggedIn = false;
  while (!loggedIn) {
    final res = await loginDialog(); // ya existe: flutter/lib/common/widgets/login.dart:456
    loggedIn = gFFI.userModel.isLogin;
  }
}
runApp(App());
```

`loginDialog()` (`flutter/lib/common/widgets/login.dart:456`) ya existe completo y funcional
— ya lo estamos usando hoy tal cual, sin cambios, para el login manual desde Ajustes. Solo hay
que forzar su apertura al arranque en vez de esperar a que el usuario lo abra desde el menú.

**Detalle a resolver en la implementación real (no lo puedo verificar sin compilar):**
`gFFI.dialogManager` probablemente necesita que el árbol de widgets ya esté montado
(`runApp` ya ejecutado) para poder mostrar el diálogo. Si `loginDialog()` falla al llamarse
antes de `runApp()`, la alternativa es moverlo a un `WidgetsBinding.instance.addPostFrameCallback`
inmediatamente después de `runApp(App())`, o dentro del callback
`windowManager.waitUntilReadyToShow(...)` que ya existe en `main.dart` líneas ~155-168 — ahí ya
se hace lógica de mostrar/ocultar la ventana principal, es un buen candidato alternativo.

Si `api_server` no está configurado (deployment sin panel de membresías), `fetchClientPolicy()`
debe fallar en silencio y comportarse como hoy (login opcional) — no forzar nada si no hay
servidor de membresías configurado.

### 2.2 Sondeo periódico de estado de membresía + banner/modal

**Dónde vive el estado de usuario:** `flutter/lib/models/user_model.dart`, clase `UserModel`.
Ahí ya están `userName`, `displayName`, `isLogin`, etc. como `Rx` (observables de GetX).

Agregar:

```dart
// nuevos campos observables en UserModel
final RxBool membershipBlocked = false.obs;
final RxString membershipMessage = ''.obs;
final RxnInt daysLeft = RxnInt();

Timer? _membershipTimer;

void startMembershipPolling() {
  _membershipTimer?.cancel();
  _membershipTimer = Timer.periodic(Duration(minutes: 5), (_) => checkMembershipStatus());
  checkMembershipStatus(); // primera corrida inmediata
}

Future<void> checkMembershipStatus() async {
  if (!isLogin) return;
  final url = await bind.mainGetApiServer();
  final token = bind.mainGetLocalOption(key: 'access_token');
  final resp = await http.get(Uri.parse('$url/api/membership/status'),
      headers: {'Authorization': 'Bearer $token'});
  if (resp.statusCode != 200) return; // fallar en silencio, no molestar si el server no responde
  final data = jsonDecode(resp.body);
  membershipBlocked.value = data['blocked'] == true;
  membershipMessage.value = data['message'] ?? '';
  daysLeft.value = data['days_left'];
}
```

Llamar `startMembershipPolling()` justo después de un login exitoso (dentro de `login()` en
`user_model.dart`, después de `_parseAndUpdateUser`) y también al arrancar si ya había sesión
guardada (dentro de `refreshCurrentUser()`, en la rama de éxito).

**Intervalo sugerido:** 5 minutos. El panel ya sincroniza el bloqueo real hacia `hbbs` cada
5 minutos también (ver `src/membershipSync.js` del panel), así que no tiene sentido sondear
más seguido que eso — el dato del lado servidor no cambia más rápido.

**UI del aviso:** un banner no bloqueante (tipo `MaterialBanner` o una barra fija arriba de
`desktop_home_page.dart`) cuando `daysLeft <= 7 && !blocked`, y un modal bloqueante (reusar
el mismo mecanismo de `gFFI.dialogManager.show`) cuando `blocked == true`, con el texto de
`membershipMessage` y un botón "Entendido" (no hay nada más que puedan hacer desde la app,
el desbloqueo lo hace el administrador desde el panel).

### 2.3 Bloquear el botón "Conectar" cuando la cuenta está suspendida

**Dónde vive el botón:** `flutter/lib/desktop/pages/desktop_home_page.dart`, la sección que
arma el `TextFormField` de `model.serverId` y el botón de conectar (buscar `onConnect` /
`connect(` en ese archivo — es el mismo widget que ves en la captura de pantalla, la caja
"Controlar escritorio remoto").

Envolver la acción del botón:

```dart
onPressed: gFFI.userModel.membershipBlocked.value
    ? () => showMembershipBlockedDialog(gFFI.userModel.membershipMessage.value)
    : () => connect(...), // logica existente
```

Esto es puramente cosmético/UX (evita que el usuario pierda tiempo intentando conectar sabiendo
que va a fallar) — el bloqueo real ya lo hace `hbbs` en el servidor sin importar qué haga el
cliente (ver siguiente sección).

### 2.4 Centro de mensajes/notificaciones (avisos automáticos + mensajes del admin)

Independiente del banner de membresía (2.2), esto es un canal más general: el admin puede
mandar mensajes ad-hoc ("mantenimiento programado", "cambio de precios", etc.) además de los
avisos automáticos de vencimiento.

Reusar el mismo `Timer.periodic` de 5 minutos de la sección 2.2 (o uno propio) para llamar
`GET /api/messages?unread=1`. Si hay mensajes nuevos:

- Mostrar un ícono de campana con contador en la barra superior (patrón común, no hay un
  widget existente exacto para reusar — es UI nueva).
- O, más simple para una primera versión: un `showToast()` (ya se usa en varios lados del
  código, ej. `common.dart`) por cada mensaje nuevo, y llamar
  `POST /api/messages/{id}/ack` apenas se muestra.
- Vale la pena mostrar `title` en negritas y `message` como cuerpo del toast/notificación.

No hay que reinventar gran cosa acá: es la misma idea que un feed de notificaciones simple,
solo mostrar lo que el endpoint devuelve.

### 2.5 Tiempo real (WebSocket) — reemplaza el polling de 2.2/2.4

**Esto es nuevo y es la mejora importante de esta versión del documento.** Antes, mensajes y
cambios de membresía solo se enteraban por sondeo cada 5 minutos. Ahora el servidor tiene un
WebSocket que empuja los eventos **al instante** en que ocurren (probado end-to-end: un mensaje
mandado desde el panel, o una cuenta suspendida, le llega a un cliente conectado en menos de
un segundo, sin ningún polling de por medio).

**No elimines el polling de las secciones 2.2/2.4** — déjalo como respaldo de baja frecuencia
(o bájalo a cada 10-15 minutos) por si el WebSocket se cae y no reconecta a tiempo. El
WebSocket es la vía primaria; el polling es la red de seguridad.

**Endpoint:** `ws://{api_server}/api/ws?token={access_token}` (o `wss://` si el panel está
detrás de TLS). El token es el mismo `access_token` que ya guardan en `LocalConfig` tras el
login — va como query param porque el handshake de WebSocket en la mayoría de los clientes no
deja mandar headers custom con facilidad; en Flutter con `web_socket_channel` sí se pueden
mandar headers, pero usar query param es más simple y ya está probado así.

Conectar justo después de un login exitoso (mismo lugar que `startMembershipPolling()`) y
también si ya había sesión guardada al arrancar:

```dart
// pseudo-código, agregar a UserModel
WebSocketChannel? _wsChannel;

void connectRealtimeChannel() async {
  _wsChannel?.sink.close();
  final url = await bind.mainGetApiServer();
  final token = bind.mainGetLocalOption(key: 'access_token');
  if (token.isEmpty) return;
  final wsUrl = url.replaceFirst('http', 'ws') + '/api/ws?token=$token';
  _wsChannel = WebSocketChannel.connect(Uri.parse(wsUrl));
  _wsChannel!.stream.listen(
    (raw) => _handleRealtimeEvent(jsonDecode(raw)),
    onDone: () => Future.delayed(Duration(seconds: 5), connectRealtimeChannel), // reconectar
    onError: (_) => Future.delayed(Duration(seconds: 5), connectRealtimeChannel),
  );
}

void _handleRealtimeEvent(Map<String, dynamic> event) {
  switch (event['type']) {
    case 'connected':
      inspectAndApplyServerKey(event['server_key']);
      break;
    case 'server_key_changed':
      refetchValidateAndApplyServerKey();
      break;
    case 'membership_status':
      final d = event['data'];
      membershipBlocked.value = d['blocked'] == true;
      membershipMessage.value = d['message'] ?? '';
      if (membershipBlocked.value) showBlockedDialogIfNotShown();
      break;
    case 'message':
      final d = event['data'];
      showToast('${d['title']}: ${d['message']}');
      ackMessage(d['id']); // POST /api/messages/{id}/ack, ya documentado en 2.4
      break;
  }
}
```

Cerrar el canal (`_wsChannel?.sink.close()`) en `logOut()`.

**Formato de los eventos que manda el servidor** (implementación de referencia:
`src/ws.js` del repo `rustdesk-admin-panel`):

```json
{ "type": "connected", "server_key": { "algorithm": "Ed25519", "public_key": "...", "fingerprint_sha256": "...", "updated_at": "..." } }
```


```json
{ "type": "server_key_changed", "data": { "algorithm": "Ed25519", "public_key": "...", "fingerprint_sha256": "...", "updated_at": "..." } }
```


```json
{ "type": "membership_status", "data": { "blocked": true, "reason": "suspended", "message": "Tu cuenta esta suspendida. Contacta al administrador." } }
```


```json
{ "type": "message", "data": { "id": 5, "type": "custom", "title": "Prueba en vivo", "message": "...", "created_at": "2026-07-23T02:53:54.680Z" } }
```

Es un canal **solo servidor→cliente** (push). No hace falta mandar nada de vuelta salvo,
opcionalmente, un `"ping"` de texto plano cada cierto tiempo si algún proxy/NAT corta
conexiones inactivas — el servidor responde `{"type":"pong"}`.

---

## 3. Endpoints nuevos: resumen para copiar/pegar

```
GET  {api_server}/api/client-policy          (sin auth)
GET  {api_server}/api/public/server-key      (sin auth)
GET  {api_server}/api/membership/status      (Authorization: Bearer <token>)
GET  {api_server}/api/messages               (Authorization: Bearer <token>)
GET  {api_server}/api/messages?unread=1      (Authorization: Bearer <token>)
POST {api_server}/api/messages/:id/ack       (Authorization: Bearer <token>)
WS   {api_server}/api/ws?token={token}       (tiempo real, ver 2.5)
```

Implementación de referencia (ya corriendo) en el panel:
`src/routes/clientExtensions.js` del repo `rustdesk-admin-panel`.

---

## 4. Importante: qué es UX y qué es seguridad real

Todo lo de este documento (modal de login, banners, deshabilitar el botón conectar, el
WebSocket de 2.5 incluido) es **cosmético**. Un cliente modificado por un usuario malicioso
(o simplemente una versión vieja del cliente sin este parche, o uno que simplemente nunca
abre la conexión WebSocket) puede ignorar `/api/client-policy`, `/api/membership/status` y
`/api/ws` por completo y seguir usando la app con normalidad — el WebSocket solo entrega el
aviso más rápido, no agrega ninguna restricción que no estuviera ya.

La única protección que **no se puede saltar desde el cliente** ya está implementada del lado
del servidor: `hbbs` (el servidor de señalización, parchado — ver `src/rendezvous_server.rs`
y `src/database.rs` del repo `rustdesk-server`) rechaza cualquier intento de **conectarse a**
un equipo cuyo dueño esté suspendido o con el plan vencido, sin importar qué cliente se use
ni si tiene este parche o no. Eso ya está probado end-to-end (ver historial de este proyecto).

Lo de este documento es exclusivamente para que el usuario **entienda por qué** no puede
conectarse, en vez de encontrarse un error genérico de conexión fallida.

---

## 5. Checklist para quien implemente esto

- [ ] `GET /api/client-policy` al arranque, antes de `runApp()` (o inmediatamente después si
      `dialogManager` lo requiere).
- [ ] Validar y guardar `server_key.public_key` como opción RustDesk `key`; fijar la primera
      clave mediante HTTPS, clave incluida en la app o confirmación explícita del usuario.
- [ ] Forzar `loginDialog()` en loop hasta login exitoso si `force_login == true` y no hay sesión.
- [ ] Si no hay `api_server` configurado, no forzar nada (comportamiento actual sin cambios).
- [ ] `UserModel.startMembershipPolling()` tras login exitoso y tras `refreshCurrentUser()` exitoso.
- [ ] Timer de 5 minutos llamando `GET /api/membership/status`.
- [ ] Banner no bloqueante si `days_left <= 7`.
- [ ] Modal bloqueante (solo informativo, con botón "Entendido") si `blocked == true`.
- [ ] Deshabilitar/interceptar el botón "Conectar" de `desktop_home_page.dart` si `blocked == true`.
- [ ] Sondeo de `GET /api/messages?unread=1` como respaldo (bajar frecuencia a 10-15 min si ya
      hay WebSocket funcionando), mostrar toast/notificación por cada uno, `ack` al mostrarlo.
- [ ] Conectar `ws://{api_server}/api/ws?token={access_token}` tras login/al arrancar con sesion
      guardada, con reconexion automatica si se cae.
- [ ] Manejar eventos `membership_status` (actualizar banner/modal al instante) y `message`
      (toast + ack) que llegan por el WebSocket.
- [ ] Manejar `server_key_changed`: volver a consultar la clave, validarla y reconectar.
- [ ] Cerrar el WebSocket en `logOut()`.
- [ ] Probar contra el panel real: `docs/CLIENT_INTEGRATION.md` mismo repo trae los endpoints
      corriendo en `http://<ip-del-servidor>:8899`.

---

## 6. Novedades del lado del servidor (modo profesional del panel)

Desde la última versión de este documento, el panel de administración agregó, **sin requerir
ningún cambio adicional de tu parte** más allá de lo ya descrito arriba:

- **Pagos y adelantos**: el admin registra pagos (efectivo/transferencia/tarjeta) por usuario,
  opcionalmente extendiendo `plan_expires_at` por una cantidad de días exacta (cubre tanto
  renovaciones completas como adelantos parciales). Esto ya se refleja automáticamente en
  `GET /api/membership/status` (nuevo `days_left`, `plan_expires_at`) sin que el cliente tenga
  que saber nada de pagos — el cliente solo ve el resultado (más días antes de vencer).
- **Alertas automáticas**: un cron corre cada hora en el servidor y genera avisos de
  vencimiento (7, 3 y 1 día antes), vencido, y suspendido — estos son los que llegan por
  `GET /api/messages` con `type: "expiry_warning" | "expired" | "suspended"`.
- **Mensajes manuales y broadcast**: el admin puede escribir mensajes libres desde el panel
  (a un usuario o a todos) — llegan por el mismo `GET /api/messages` con `type: "custom"`.
- **Email opcional**: si el admin configura SMTP en el panel, las alertas también se mandan
  por correo — esto es 100% del lado servidor, no afecta nada de lo que implemente el cliente.

En resumen: todo lo nuevo del lado servidor cae dentro de los mismos tres endpoints ya
documentados (`membership/status`, `messages`, `messages/:id/ack`). No hay endpoints
adicionales que agregar para soportar pagos o alertas — ya están cubiertos.

---

## 7. Respuesta a los 5 casos borde (23/07)

Gracias por implementar con criterio propio en vez de bloquearse esperando confirmación — los
5 supuestos que tomaron son correctos o quedan resueltos así:

**1. `days_left` ausente/null en plan sin vencimiento → sin banner de "por vencer".**
Correcto tal cual lo implementaron. `days_left` viene `null` cuando `plan_expires_at` es
`null` (usuario sin plan asignado, o un plan pensado como "sin vencimiento" — hoy no existe
un plan literalmente ilimitado en el modelo de datos, pero un admin puede simular uno con
`duration_days` muy largo, ej. el plan "Free" ya viene con 3650 días). Ningún cambio necesario.

**2. `/api/client-policy` inalcanzable → falla abierto (`force_login` efectivo `false`).**
Es el comportamiento que queremos, no lo cambien. El login (`force_login`) es una capa de UX
para que el usuario entienda su situación — la protección real (que nadie controle un equipo
de una cuenta suspendida/vencida) ya la hace `hbbs` de forma independiente, sin importar si el
panel de membresías está arriba o no en ese instante. Bloquear la app entera por una caída
momentánea del panel sería peor UX sin ganar nada en seguridad real.

**3. `/api/membership/status` con 401 → hoy el cliente ignora y no desloguea.**
Aquí sí pedimos que lo ajusten: **alinéenlo con el comportamiento de `/api/currentUser`** — un
401 en cualquiera de los dos significa lo mismo (token inválido o expirado), así que debería
forzar el mismo reset de sesión local (`UserModel.reset`) en ambos casos. No es intencional
que solo `currentUser` dispare logout; fue simplemente el único que documentamos con ese
detalle en su momento.

**4. WebSocket 24/7 + ping cada 30s + polling de respaldo cada 15 min.**
Sin objeción — para el volumen de dispositivos esperado (deployments propios, no a escala de
`rustdesk.com`) esto es trivial para el servidor. Si en algún momento manejan miles de
dispositivos conectados simultáneamente, aviser y ahí sí conviene revisar (connection pooling,
mover el WS a un proceso aparte, etc.), pero no hace falta anticiparlo ahora.

**5. `message` vacío/ausente cuando `blocked: true` → texto de respaldo genérico.**
Confirmado: en la implementación actual `message` **siempre** viene poblado cuando
`blocked: true` (dos únicas causas posibles — suspendida o vencida — y ambas setean un texto
fijo no vacío antes de responder). Pueden seguir teniendo el fallback como red de seguridad,
pero no deberían necesitarlo en la práctica contra este servidor.

---

## 8. Corrección importante: tags "Cabinas" y "Clientes" (23/07)

**`/api/ab/tag/*` NO existe en este servidor** — esos endpoints (`/api/ab/tags/:guid`,
`/api/ab/tag/add/:guid`, `/api/ab/tag/rename/:guid`, etc.) son parte del protocolo de
address book **compartido/con grupos**, que este servidor deja intencionalmente en 404
(`/api/ab/personal` devuelve 404 → el cliente cae a "legacy mode", ver sección 1.1). Si el
cliente está llamando a `/api/ab/tag/*` contra este servidor, esas llamadas están fallando en
silencio con 404 — vale la pena revisar que efectivamente esté entrando en legacy mode.

**Cómo funcionan los tags en legacy mode (`LegacyAb`, ya implementado y probado):**
No hay endpoints por acción. El cliente sube el estado **completo** de su libreta (alias +
tags de cada equipo) en cada `POST /api/ab`, igual que ya hace para los alias — no hace falta
ningún endpoint nuevo de nuestro lado, en eso tenían razón, pero el mecanismo es el `POST /api/ab`
que ya estaba, no `/api/ab/tag/*`.

```json
// dentro de "data" (string JSON) de POST /api/ab
{
  "tags": ["Cabinas"],
  "peers": [{ "id": "485236790", "alias": "Roy", "tags": ["Cabinas"] }],
  "tag_colors": "{}"
}
```

**Respuesta a las dos preguntas:**

- **¿Ya existen "Cabinas" y "Clientes" en las cuentas actuales?** Sí, ahora sí — `GET /api/ab`
  siempre los incluye en el `tags` de nivel superior, aunque el usuario no tenga ningún equipo
  todavía (antes devolvíamos `null` con la libreta vacía; ahora devolvemos la estructura
  completa con `peers: []` para que los dos tags reservados se vean desde el primer momento).
- **¿El servidor impide borrarlos/renombrarlos?** Sí, a nivel servidor, no solo de UI: el
  `tags` de nivel superior que manda el cliente en el `POST` se **ignora a propósito** — el
  servidor siempre recalcula y devuelve `["Cabinas", "Clientes", ...cualquier otro tag en uso]`
  sin importar qué mande el cliente. Probado explícitamente: mandamos un `POST` con
  `tags: ["Cabinas"]` (omitiendo "Clientes" a propósito) y el siguiente `GET` igual devolvió
  ambos. La asignación de tags **por equipo** (`peers[].tags`) sí es libre, como debe ser —
  solo la existencia de los dos tags reservados está protegida.

Implementación de referencia: `src/routes/hbbsHttp.js` (`RESERVED_TAGS`, handlers de
`/api/ab`) del repo `rustdesk-admin-panel`.

---

## 9. Auto-asociación de equipo al hacer login + límite de cupos (23/07)

**Esto no requiere ningún cambio de su lado — es informativo.** El cliente ya manda todo lo
necesario (`id` del equipo en `POST /api/login`, y `id` en `POST /api/logout`), así que el
comportamiento nuevo es transparente.

**Qué cambió:**

- Al loguearse, si el `id` del equipo (que ya mandan en el login) no tiene dueño todavía, el
  servidor lo asocia automáticamente a la cuenta que inició sesión — ya no hace falta que el
  usuario entre al panel web a "reclamar" el equipo a mano.
- Si el usuario ya alcanzó el límite de equipos de su plan y el equipo desde el que intenta
  loguearse es uno nuevo (no reclamado antes), el login se **rechaza** (sin emitir
  `access_token`) con:
  ```json
  { "error": "Alcanzaste el limite de 1 equipo(s) de tu plan. Cierra sesion en otro equipo para poder ingresar aqui." }
  ```
  Este mensaje ya llega por el mismo `error` que manejan hoy en `login()` (`RequestException`),
  así que debería mostrarse solo con el flujo que ya tienen.
- Al hacer logout, el equipo se **libera automáticamente**, dejando el cupo disponible para
  otro equipo. Probamos el ciclo completo: login desde equipo A (se reclama), login desde
  equipo B con el plan ya lleno (rechazado), logout de A (libera cupo), login de B (ahora sí
  funciona), login de A otra vez (ahora rechazado porque B ocupa el cupo) — todo se comportó
  como se esperaba.
- Si el equipo ya está asociado a **otra** cuenta (no la que intenta loguearse), el login
  también se rechaza con `"Este equipo ya esta asociado a otra cuenta. Contacta al administrador."`

Implementación de referencia: `src/deviceClaim.js` (lógica compartida de reclamar/liberar) y
`src/routes/hbbsHttp.js` (`/api/login`, `/api/logout`) del repo `rustdesk-admin-panel`.

---

## 10. Nuevo campo requerido: `deviceInfo.machine_id` (26/07)

Mensaje para quien toque el fork del cliente, tanto en Android (Kotlin/Rust) como en
desktop (Windows/Linux/macOS, Flutter + Rust):

**El problema que resuelve:** hoy, si un usuario desinstala RustDesk y borra los datos
locales, al reinstalar el cliente genera un `id` de RustDesk completamente nuevo. El
servidor no tiene forma de saber que es la misma máquina física, así que la trata como un
equipo nuevo: el registro anterior queda huérfano/obsoleto y, si el plan tiene cupo
limitado, puede incluso rechazar el reingreso por "límite de equipos alcanzado" cuando en
realidad es el mismo equipo de siempre.

**Lo que necesitamos que mande el cliente:** un identificador de hardware **estable**, que
sobreviva a la desinstalación/reinstalación, dentro del mismo objeto `deviceInfo` que ya se
manda en `POST /api/login` (junto a `name` y `os`):

```jsonc
{
  "username": "...",
  "password": "...",
  "id": "123456789",      // el rustdesk id, como ya se manda hoy
  "uuid": "...",           // ya existe, solo Android
  "deviceInfo": {
    "name": "DESKTOP-ABC123",
    "os": "Windows 11",
    "machine_id": "a1b2c3d4e5f6..."   // <-- NUEVO campo a agregar
  }
}
```

`machine_id` debe ser un **hash** (SHA-256 en hex, por ejemplo) del identificador de
hardware real, no el valor crudo — es lo mismo que hace cualquier telemetría que respeta
privacidad, y evita mandar en claro un identificador de máquina que podría usarse para
otros fines. El servidor lo trata como un string opaco: no necesita decodificarlo, solo
compararlo por igualdad.

**De dónde sacar el identificador crudo antes de hashear, por plataforma:**

- **Windows:** `MachineGuid` del registro, en
  `HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Cryptography\MachineGuid`. Es estable entre
  reinstalaciones de cualquier software (sobrevive porque vive en el registro de Windows,
  no en la carpeta de datos de la app) y solo cambia con un reinstall de Windows.
- **Linux:** contenido de `/etc/machine-id` (o `/var/lib/dbus/machine-id` como respaldo si
  el primero no existe). Estándar de facto en systemd, generado una vez por instalación de
  SO.
- **macOS:** `IOPlatformUUID`, obtenible vía
  `ioreg -rd1 -c IOPlatformExpertDevice | grep IOPlatformUUID` (o la API `IOKit`
  equivalente en Rust/Objective-C). Estable mientras no se reemplace la placa lógica.
- **Android:** no hace falta agregar nada nuevo — el campo `uuid` que ya se manda
  (Android `Settings.Secure.ANDROID_ID` o equivalente) ya cumple el mismo rol y el
  servidor ya lo guarda. Opcionalmente pueden mandar el mismo valor también como
  `deviceInfo.machine_id` para unificar el campo entre plataformas, pero no es
  obligatorio: el servidor sigue leyendo `uuid` para Android.

**Qué hace el servidor con esto (ya implementado, corriendo hoy):** en `POST /api/login`,
si el `id` que llega no tiene dueño todavía, antes de tratarlo como equipo nuevo el
servidor busca si esa cuenta ya tiene un equipo reclamado con el mismo `machine_id`. Si lo
encuentra, **reasigna** ese registro (alias, tags, historial) al nuevo `id` en vez de crear
uno nuevo — no consume un cupo adicional del plan y el equipo sigue apareciendo con el
mismo alias/tag de siempre en la libreta de direcciones. Si no hay `machine_id` (cliente
viejo sin este parche, o el campo viene vacío), el comportamiento es exactamente el de
antes: se trata como equipo nuevo.

**Importancia de mandarlo siempre que se pueda:** aunque el matching es "mejor esfuerzo" y
no rompe nada si falta, mandarlo en *todos* los logins (no solo el primero) es lo que
permite detectar la reinstalación. No hace falta mandarlo en `/api/sysinfo` ni
`/api/heartbeat` — con que viaje en `POST /api/login` alcanza, porque es el único momento
en que el servidor decide si el equipo es "nuevo" o no.

Implementación de referencia: `src/deviceClaim.js`
(`findDeviceByMachineId`, `migrateDeviceId`) y `src/routes/hbbsHttp.js`
(`normalizeMachineId`, handler de `/api/login`) del repo `rustdesk-admin-panel`.

---

## 11. Nuevo campo requerido: `whatsapp_number` en `/api/client-policy` (27/07)

**El problema que resuelve:** el número de WhatsApp de soporte estaba **hardcodeado en el
cliente** (`flutter/lib/desktop/pages/support_sidebar.dart`) — cambiar el número exigía una
release nueva. Además, se agregó un aviso de vencimiento con botón de WhatsApp en el header
de la app (`flutter/lib/common.dart::buildMembershipBanner`) que necesita el mismo número.

**Lo que necesitamos que agreguen a `GET /api/client-policy`** (mismo endpoint que ya
devuelve `force_login`/`server_key`/`screen_cam`, sin autenticación):

```json
{
  "force_login": true,
  "server_key": { "...": "..." },
  "whatsapp_number": "51948793154",
  "screen_cam": { "...": "..." }
}
```

- `whatsapp_number`: string, **sin el `+`** (ej. `"51948793154"`, no `"+51948793154"`) — el
  cliente arma el link `https://wa.me/{whatsapp_number}` directamente con ese valor.
- Si no lo mandan o viene vacío/ausente, el cliente **oculta** el botón de WhatsApp en los
  dos lugares donde se usa (no muestra un número viejo/incorrecto como respaldo) — así que
  no rompe nada un deployment que todavía no lo haya configurado.
- Es un valor único, global de la instalación del panel (no depende de la cuenta ni del
  plan) — un solo número de soporte para todos los clientes de este servidor.
- Se lee una sola vez por consulta a `client-policy` (arranque de la app, y cada vez que el
  WebSocket reconecta — ya lo hacíamos para `screen_cam`), así que un cambio del número
  tarda como máximo hasta la próxima reconexión en propagarse a un cliente ya abierto.

**Dónde lo usa el cliente:**
1. `Ajustes → Soporte → WhatsApp` (enlace directo a la conversación).
2. Banner de aviso de vencimiento (header de la app), botón "Contactar por WhatsApp" —
   solo visible cuando el plan está por vencer (`days_left <= 7`, mismo umbral que ya
   usan para `expiry_warning`).

Implementación de referencia esperada: agregar `whatsapp_number` a la config global del
panel (tabla de settings/configuración de instancia) y devolverlo tal cual en el handler
de `GET /api/client-policy`, junto al resto de los campos que ya arma ese endpoint.

## 12. Nuevos campos requeridos: credenciales RTSP de ScreenCam (27/07)

**El problema que resuelve:** hasta ahora el stream RTSP de ScreenCam
(`rtsp://{local_ip}:{rtsp_port}/live/main`) estaba **abierto**: cualquiera en la misma
red LAN que supiera la IP y el puerto podía ver la pantalla del equipo sin credenciales.
Ya implementamos autenticación RTSP en el cliente (Basic + Digest MD5, RFC 2617), pero
**las credenciales las tiene que generar y enviar el servidor** — el cliente no las
inventa ni permite editarlas localmente, igual que el resto de la política de ScreenCam.

**Lo que necesitamos que agreguen al bloque `screen_cam` de `GET /api/client-policy`**
(y al mismo bloque del evento WebSocket `screen_cam.update`, que el cliente ya procesa
con el mismo código):

```json
{
  "screen_cam": {
    "licensed": true,
    "desired_state": "running",
    "mode": "managed",
    "max_streams": 1,
    "rtsp_user": "seh_a1b2c3",
    "rtsp_password": "K7pQ2mVx9nR4",
    "rtsp_port_override": null,
    "onvif_port_override": null
  }
}
```

### Reglas de los puertos (opcional, sólo si hace falta moverlos)

El cliente ya escucha en 554 (RTSP) y 80 (ONVIF), que es lo que un Dahua o un
Hikvision asumen cuando el operador da de alta el equipo por IP. Estos dos campos
existen sólo para el caso en que esos puertos estén ocupados en la máquina.

- `rtsp_port_override` / `onvif_port_override`: entero 1-65535, o `null`.
- **`null` o ausente = sin override**, el equipo usa 554/80. No es lo mismo que 0:
  un 0 se descarta como inválido, porque un puerto efímero no le sirve a un NVR.
- **El nombre lleva `_override` a propósito.** El bloque `screen_cam` del
  *heartbeat* ya usa `rtsp_port` con otro significado — ahí es el puerto que el
  equipo **reporta** estar usando, y el panel lo guarda para construir la
  `rtsp_url` que muestra en la ficha. Si la política reutilizara ese nombre,
  ambos sentidos acabarían en la misma columna y el primer heartbeat del equipo
  pisaría el valor que eligió el administrador.
- **Cambiarlos requiere reiniciar el servicio del equipo**: el puerto se lee una
  sola vez al arrancar, a diferencia de las credenciales, que se releen cada 2 s.

### Reglas de los dos campos nuevos

- `rtsp_user`: string. Es el usuario que el operador escribirá en el DVR/NVR o en VLC.
- `rtsp_password`: string. Idem para la contraseña.
- **Ambos por dispositivo, no por cuenta.** Lo ideal es una credencial distinta por
  equipo: así revocar/rotar un equipo comprometido no obliga a reconfigurar los demás
  NVR de ese cliente. Si por ahora les resulta más simple una por cuenta, funciona igual
  — el cliente no asume nada sobre cómo las generan.
- **Generación:** que las genere el panel, aleatorias. Sugerencia: usuario
  `seh_` + 6-8 caracteres hex, contraseña de 12+ caracteres alfanuméricos.
  **Eviten `:` en el usuario** (Basic auth separa usuario y contraseña con `:`, y el
  cliente parte en el *primer* `:`, así que un `:` en el usuario rompería el login; en la
  contraseña sí está soportado, pero mejor evitarlo también). Sin espacios ni comillas.
- **Ausente/`null`/vacío = sin autenticación.** El cliente lo trata como "el panel todavía
  no configuró credenciales" y deja el stream abierto, igual que hoy. Es deliberado para
  no romper los equipos ya desplegados antes de que ustedes suban este cambio — **pero
  significa que hasta que lo implementen, el stream sigue sin protección.**
- **Borrarlas debe funcionar:** si mandan `null`/`""`, el cliente **apaga** la
  autenticación (no se queda con el último par que recibió). Ya está implementado así,
  aprendiendo del caso de `whatsapp_number`.
- **Rotación en caliente:** el cliente re-lee las credenciales cada 2 segundos desde su
  configuración local, así que un cambio en el panel se aplica sin reiniciar la app ni el
  servicio. Las conexiones RTSP ya establecidas siguen vivas hasta que el NVR reconecte
  (no cortamos sesiones activas al rotar).

### Lo que el cliente les devuelve en el heartbeat

Al bloque `screen_cam` de `POST /api/heartbeat` le agregamos dos campos, para que el panel
pueda confirmar que las credenciales que emitió efectivamente llegaron al equipo:

```json
{
  "screen_cam": {
    "actual_state": "running",
    "encoder": "h264_nvenc",
    "last_error": null,
    "rtsp_clients": 1,
    "local_ip": "192.168.0.3",
    "rtsp_port": 8554,
    "auth_enabled": true,
    "rtsp_user": "seh_a1b2c3"
  }
}
```

- `auth_enabled`: bool. `false` significa que ese equipo tiene el stream **abierto**.
  Si el panel cree que le mandó credenciales y el equipo reporta `false`, es una
  desconfiguración que conviene mostrarle al admin.
- `rtsp_user`: string, solo el usuario. **La contraseña nunca vuelve al servidor.**

### Qué implementa el cliente (ya hecho, para su referencia)

- `src/server/screen_cam/auth.rs` — nuevo. Digest MD5 y Basic; ambos se ofrecen en cada
  desafío `401` y se acepta cualquiera de los dos (Digest primero, porque es el que
  eligen los NVR y no manda la contraseña en claro; Basic queda como respaldo para
  equipos baratos que solo implementan eso). Nonce aleatorio por conexión, comparación
  en tiempo constante.
- Se exige autenticación en `DESCRIBE`, `SETUP` y `PLAY`. `OPTIONS` queda abierto a
  propósito (los NVR lo usan para descubrir capacidades antes de tener credenciales y no
  revela nada).
- El operador puede seguir usando la forma habitual `rtsp://usuario:clave@ip:8554/live/main`.

### Pendiente que conviene que sepan

El **ONVIF sigue sin autenticación** (`src/server/screen_cam/onvif.rs`). Expone metadatos
del dispositivo y la *URL* del stream, pero **no el video** — quien consulte ONVIF sin
credenciales igual no puede ver nada sin el usuario/clave de arriba. Lo dejamos así a
propósito en esta etapa para no arriesgar el auto-descubrimiento de NVR (que es todo el
sentido de la Fase 6); agregar WS-Security UsernameToken ahí es el paso natural siguiente
y no requiere ningún campo nuevo de su lado: reutilizaría estas mismas credenciales.
