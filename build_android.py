#!/usr/bin/env python3
"""Compile the SehControl Android release APK from Windows."""

from __future__ import annotations

import shutil
import subprocess
import sys
import os
from pathlib import Path


def build_native_android(project_dir: Path) -> int:
    """Build and stage the ARM64 Rust library used by the Android APK."""
    target = "aarch64-linux-android"
    env = os.environ.copy()

    if sys.platform == "win32":
        ndk = Path(
            env.get("ANDROID_NDK_HOME")
            or env.get("ANDROID_NDK_ROOT")
            or Path.home()
            / "AppData"
            / "Local"
            / "Android"
            / "Sdk"
            / "ndk"
            / "25.2.9519653"
        )
        toolchain = ndk / "toolchains" / "llvm" / "prebuilt" / "windows-x86_64"
        bin_dir = toolchain / "bin"
        msys_bin = Path(r"C:\msys64\usr\bin")
        required = (
            bin_dir / "myclang-arm64.cmd",
            bin_dir / "myclangxx-arm64.cmd",
            bin_dir / "llvm-ar.exe",
            msys_bin / "perl.exe",
            msys_bin / "make.exe",
        )
        missing = [str(path) for path in required if not path.is_file()]
        if missing:
            print(
                "Error: faltan herramientas para compilar el núcleo Android:\n"
                + "\n".join(missing),
                file=sys.stderr,
            )
            return 1

        posix_bin = bin_dir.as_posix()
        sysroot = (toolchain / "sysroot").as_posix()
        env["PATH"] = os.pathsep.join(
            (str(bin_dir), env.get("PATH", ""), str(msys_bin))
        )
        env["CC_aarch64_linux_android"] = f"{posix_bin}/myclang-arm64.cmd"
        env["CXX_aarch64_linux_android"] = f"{posix_bin}/myclangxx-arm64.cmd"
        env["AR_aarch64_linux_android"] = f"{posix_bin}/llvm-ar.exe"
        env["RANLIB_aarch64_linux_android"] = f"{posix_bin}/llvm-ranlib.exe"
        clang_args = f"--target=aarch64-linux-android21 --sysroot={sysroot}"
        env["BINDGEN_EXTRA_CLANG_ARGS"] = clang_args
        env["BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android"] = clang_args
        env["BINDGEN_EXTRA_CLANG_ARGS_aarch64-linux-android"] = clang_args
        env.pop("PERL", None)
        env.pop("MAKE", None)

        command = [
            "cargo",
            "build",
            "--locked",
            "--lib",
            "--target",
            target,
            "--release",
            "--features",
            "flutter",
        ]
    else:
        command = [
            "cargo",
            "ndk",
            "--platform",
            "21",
            "--target",
            target,
            "build",
            "--locked",
            "--lib",
            "--release",
            "--features",
            "flutter,hwcodec",
        ]

    print("Compilando núcleo nativo Android ARM64...", flush=True)
    result = subprocess.run(command, cwd=project_dir, env=env, check=False)
    if result.returncode != 0:
        print(
            f"La compilación del núcleo Android terminó con error "
            f"({result.returncode}).",
            file=sys.stderr,
        )
        return result.returncode

    source = project_dir / "target" / target / "release" / "libsehcontrol.so"
    destination = (
        project_dir
        / "flutter"
        / "android"
        / "app"
        / "src"
        / "main"
        / "jniLibs"
        / "arm64-v8a"
    )
    if not source.is_file():
        print(f"Error: no se generó {source}", file=sys.stderr)
        return 1

    destination.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination / "libsehcontrol.so")
    duplicate = destination / "sehcontrol.so"
    if duplicate.exists():
        duplicate.unlink()
    print(f"Núcleo Android actualizado: {source}", flush=True)

    # libsehcontrol.so enlaza contra la runtime de C++ del NDK, y Android NO la
    # trae: de las seis dependencias que declara, cinco las aporta el sistema
    # (liblog, libdl, libm, libc, libOpenSLES) y libc++_shared.so la tiene que
    # empaquetar la propia app.
    #
    # Sin ella el APK instala sin quejarse y la app muere al abrirse, en
    # MainApplication.onCreate, antes de pintar nada:
    #
    #   java.lang.UnsatisfiedLinkError: dlopen failed:
    #   library "libc++_shared.so" not found: needed by .../libsehcontrol.so
    #
    # Se copia del mismo NDK con el que se acaba de compilar, no de una ruta
    # fija: usar otra version que la del toolchain es justo como se llega a
    # incompatibilidades de ABI dificiles de rastrear.
    if sys.platform == "win32":
        cxx_shared = (
            toolchain
            / "sysroot"
            / "usr"
            / "lib"
            / "aarch64-linux-android"
            / "libc++_shared.so"
        )
        if not cxx_shared.is_file():
            print(
                f"Error: no se encontró {cxx_shared}. El APK se instalaría pero "
                "la app no abriría.",
                file=sys.stderr,
            )
            return 1
        shutil.copy2(cxx_shared, destination / "libc++_shared.so")
        print(f"Runtime de C++ empaquetada: {cxx_shared}", flush=True)

    return 0


def main() -> int:
    project_dir = Path(__file__).resolve().parent
    flutter_dir = project_dir / "flutter"
    # On Windows the Flutter SDK also contains an extensionless Unix launcher.
    # Selecting flutter.bat explicitly prevents subprocess from opening the
    # wrong launcher and becoming stuck.
    flutter = shutil.which("flutter.bat") if sys.platform == "win32" else None
    flutter = flutter or shutil.which("flutter")

    if flutter is None:
        print(
            "Error: no se encontró 'flutter' en PATH. "
            "Abre una terminal configurada para Flutter e inténtalo nuevamente.",
            file=sys.stderr,
        )
        return 1

    if not (flutter_dir / "pubspec.yaml").is_file():
        print(f"Error: no se encontró el proyecto Flutter en {flutter_dir}", file=sys.stderr)
        return 1

    native_result = build_native_android(project_dir)
    if native_result != 0:
        return native_result

    flutter_command = [flutter]
    build_env = os.environ.copy()
    if sys.platform == "win32":
        flutter_root = Path(flutter).resolve().parent.parent
        dart = flutter_root / "bin" / "cache" / "dart-sdk" / "bin" / "dart.exe"
        snapshot = flutter_root / "bin" / "cache" / "flutter_tools.snapshot"
        packages = (
            flutter_root
            / "packages"
            / "flutter_tools"
            / ".dart_tool"
            / "package_config.json"
        )
        if all(path.is_file() for path in (dart, snapshot, packages)):
            # Some Windows environments can make flutter.bat loop before Dart
            # starts. Calling the same Flutter snapshot directly is equivalent
            # to the final command executed by the official launcher.
            flutter_command = [
                str(dart),
                f"--packages={packages}",
                str(snapshot),
            ]
            build_env["FLUTTER_ROOT"] = str(flutter_root)

    command = flutter_command + [
        "build",
        "apk",
        "--release",
        "--target-platform",
        "android-arm64",
        "--obfuscate",
        "--split-debug-info",
        str(flutter_dir / "split-debug-info"),
    ]

    print(f"Proyecto: {project_dir}", flush=True)
    print("Compilando APK Android release...", flush=True)
    result = subprocess.run(command, cwd=flutter_dir, env=build_env, check=False)
    if result.returncode != 0:
        print(f"La compilación terminó con error ({result.returncode}).", file=sys.stderr)
        return result.returncode

    apk = flutter_dir / "build" / "app" / "outputs" / "flutter-apk" / "app-release.apk"
    print(f"APK generado: {apk}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
