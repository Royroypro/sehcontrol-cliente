<p align="center">
  <img src="res/logo-header.svg" alt="Sehcontrol" width="520">
</p>

<h1 align="center">Sehcontrol Cliente</h1>

<p align="center">
  Cliente de escritorio remoto personalizado para la plataforma Sehcontrol.
</p>

<p align="center">
  <a href="https://sehcontrol.sehuacho.com">Sitio web</a> ·
  <a href="#compilación">Compilación</a> ·
  <a href="#estructura-del-proyecto">Estructura</a> ·
  <a href="#créditos-y-licencia">Créditos</a>
</p>

## Acerca de Sehcontrol

Sehcontrol permite acceder y brindar asistencia remota a equipos mediante una
aplicación de escritorio integrada con los servicios de Sehcontrol.

Esta edición incorpora, entre otras adaptaciones:

- Identidad visual y experiencia de usuario de Sehcontrol.
- Servidores ID y relay preconfigurados para la plataforma.
- Integración con cuentas, membresías, planes y equipos.
- Notificaciones y accesos al portal de Sehcontrol.
- Configuración protegida del servicio y soporte para rotación de claves.

> [!CAUTION]
> Sehcontrol debe utilizarse únicamente en equipos propios o con autorización
> expresa de su propietario. No se admite el acceso no autorizado, la invasión
> de privacidad ni ningún uso ilegal del software.

## Requisitos

El proyecto utiliza Rust para el núcleo de la aplicación y Flutter para la
interfaz actual. Para compilarlo se necesitan:

- Git con soporte para submódulos.
- Rust y Cargo.
- Flutter compatible con el proyecto.
- Herramientas de compilación de C/C++ para la plataforma de destino.
- Las dependencias nativas indicadas por RustDesk para Windows, Linux o macOS.

## Obtener el código

El submódulo `libs/hbb_common` es obligatorio. Clone el repositorio usando:

```bash
git clone --recurse-submodules https://github.com/Royroypro/sehcontrol-cliente.git
cd sehcontrol-cliente
```

Si el repositorio ya fue clonado sin submódulos:

```bash
git submodule update --init --recursive
```

## Publicar un Release

El script `scripts/publish-release.ps1` crea la etiqueta, activa GitHub Actions,
espera la publicación y puede adjuntar archivos al Release. Requiere
[GitHub CLI](https://cli.github.com/) con una sesión iniciada mediante
`gh auth login`.

Para publicar la compilación Windows existente:

```powershell
.\scripts\publish-release.ps1 v1.5.0 -PackageWindows
```

Para adjuntar uno o varios instaladores específicos:

```powershell
.\scripts\publish-release.ps1 v1.5.0 `
  -AssetPath .\dist\Sehcontrol-Setup.exe, .\dist\Sehcontrol.msi
```

Antes de ejecutarlo, la rama `master` debe estar limpia y completamente
publicada. El script detecta los remotos `sehcontrol` u `origin` y verifica que
apunten a este repositorio. Las credenciales se administran mediante GitHub CLI
y nunca se guardan en el código.

## Compilación

### Interfaz Flutter

La interfaz principal se encuentra en `flutter/`.

```bash
cd flutter
flutter pub get
flutter run -d windows
```

Cambie `windows` por la plataforma de destino correspondiente. La compilación
completa también requiere que las bibliotecas nativas de Rust estén preparadas
para esa plataforma.

### Núcleo Rust

Con las dependencias nativas configuradas:

```bash
cargo build
```

Para generar una compilación optimizada:

```bash
cargo build --release
```

Consulte la
[documentación de compilación de RustDesk](https://rustdesk.com/docs/en/dev/build/)
para conocer los requisitos nativos y pasos específicos de cada sistema
operativo.

## Estructura del proyecto

- `flutter/`: interfaz Flutter para escritorio y dispositivos móviles.
- `src/`: núcleo Rust del cliente.
- `src/server/`: servicios de audio, vídeo, portapapeles, entrada y red.
- `src/platform/`: implementaciones específicas de cada sistema operativo.
- `libs/hbb_common/`: configuración, protocolo y utilidades compartidas.
- `libs/scrap/`: captura de pantalla.
- `libs/enigo/`: control de teclado y ratón.
- `libs/clipboard/`: integración del portapapeles.
- `res/`: iconos, logotipos y otros recursos de la aplicación.

## Seguridad

No publique credenciales privadas, tokens de acceso ni claves privadas en el
repositorio. Las claves públicas de infraestructura deben administrarse según
el mecanismo de configuración y rotación previsto por Sehcontrol.

Si encuentra una vulnerabilidad, comuníquela de forma privada al equipo
responsable de Sehcontrol antes de divulgarla públicamente.

## Créditos y licencia

Sehcontrol está basado en
[RustDesk](https://github.com/rustdesk/rustdesk), un proyecto de escritorio
remoto de código abierto escrito principalmente en Rust.

Agradecemos y reconocemos expresamente el trabajo de los autores, mantenedores
y colaboradores de RustDesk. Sehcontrol conserva referencias al proyecto
original y utiliza componentes de su arquitectura, protocolo e interfaz como
base para esta adaptación.

- Proyecto original: [rustdesk/rustdesk](https://github.com/rustdesk/rustdesk)
- Servidor original:
  [rustdesk/rustdesk-server](https://github.com/rustdesk/rustdesk-server)
- Documentación: [RustDesk Docs](https://rustdesk.com/docs/)

El uso, modificación y distribución de este código debe respetar las licencias
aplicables del proyecto RustDesk y de cada dependencia incluida. Los nombres,
marcas y recursos propios de Sehcontrol pertenecen a sus respectivos titulares.
