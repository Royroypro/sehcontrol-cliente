@echo off
setlocal EnableExtensions

rem =====================================================================
rem  SEHCONTROL ENTREGA 4 DEV CMD - Version 3.1
rem  Abre una consola preparada para compilar Sehcontrol en Windows.
rem =====================================================================

if /I "%~1"=="__CONFIGURAR__" goto CONFIGURAR
if /I "%~1"=="__AYUDA__" goto AYUDA
if /I "%~1"=="__MENU__" goto MENU
if /I "%~1"=="__ESTADO__" goto ESTADO
if /I "%~1"=="__VERSION__" goto VERSION
if /I "%~1"=="__APK_INSTALL__" goto APK_INSTALL

start "Sehcontrol Entrega 4 Dev CMD" cmd.exe /k ""%~f0" __CONFIGURAR__"
exit /b


:CONFIGURAR
endlocal
@echo off
title Sehcontrol Entrega 4 - Entorno de compilacion
color 0A

call :DEFINIR_ENTORNO

rem ---------------------------------------------------------------------
rem  Validaciones sin bloques entre parentesis.
rem  Esto evita el error provocado por "Program Files (x86)".
rem ---------------------------------------------------------------------

if exist "%SEHCONTROL_ROOT%\" goto CHECK_VS
set "SEH_ERROR=No existe el repositorio: %SEHCONTROL_ROOT%"
goto FIN_ERROR

:CHECK_VS
if exist "%VS_VCVARS%" goto CHECK_VCPKG
set "SEH_ERROR=No se encontro vcvars64.bat: %VS_VCVARS%"
goto FIN_ERROR

:CHECK_VCPKG
if exist "%VCPKG_ROOT%\vcpkg.exe" goto CHECK_CLANG
set "SEH_ERROR=No se encontro vcpkg.exe: %VCPKG_ROOT%\vcpkg.exe"
goto FIN_ERROR

:CHECK_CLANG
if exist "%LLVM_HOME%\bin\clang.exe" goto CHECK_LIBCLANG
set "SEH_ERROR=No se encontro clang.exe: %LLVM_HOME%\bin\clang.exe"
goto FIN_ERROR

:CHECK_LIBCLANG
if exist "%LIBCLANG_PATH%\libclang.dll" goto CHECK_FLUTTER
set "SEH_ERROR=No se encontro libclang.dll: %LIBCLANG_PATH%\libclang.dll"
goto FIN_ERROR

:CHECK_FLUTTER
if exist "%FLUTTER_ROOT%\bin\flutter.bat" goto LOAD_VS
set "SEH_ERROR=No se encontro Flutter: %FLUTTER_ROOT%\bin\flutter.bat"
goto FIN_ERROR


:LOAD_VS
call "%VS_VCVARS%"
if errorlevel 1 goto VS_ERROR

rem vcvars64.bat reemplaza VCPKG_ROOT; restaurarlo siempre despues.
call :DEFINIR_ENTORNO
set "PATH=%LLVM_HOME%\bin;%FLUTTER_ROOT%\bin;%ANDROID_SDK_ROOT%\platform-tools;%PATH%"

cd /d "%SEHCONTROL_ROOT%"
if errorlevel 1 goto CD_ERROR

rem Activar Rust 1.75.0 cuando ya esta instalado.
where rustup >nul 2>&1
if errorlevel 1 goto REGISTER_COMMANDS

rustup toolchain list | findstr /I /C:"1.75.0-x86_64-pc-windows-msvc" >nul
if errorlevel 1 goto RUST_WARNING

rustup override set 1.75.0-x86_64-pc-windows-msvc >nul
goto REGISTER_COMMANDS

:RUST_WARNING
echo.
echo [AVISO] Rust 1.75.0 MSVC no aparece instalado.
echo Instalar con:
echo rustup toolchain install 1.75.0-x86_64-pc-windows-msvc
echo.


:REGISTER_COMMANDS
rem ---------------------------------------------------------------------
rem  Deteccion de Android. A diferencia de las validaciones de arriba esta
rem  NO aborta: compilar solo para Windows es el caso normal, asi que aqui
rem  unicamente se marca si el toolchain esta completo para avisarlo en el
rem  panel y en "estado".
rem ---------------------------------------------------------------------
set "SEH_ANDROID=OK"
set "SEH_ANDROID_FALTA="
if exist "%ANDROID_SDK_ROOT%\platform-tools\adb.exe" goto AND_CHECK_NDK
set "SEH_ANDROID=FALTA"
set "SEH_ANDROID_FALTA=%SEH_ANDROID_FALTA% adb"

:AND_CHECK_NDK
if exist "%ANDROID_NDK_HOME%\toolchains\llvm\prebuilt\windows-x86_64\bin\llvm-ar.exe" goto AND_CHECK_MSYS
set "SEH_ANDROID=FALTA"
set "SEH_ANDROID_FALTA=%SEH_ANDROID_FALTA% NDK"

:AND_CHECK_MSYS
rem build_android.py necesita perl y make de MSYS2 para las dependencias C.
if exist "%MSYS_BIN%\perl.exe" goto AND_CHECK_TARGET
set "SEH_ANDROID=FALTA"
set "SEH_ANDROID_FALTA=%SEH_ANDROID_FALTA% msys2"

:AND_CHECK_TARGET
rem  El target de Rust se agrega con:
rem  rustup target add aarch64-linux-android
where rustup >nul 2>&1
if errorlevel 1 goto REGISTER_ALIAS
rustup target list --installed | findstr /I /C:"aarch64-linux-android" >nul
if not errorlevel 1 goto REGISTER_ALIAS
set "SEH_ANDROID=FALTA"
set "SEH_ANDROID_FALTA=%SEH_ANDROID_FALTA% target-rust"

:REGISTER_ALIAS
rem ---------------------------------------------------------------------
rem  Alias de compilacion
rem ---------------------------------------------------------------------
doskey build-dev=python .\build.py --portable --flutter --skip-portable-pack --hwcodec --vram
doskey builddev=python .\build.py --portable --flutter --skip-portable-pack --hwcodec --vram
doskey build-screen=python .\build.py --portable --flutter --skip-portable-pack --hwcodec --vram --screencam
doskey buildscreen=python .\build.py --portable --flutter --skip-portable-pack --hwcodec --vram --screencam
doskey build-installer=python .\build.py --portable --flutter --hwcodec --vram --screencam
doskey buildinstaller=python .\build.py --portable --flutter --hwcodec --vram --screencam
doskey build-basic=python .\build.py --flutter --skip-portable-pack
doskey buildbasic=python .\build.py --flutter --skip-portable-pack
doskey build-rust=cargo build --locked --features hwcodec,vram,flutter --lib --release
doskey buildrust=cargo build --locked --features hwcodec,vram,flutter --lib --release
rem  El DLL que se instala en produccion (C:\Program Files\Sehcontrol\
rem  sehcontrol.dll). Es build-rust MAS screencam: sin esa feature el binario
rem  compila igual pero sale sin ScreenCam, que es justo lo que se va a probar.
rem  Deja el resultado en target\release\sehcontrol.dll.
doskey build-dll=cargo build --locked --features screencam,vram,flutter --lib --release
doskey builddll=cargo build --locked --features screencam,vram,flutter --lib --release
doskey build-options=python .\build.py --help
doskey buildoptions=python .\build.py --help

rem ---------------------------------------------------------------------
rem  Alias de Android
rem  build_android.py hace las dos mitades: compila el nucleo Rust para
rem  aarch64-linux-android, lo copia a jniLibs\arm64-v8a y recien ahi
rem  ejecuta "flutter build apk". Compilar solo una mitad deja el APK con
rem  el .so viejo, por eso build-android-rust es solo para iterar rapido.
rem ---------------------------------------------------------------------
doskey build-android=python .\build_android.py
doskey buildandroid=python .\build_android.py
doskey build-android-rust=cargo build --locked --lib --target aarch64-linux-android --release --features flutter
doskey buildandroidrust=cargo build --locked --lib --target aarch64-linux-android --release --features flutter
doskey android-devices=adb devices -l
rem  El APK ya no se llama app-release.apk: build_android.py lo renombra a
rem  sehcontrol-<version>.apk. Se delega en un punto de entrada porque hay que
rem  buscar el mas reciente, y un doskey no puede llevar logica.
doskey android-install=call "%SEHCONTROL_LAUNCHER%" __APK_INSTALL__
doskey android-log=adb logcat -v time ^| findstr /I /C:"sehcontrol" /C:"flutter"
doskey android-log-limpiar=adb logcat -c
doskey android-desinstalar=adb uninstall com.carriez.sehcontrol
doskey abrir-apk=start "" "%SEHCONTROL_ROOT%\flutter\build\app\outputs\flutter-apk"

rem ---------------------------------------------------------------------
rem  Alias de ayuda, mantenimiento y diagnostico
rem ---------------------------------------------------------------------
doskey version=call "%SEHCONTROL_LAUNCHER%" __VERSION__
doskey cambiar-version=call "%SEHCONTROL_LAUNCHER%" __VERSION__
doskey ayuda=call "%SEHCONTROL_LAUNCHER%" __AYUDA__
doskey seh-ayuda=call "%SEHCONTROL_LAUNCHER%" __AYUDA__
doskey menu=call "%SEHCONTROL_LAUNCHER%" __MENU__
doskey estado=call "%SEHCONTROL_LAUNCHER%" __ESTADO__
doskey update-seh=git pull $T git submodule update --init --recursive $T cd /d "%SEHCONTROL_ROOT%\flutter" $T flutter pub get $T cd /d "%SEHCONTROL_ROOT%"
doskey submodulos=git submodule update --init --recursive
doskey flutter-get=cd /d "%SEHCONTROL_ROOT%\flutter" $T flutter pub get $T cd /d "%SEHCONTROL_ROOT%"
doskey flutter-clean=cd /d "%SEHCONTROL_ROOT%\flutter" $T flutter clean $T flutter pub get $T cd /d "%SEHCONTROL_ROOT%"
doskey clean-rust=cargo clean
doskey ffmpeg-list="%VCPKG_ROOT%\vcpkg.exe" list --x-install-root="%VCPKG_ROOT%\installed" ^| findstr /I ffmpeg
doskey git-estado=git status --short --branch
doskey abrir-build=start "" "%SEHCONTROL_ROOT%\flutter\build\windows\x64\runner\Release"
doskey ejecutar-seh=start "" "%SEHCONTROL_ROOT%\flutter\build\windows\x64\runner\Release\sehcontrol.exe"
doskey seh-comandos=doskey /macros

cls
call :PREGUNTAR_VERSION
call :PANEL_INICIAL
goto :eof


rem ---------------------------------------------------------------------
rem  Pregunta si se quiere subir la version antes de compilar.
rem
rem  Existe porque olvidarse es facil y el sintoma aparece tarde y confunde:
rem  si el binario publicado no alcanza la version declarada en el panel, los
rem  equipos ofrecen la actualizacion, la instalan, siguen viendo la misma
rem  version y la vuelven a ofrecer, en bucle.
rem
rem  El default es NO y hay timeout: cambiar la version solo hace falta cuando
rem  se va a publicar, no en cada compilacion de prueba.
rem ---------------------------------------------------------------------
:PREGUNTAR_VERSION
powershell -NoProfile -ExecutionPolicy Bypass -File "%SEHCONTROL_ROOT%\scripts\cambiar-version.ps1" -Mostrar
echo.
choice /C SN /N /T 15 /D N /M "Cambiar la version antes de compilar? [S/N] (N en 15s): "
if errorlevel 2 goto PREGUNTAR_VERSION_FIN
powershell -NoProfile -ExecutionPolicy Bypass -File "%SEHCONTROL_ROOT%\scripts\cambiar-version.ps1"
echo.
pause

:PREGUNTAR_VERSION_FIN
rem  choice deja en ERRORLEVEL el numero de la opcion elegida, y por defecto
rem  responde N, que es 2. Sin esto el arranque termina con codigo de salida 2
rem  aunque todo haya ido bien, y entonces "call ...bat && loquesea" no
rem  encadena nunca y cualquier automatizacion lo lee como fallo.
cmd /c exit 0
cls
goto :eof


:VERSION
powershell -NoProfile -ExecutionPolicy Bypass -File "%SEHCONTROL_ROOT%\scripts\cambiar-version.ps1"
exit /b


rem ---------------------------------------------------------------------
rem  Instala el APK mas reciente. Se busca en vez de fijar el nombre porque
rem  build_android.py lo renombra con la version (sehcontrol-1.4.14.apk), y
rem  un nombre fijo obligaria a editar esto en cada release.
rem
rem  Se acepta tambien app-release.apk: es como se llamaba antes, y un APK
rem  compilado con una version anterior del script sigue estando ahi.
rem ---------------------------------------------------------------------
:APK_INSTALL
endlocal
@echo off
if not defined SEHCONTROL_ROOT call :DEFINIR_ENTORNO
set "APK_DIR=%SEHCONTROL_ROOT%\flutter\build\app\outputs\flutter-apk"
set "APK_FILE="
for /f "delims=" %%A in ('dir /b /o-d "%APK_DIR%\sehcontrol-*.apk" 2^>nul') do if not defined APK_FILE set "APK_FILE=%%A"
if not defined APK_FILE for /f "delims=" %%A in ('dir /b /o-d "%APK_DIR%\app-release.apk" 2^>nul') do if not defined APK_FILE set "APK_FILE=%%A"
if not defined APK_FILE goto APK_NO_ENCONTRADO
rem  En la consola preparada adb esta en el PATH. Ejecutado en frio no, asi
rem  que se recurre a la ruta del SDK antes de rendirse.
set "ADB=adb"
where adb >nul 2>&1
if errorlevel 1 set "ADB=%ANDROID_SDK_ROOT%\platform-tools\adb.exe"
if not exist "%ADB%" if not "%ADB%"=="adb" goto APK_SIN_ADB
echo Instalando %APK_FILE% ...
"%ADB%" install -r "%APK_DIR%\%APK_FILE%"
exit /b

:APK_SIN_ADB
echo.
echo No se encontro adb. Instala Platform-Tools del SDK de Android o abre
echo el entorno preparado, que lo agrega al PATH.
echo.
exit /b

:APK_NO_ENCONTRADO
echo.
echo No se encontro ningun APK en:
echo   %APK_DIR%
echo Compilalo primero con: build-android
echo.
exit /b


rem ---------------------------------------------------------------------
rem  Definicion unica del entorno.
rem
rem  Se invoca dos veces en el arranque: antes de las validaciones y otra vez
rem  despues de vcvars64.bat, que pisa VCPKG_ROOT. Estaba duplicada en esos
rem  dos sitios y esa duplicacion ya costo una variable: se agrego en un
rem  bloque y no en el otro. Con una sola definicion eso no puede repetirse.
rem
rem  PATH deliberadamente NO se toca aqui: solo interesa despues de vcvars, y
rem  llamar dos veces lo antepondria dos veces.
rem ---------------------------------------------------------------------
:DEFINIR_ENTORNO
rem  El repositorio es la carpeta donde vive este .bat, no una ruta fija: asi
rem  el entorno sigue funcionando si el clon se mueve o se renombra.
rem  %~dp0 termina en "\", que hay que quitar para poder concatenar rutas.
set "SEHCONTROL_ROOT=%~dp0"
if "%SEHCONTROL_ROOT:~-1%"=="\" set "SEHCONTROL_ROOT=%SEHCONTROL_ROOT:~0,-1%"
set "SEHCONTROL_LAUNCHER=%~f0"
set "VS_VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set "VCPKG_ROOT=C:\vcpkg-sehcontrol"
set "LLVM_HOME=C:\LLVM15"
set "LIBCLANG_PATH=C:\LLVM15\bin"
set "FLUTTER_ROOT=C:\tools\flutter"
set "CARGO_BUILD_JOBS=4"

rem  bindgen (hwcodec, scrap, magnum-opus) agrega esto a los argumentos de
rem  clang. Si quedo definida a nivel de Usuario apuntando a otro vcpkg -el
rem  caso real que motivo esta linea era "-IE:\vcpkg\..."- clang recibe
rem  cabeceras de una instalacion que no es la nuestra, o de una unidad que
rem  ni existe. Vaciarla obliga a que las cabeceras salgan de VCPKG_ROOT y de
rem  lo que deja vcvars64, que es lo que este entorno controla.
set "BINDGEN_EXTRA_CLANG_ARGS="

rem  Android. No son obligatorias: si faltan, solo se desactivan los
rem  comandos android-* y build-android, el resto del entorno sigue igual.
rem  Las rutas coinciden con las que build_android.py busca por defecto.
set "ANDROID_SDK_ROOT=%LOCALAPPDATA%\Android\Sdk"
set "ANDROID_HOME=%LOCALAPPDATA%\Android\Sdk"
set "ANDROID_NDK_HOME=%LOCALAPPDATA%\Android\Sdk\ndk\25.2.9519653"
set "MSYS_BIN=C:\msys64\usr\bin"
goto :eof


:PANEL_INICIAL
echo ========================================================================
echo          SEHCONTROL ENTREGA 4 - ENTORNO DE COMPILACION LISTO
echo ========================================================================
echo.
echo  COMPILACION
echo  ----------------------------------------------------------------------
echo   build-dev        Flutter + hwcodec + VRAM, sin empaquetado final
echo   build-screen     Igual, agregando ScreenCam
echo   build-installer  Instalador/portable completo con ScreenCam
echo   build-basic      Flutter sin hwcodec ni VRAM
echo   build-rust       Solamente el nucleo Rust
echo   build-dll        Solo el DLL con ScreenCam (el que se instala)
echo   build-options    Todas las opciones originales de build.py
echo.
echo  ANDROID  [%SEH_ANDROID%]%SEH_ANDROID_FALTA%
echo  ----------------------------------------------------------------------
echo   build-android    APK release ARM64 (nucleo Rust + Flutter)
echo   android-devices  Listar telefonos conectados por adb
echo   android-install  Instalar el ultimo APK en el telefono
echo   android-log      Ver logcat filtrado por Sehcontrol
echo   abrir-apk        Abrir la carpeta del APK generado
echo.
echo  AYUDA Y UTILIDADES
echo  ----------------------------------------------------------------------
echo   version          Cambiar la version antes de publicar
echo   ayuda            Guia completa de variantes y comandos
echo   menu             Menu interactivo por numeros
echo   estado           Revisar herramientas, variables y FFmpeg
echo   update-seh       Actualizar Git, submodulos y Flutter
echo   git-estado       Mostrar cambios pendientes
echo   ejecutar-seh     Ejecutar el ultimo binario compilado
echo   abrir-build      Abrir la carpeta runner\Release
echo   seh-comandos     Mostrar todos los alias registrados
echo.
echo  COMANDO RECOMENDADO:
echo.
echo      build-dev
echo.
echo  Tambien funcionan las versiones sin guion: builddev, buildscreen, etc.
echo ========================================================================
echo.
goto :eof


:AYUDA
endlocal
@echo off
echo.
echo =========================================================================
echo              GUIA DE COMPILACION - SEHCONTROL ENTREGA 4
echo =========================================================================
echo.
echo  DESARROLLO NORMAL
echo  -----------------------------------------------------------------------
echo   build-dev
echo   python .\build.py --portable --flutter --skip-portable-pack --hwcodec --vram
echo.
echo  DESARROLLO CON SCREENCAM
echo  -----------------------------------------------------------------------
echo   build-screen
echo   python .\build.py --portable --flutter --skip-portable-pack --hwcodec --vram --screencam
echo.
echo  INSTALADOR O PORTABLE FINAL CON SCREENCAM
echo  -----------------------------------------------------------------------
echo   build-installer
echo   python .\build.py --portable --flutter --hwcodec --vram --screencam
echo.
echo  VERSION BASICA SIN HWCODEC
echo  -----------------------------------------------------------------------
echo   build-basic
echo   python .\build.py --flutter --skip-portable-pack
echo.
echo  SOLO NUCLEO RUST
echo  -----------------------------------------------------------------------
echo   build-rust
echo   cargo build --locked --features hwcodec,vram,flutter --lib --release
echo.
echo  SOLO EL DLL CON SCREENCAM (el que se instala en produccion)
echo  -----------------------------------------------------------------------
echo   build-dll
echo   cargo build --locked --features screencam,vram,flutter --lib --release
echo.
echo   Genera target\release\sehcontrol.dll, que es lo que reemplaza a
echo   C:\Program Files\Sehcontrol\sehcontrol.dll. Detener el servicio antes
echo   de copiarlo, y volver a arrancarlo despues.
echo.
echo  TODAS LAS OPCIONES DE BUILD.PY
echo  -----------------------------------------------------------------------
echo   build-options
echo   python .\build.py --help
echo.
echo  ANDROID (APK release ARM64)          Estado: %SEH_ANDROID% %SEH_ANDROID_FALTA%
echo  -----------------------------------------------------------------------
echo   build-android
echo   python .\build_android.py
echo.
echo   Hace las dos mitades en orden:
echo     1) cargo build --lib --target aarch64-linux-android --release
echo        --features flutter
echo     2) copia libsehcontrol.so a flutter\android\app\src\main\jniLibs\
echo        arm64-v8a
echo     3) flutter build apk --release --target-platform android-arm64
echo.
echo   build-android-rust  Solo el paso 1 y 2, para iterar sin rearmar el APK
echo.
echo   Requisitos: SDK con platform-tools, NDK 25.2.9519653, MSYS2 (perl y
echo   make) y el target de Rust:
echo     rustup target add aarch64-linux-android
echo.
echo   ANDROID_SDK_ROOT : %ANDROID_SDK_ROOT%
echo   ANDROID_NDK_HOME : %ANDROID_NDK_HOME%
echo.
echo   android-devices      Listar telefonos conectados
echo   android-install      Instalar el ultimo APK generado
echo   android-log          Logcat filtrado por Sehcontrol
echo   android-log-limpiar  Vaciar el buffer de logcat
echo   android-desinstalar  Quitar la app del telefono
echo   abrir-apk            Abrir la carpeta flutter-apk
echo.
echo  OPCIONES IMPORTANTES
echo  -----------------------------------------------------------------------
echo   --flutter              Construir interfaz Flutter
echo   --hwcodec              Activar codecs de video por hardware
echo   --vram                 Activar uso de VRAM en Windows
echo   --screencam            Activar ScreenCam; implica hwcodec
echo   --portable             Construir variante portable de Windows
echo   --skip-portable-pack   Omitir el empaquetado portable final
echo   --skip-cargo           Omitir Cargo cuando sea compatible
echo   --package NOMBRE       Construir un paquete concreto
echo   --feature ...          Integrar funciones adicionales
echo   --unix-file-copy-paste Activar copiar/pegar archivos en Unix
echo.
echo  MANTENIMIENTO
echo  -----------------------------------------------------------------------
echo   update-seh       Actualizar Git, submodulos y paquetes Flutter
echo   submodulos       Inicializar/actualizar submodulos
echo   flutter-get      Ejecutar flutter pub get
echo   flutter-clean    flutter clean y flutter pub get
echo   clean-rust       Ejecutar cargo clean
echo.
echo  DIAGNOSTICO
echo  -----------------------------------------------------------------------
echo   estado           Comprobar el entorno completo
echo   ffmpeg-list      Mostrar FFmpeg del vcpkg correcto
echo   git-estado       Ver rama y cambios pendientes
echo   seh-comandos     Mostrar todos los alias registrados
echo   abrir-build      Abrir runner\Release
echo   ejecutar-seh     Ejecutar el ultimo sehcontrol.exe
echo   menu             Abrir menu interactivo
echo   ayuda            Mostrar nuevamente esta guia
echo.
echo =========================================================================
echo.
exit /b


:ESTADO
endlocal
@echo off
rem ---------------------------------------------------------------------
rem  Llamado desde la consola preparada, el entorno ya existe. Ejecutado en
rem  frio (doble clic, otra cmd) no existia, y este informe imprimia rutas
rem  vacias mas el LIBCLANG_PATH heredado del sistema, que suele ser justo el
rem  valor roto que se esta intentando diagnosticar. Rellenarlo con la misma
rem  definicion que usa el arranque hace que las rutas mostradas sean siempre
rem  las que el entorno configura; el aviso de abajo explica por que las
rem  herramientas de MSVC apareceran como FALTA en ese caso.
rem ---------------------------------------------------------------------
set "SEH_ESTADO_FRIO="
if defined SEHCONTROL_ROOT goto ESTADO_CABECERA
set "SEH_ESTADO_FRIO=1"
call :DEFINIR_ENTORNO

:ESTADO_CABECERA
echo.
echo =========================================================================
echo                 ESTADO DEL ENTORNO - SEHCONTROL ENTREGA 4
echo =========================================================================
if not defined SEH_ESTADO_FRIO goto ESTADO_RUTAS
echo.
echo  [AVISO] Ejecutado fuera de la consola preparada.
echo  Las rutas de abajo son las que este entorno configura, pero vcvars64
echo  no se ha cargado en esta ventana: cl.exe y link.exe apareceran como
echo  FALTA aunque esten bien instalados. Para un informe real, abrir el
echo  entorno y escribir: estado

:ESTADO_RUTAS
echo.
echo  Repositorio   : %SEHCONTROL_ROOT%
echo  VCPKG_ROOT    : %VCPKG_ROOT%
echo  LLVM_HOME     : %LLVM_HOME%
echo  LIBCLANG_PATH : %LIBCLANG_PATH%
echo  Flutter       : %FLUTTER_ROOT%
echo  Cargo jobs    : %CARGO_BUILD_JOBS%
echo  Android SDK   : %ANDROID_SDK_ROOT%
echo  Android NDK   : %ANDROID_NDK_HOME%
echo.
where cl >nul 2>&1
if errorlevel 1 (echo  [FALTA] MSVC cl.exe) else echo  [OK] MSVC cl.exe
where link >nul 2>&1
if errorlevel 1 (echo  [FALTA] MSVC link.exe) else echo  [OK] MSVC link.exe
where clang >nul 2>&1
if errorlevel 1 (echo  [FALTA] LLVM clang.exe) else echo  [OK] LLVM clang.exe
where cargo >nul 2>&1
if errorlevel 1 (echo  [FALTA] Rust Cargo) else echo  [OK] Rust Cargo
where flutter >nul 2>&1
if errorlevel 1 (echo  [FALTA] Flutter) else echo  [OK] Flutter
where python >nul 2>&1
if errorlevel 1 (echo  [FALTA] Python) else echo  [OK] Python
if exist "%VCPKG_ROOT%\vcpkg.exe" (echo  [OK] vcpkg.exe) else echo  [FALTA] vcpkg.exe
echo.
if exist "%VCPKG_ROOT%\installed\x64-windows-static\lib\avcodec.lib" (echo  [OK] avcodec.lib) else echo  [FALTA] avcodec.lib
if exist "%VCPKG_ROOT%\installed\x64-windows-static\lib\avformat.lib" (echo  [OK] avformat.lib) else echo  [FALTA] avformat.lib
if exist "%VCPKG_ROOT%\installed\x64-windows-static\lib\avutil.lib" (echo  [OK] avutil.lib) else echo  [FALTA] avutil.lib
if exist "%VCPKG_ROOT%\installed\x64-windows-static\lib\swresample.lib" (echo  [OK] swresample.lib) else echo  [FALTA] swresample.lib
if exist "%VCPKG_ROOT%\installed\x64-windows-static\include\libavutil\attributes.h" (echo  [OK] attributes.h) else echo  [FALTA] attributes.h
echo.
echo  ANDROID
echo  -----------------------------------------------------------------------
if exist "%ANDROID_SDK_ROOT%\platform-tools\adb.exe" (echo  [OK] adb.exe) else echo  [FALTA] adb.exe - instalar Platform-Tools del SDK
if exist "%ANDROID_NDK_HOME%\toolchains\llvm\prebuilt\windows-x86_64\bin\llvm-ar.exe" (echo  [OK] NDK llvm-ar) else echo  [FALTA] NDK 25.2.9519653
if exist "%ANDROID_NDK_HOME%\toolchains\llvm\prebuilt\windows-x86_64\bin\myclang-arm64.cmd" (echo  [OK] myclang-arm64.cmd) else echo  [FALTA] myclang-arm64.cmd - lo requiere build_android.py
if exist "%MSYS_BIN%\perl.exe" (echo  [OK] MSYS2 perl.exe) else echo  [FALTA] MSYS2 perl.exe
if exist "%MSYS_BIN%\make.exe" (echo  [OK] MSYS2 make.exe) else echo  [FALTA] MSYS2 make.exe
rustup target list --installed 2>nul | findstr /I /C:"aarch64-linux-android" >nul
if errorlevel 1 (echo  [FALTA] target Rust - rustup target add aarch64-linux-android) else echo  [OK] target aarch64-linux-android
where adb >nul 2>&1
if errorlevel 1 goto ESTADO_VERSIONES
echo.
echo  Dispositivos conectados:
adb devices -l 2>nul

:ESTADO_VERSIONES
echo.
rustc --version 2>nul
cargo --version 2>nul
clang --version 2>nul | findstr /I /C:"clang version"
flutter --version 2>nul | findstr /I /C:"Flutter " /C:"Dart "
echo.
exit /b


:MENU
endlocal
@echo off
:MENU_LOOP
echo.
echo =========================================================================
echo                MENU DE TRABAJO - SEHCONTROL ENTREGA 4
echo =========================================================================
echo.
echo   WINDOWS
echo   [1] Compilar desarrollo recomendado
echo   [2] Compilar desarrollo con ScreenCam
echo   [3] Compilar instalador/portable final con ScreenCam
echo   [4] Compilar version basica sin hwcodec
echo   [5] Compilar solamente el nucleo Rust
echo   [E] Compilar solo el DLL con ScreenCam (el que se instala)
echo.
echo   ANDROID  [%SEH_ANDROID%]%SEH_ANDROID_FALTA%
echo   [6] Compilar APK release ARM64 (nucleo Rust + Flutter)
echo   [7] Compilar solo el nucleo Rust de Android
echo   [8] Instalar el ultimo APK en el telefono
echo   [9] Ver logcat filtrado por Sehcontrol
echo.
echo   OTROS
echo   [A] Mostrar opciones originales de build.py
echo   [B] Actualizar repositorio y dependencias
echo   [C] Revisar estado del entorno
echo   [D] Mostrar guia completa
echo   [0] Cerrar este menu
echo.
rem  Las comprobaciones de errorlevel van de mayor a menor y su numero es la
rem  POSICION dentro de /C, no el caracter. Agregar una opcion corre todas las
rem  posteriores: "E" es la 14 y "0" pasa a ser la 15.
choice /C 123456789ABCDE0 /N /M "Seleccione una opcion: "

if errorlevel 15 goto MENU_FIN
if errorlevel 14 goto MENU_DLL
if errorlevel 13 goto MENU_HELP
if errorlevel 12 goto MENU_STATUS
if errorlevel 11 goto MENU_UPDATE
if errorlevel 10 goto MENU_OPTIONS
if errorlevel 9 goto MENU_ANDROID_LOG
if errorlevel 8 goto MENU_ANDROID_INSTALL
if errorlevel 7 goto MENU_ANDROID_RUST
if errorlevel 6 goto MENU_ANDROID
if errorlevel 5 goto MENU_RUST
if errorlevel 4 goto MENU_BASIC
if errorlevel 3 goto MENU_INSTALLER
if errorlevel 2 goto MENU_SCREEN
if errorlevel 1 goto MENU_DEV
goto MENU_LOOP

:MENU_HELP
call "%SEHCONTROL_LAUNCHER%" __AYUDA__
goto MENU_LOOP

:MENU_STATUS
call "%SEHCONTROL_LAUNCHER%" __ESTADO__
goto MENU_LOOP

:MENU_UPDATE
git pull
if errorlevel 1 goto MENU_PAUSA
git submodule update --init --recursive
if errorlevel 1 goto MENU_PAUSA
cd /d "%SEHCONTROL_ROOT%\flutter"
flutter pub get
cd /d "%SEHCONTROL_ROOT%"
goto MENU_PAUSA

:MENU_OPTIONS
python .\build.py --help
goto MENU_PAUSA

:MENU_RUST
cargo build --locked --features hwcodec,vram,flutter --lib --release
goto MENU_PAUSA

:MENU_DLL
cargo build --locked --features screencam,vram,flutter --lib --release
goto MENU_PAUSA

:MENU_BASIC
python .\build.py --flutter --skip-portable-pack
goto MENU_PAUSA

:MENU_INSTALLER
python .\build.py --portable --flutter --hwcodec --vram --screencam
goto MENU_PAUSA

:MENU_SCREEN
python .\build.py --portable --flutter --skip-portable-pack --hwcodec --vram --screencam
goto MENU_PAUSA

:MENU_DEV
python .\build.py --portable --flutter --skip-portable-pack --hwcodec --vram
goto MENU_PAUSA

:MENU_ANDROID
python .\build_android.py
goto MENU_PAUSA

:MENU_ANDROID_RUST
cargo build --locked --lib --target aarch64-linux-android --release --features flutter
goto MENU_PAUSA

:MENU_ANDROID_INSTALL
call "%SEHCONTROL_LAUNCHER%" __APK_INSTALL__
goto MENU_PAUSA

:MENU_ANDROID_LOG
echo.
echo Mostrando logcat. Pulse Ctrl+C para volver al menu.
echo.
adb logcat -v time | findstr /I /C:"sehcontrol" /C:"flutter"
goto MENU_PAUSA

:MENU_PAUSA
echo.
pause
goto MENU_LOOP

:MENU_FIN
echo.
echo Menu cerrado. La consola principal continua disponible.
echo.
exit /b


:VS_ERROR
set "SEH_ERROR=Visual Studio no pudo inicializar el entorno x64."
goto FIN_ERROR

:CD_ERROR
set "SEH_ERROR=No se pudo entrar al repositorio: %SEHCONTROL_ROOT%"
goto FIN_ERROR

:FIN_ERROR
echo.
echo ========================================================================
echo [ERROR] %SEH_ERROR%
echo ========================================================================
echo.
echo La consola permanecera abierta para revisar el problema.
echo.
exit /b
