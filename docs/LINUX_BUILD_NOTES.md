# Notas de compilación y ejecución en Linux (Ubuntu)

Apuntes específicos del build Flutter/Linux (`python3 build.py --flutter`) y del
comportamiento en segundo plano en el escritorio. Complementa a `CLAUDE.md`.

## Comportamiento al cerrar la ventana (segundo plano)

En Linux "quedarse en segundo plano" **no** es la ventana escondida: es un diseño
de 3 procesos.

| Proceso | Rol |
|---|---|
| GUI (`sehcontrol`, sin args) | La ventana. Al cerrarla se llama `windowManager.hide()` (`setPreventClose(true)`), **no** se sale. |
| `--service` → `--server` | El servicio de fondo (systemd `sehcontrol.service`, `User=root`) que lanza `--server` como el usuario del escritorio. Es lo que acepta conexiones remotas. |
| `--tray` | El icono de bandeja; el **único** medio para restaurar la ventana escondida. |

Punto clave: la GUI solo lanza `--tray` si detecta un `--server` en marcha
(`core_main.rs`, `check_process("--server", ...)`). Por tanto, si el servicio
está parado: no hay `--server` (no se puede conectar desde otro equipo) **ni**
`--tray` (no hay icono), y cerrar la ventana deja la app sin ventana y sin icono
→ parece que "se cierra del todo". El código de cierre es correcto; simplemente
no hay bandeja que mostrar.

Diagnóstico y arreglo: asegurarse de que el servicio corre.

```bash
systemctl status sehcontrol.service
sudo systemctl start sehcontrol.service   # si está inactive
```

El unit lleva `Restart=on-failure` / `RestartSec=2` (ver `res/sehcontrol.service`)
para revivir el servicio si el proceso `--service` crashea; una parada manual
(`systemctl stop`) se sigue respetando.

## Puntos pendientes / problemas conocidos

1. **El `.deb` no declara dependencia de appindicator.** El icono de bandeja usa
   `libayatana-appindicator3` (cargado en runtime por el crate `tray_icon`). En
   Ubuntu GNOME funciona porque el sistema ya lo trae, pero en una instalación
   limpia el icono no aparecería. Pendiente: añadir `libayatana-appindicator3-1`
   a los `Depends` del paquete (ver `build_flutter_deb` / `generate_control_file`
   en `build.py` y `res/DEBIAN/`).

2. **Bug latente al ocultar la ventana bajo GNOME/Wayland.** En el journal del
   servicio aparece, al ocultar la ventana del Connection Manager:

   ```
   PlatformException(window_destroyed, The window has been destroyed)
       ← WindowManager.setOpacity ← hideCmWindow (flutter/lib/main.dart:405)
   Attempted to set message handler on an FlBinaryMessenger without an engine
   ```

   La ventana se **destruye** en vez de ocultarse. Sospecha: el hack de
   `delete-event` del fork `rustdesk-org/window_manager` (desconecta el handler
   por defecto de Flutter buscándolo por el dato `fl_view`) no aguanta en Flutter
   3.24.5, de modo que el `GtkApplicationWindow` se destruye. Con el `--tray`
   activo el efecto práctico es menor, pero conviene revisarlo. Pendiente de
   investigación a fondo.

## Notas de compilación (build Flutter/Linux)

Bloqueos no obvios encontrados al compilar en Ubuntu (GCC 15, clang/llvm-21) y
sus soluciones:

- **`src/bridge_generated.rs` (flutter_rust_bridge).** Está gitignored y se
  genera. Instalar `flutter_rust_bridge_codegen 1.80.1` (feature `uuid`),
  activar `ffigen 5.0.1`, hacer `flutter pub get` en `flutter/`, y generar con
  `flutter_rust_bridge_codegen --rust-input ./src/flutter_ffi.rs --dart-output
  ./flutter/lib/generated_bridge.dart`. Con el codegen de crates.io (el proyecto
  usa el fork `SoLongAndThanksForAllThePizza/flutter_rust_bridge`) el archivo
  generado necesita dos parches manuales en el bloque "DUMMY CODE FOR BINDGEN":
  añadir `pub type Dart_Handle = *const core::ffi::c_void;` y borrar las 5
  funciones dummy `#[no_mangle]` (`store_dart_post_cobject`, `get_dart_object`,
  `drop_dart_object`, `new_dart_opaque`, `init_frb_dart_api_dl`), que duplican
  símbolos reales de `allo-isolate`/`dart-sys`. **Estos parches se pierden al
  regenerar el bridge.**

- **`webm-sys` / libwebm.** `mkvparser.cc` usa `uint64_t` sin incluir
  `<cstdint>` (GCC 13+ ya no lo arrastra por inclusiones transitivas). Se
  soluciona con `CXXFLAGS="-include cstdint"` (en el `.cargo/config.toml` local).

- **`hwcodec` (`--hwcodec`).** El overlay-port de ffmpeg del proyecto compila con
  `--disable-swresample` (`res/vcpkg/ffmpeg/portfile.cmake`), pero
  `libs/hwcodec/build.rs` intentaba enlazar `swresample` estático → *could not
  find native static library `swresample`*. Como el código de hwcodec no usa
  ningún símbolo `swr_*`, se dejó de enlazar `swresample` en Linux (Windows/macOS
  intactos).
