#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
sehcontrol_rebrand_safe.py
Rebranding seguro de Sehcontrol -> Sehcontrol (Windows / PowerShell friendly)

USO (PowerShell):
  # 1) Simular (no escribe)
  python .\sehcontrol_rebrand_safe.py "C:\sehcontrol" --dry-run --old sehcontrol --new sehcontrol --new-camel Sehcontrol

  # 2) Aplicar (con backups .bak)
  python .\sehcontrol_rebrand_safe.py "C:\sehcontrol" --apply --backup --old sehcontrol --new sehcontrol --new-camel Sehcontrol

  # 3) Revertir (restaura desde .bak)
  python .\sehcontrol_rebrand_safe.py "C:\sehcontrol" --revert

IMPORTANTE:
- Protege ramas git tipo branch="rustdesk/..." y URLs rustdesk-org para NO romper dependencias.
- Protege URLs (http/https/www) para no modificar links.
"""

import os
import re
import stat
import argparse
import shutil
from pathlib import Path
from typing import Tuple, List

# -----------------------------
# CONFIG
# -----------------------------
BINARY_EXTS = {
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".icns",
    ".so", ".dll", ".exe", ".bin", ".a", ".lib",
    ".zip", ".7z", ".rar", ".pdf",
    ".ttf", ".woff", ".woff2",
    ".obj", ".pdb", ".exp", ".ilk", ".node",
    ".res", ".pak"
}

DEFAULT_EXCLUDE_DIRS = {
    ".git", "target", "node_modules", "build", "dist", "__pycache__",
    ".dart_tool", ".idea", ".vscode", ".gradle", ".android", ".ios",
    ".vs"
}

TEXT_EXTS = {
    ".rs", ".toml", ".lock", ".md", ".txt",
    ".yaml", ".yml", ".json", ".xml", ".rc", ".manifest",
    ".c", ".cc", ".cpp", ".h", ".hpp", ".m", ".mm",
    ".dart", ".kt", ".java", ".swift",
    ".ps1", ".bat", ".cmd", ".sh",
    ".iss", ".wxs", ".csproj", ".props", ".targets"
}

# URLs: no se tocan
URL_PATTERN = re.compile(r'(?:https?://|www\.)[^\s<>"\')\]]+', re.IGNORECASE)

# Líneas a proteger para NO romper dependencias externas (Cargo.toml/Cargo.lock principalmente)
PROTECT_LINE_REGEXES = [
    # dependencias git en toml
    re.compile(r'^\s*(branch|tag|rev)\s*=\s*".*"$', re.IGNORECASE),
    re.compile(r'^\s*git\s*=\s*".*"$', re.IGNORECASE),
    # Cargo.lock sources
    re.compile(r'^\s*source\s*=\s*".*"$', re.IGNORECASE),
    # organizaciones / URLs de upstream (no rebrandear)
    re.compile(r'rustdesk-org', re.IGNORECASE),
    re.compile(r'github\.com/sehcontrol', re.IGNORECASE),
    # ramas estilo sehcontrol/xxx dentro de comillas
    re.compile(r'"rustdesk/[^"]+"', re.IGNORECASE),
    # ssh urls git@github.com:rustdesk...
    re.compile(r'git@github\.com:sehcontrol', re.IGNORECASE),
]

# -----------------------------
# HELPERS
# -----------------------------
def make_writable(path: Path):
    if path.exists():
        mode = os.stat(path).st_mode
        os.chmod(path, mode | stat.S_IWRITE)

def is_binary_file(path: Path) -> bool:
    if path.suffix.lower() in BINARY_EXTS:
        return True
    # Si no es extensión de texto típica, intenta detectar nulos
    if path.suffix.lower() not in TEXT_EXTS:
        try:
            with open(path, "rb") as f:
                return b"\x00" in f.read(2048)
        except Exception:
            return True
    try:
        with open(path, "rb") as f:
            return b"\x00" in f.read(2048)
    except Exception:
        return True

def read_file_safe(path: Path):
    for enc in ("utf-8", "utf-8-sig", "latin-1", "cp1252"):
        try:
            return path.read_text(encoding=enc), enc
        except Exception:
            continue
    raise UnicodeError(f"No se pudo leer: {path}")

def backup_file(path: Path):
    bak = Path(str(path) + ".bak")
    if not bak.exists():
        shutil.copy2(path, bak)

def should_protect_line(line: str) -> bool:
    return any(rx.search(line) for rx in PROTECT_LINE_REGEXES)

def split_keep_urls(text: str):
    parts = URL_PATTERN.split(text)
    urls = URL_PATTERN.findall(text)
    return parts, urls

def join_keep_urls(parts, urls):
    out = []
    for i, p in enumerate(parts):
        out.append(p)
        if i < len(urls):
            out.append(urls[i])
    return "".join(out)

def safe_rename(old_path: Path, new_path: Path):
    """
    Renombre robusto en Windows (incluye caso "solo cambia mayúsculas").
    """
    if old_path == new_path:
        return
    if not old_path.exists():
        return

    # Asegurar que el destino no existe
    if new_path.exists():
        # si ya existe, evita colisión
        raise FileExistsError(f"Destino ya existe: {new_path}")

    try:
        old_path.rename(new_path)
    except OSError:
        # rename 2 pasos (Windows case-insensitive)
        tmp = new_path.with_name(new_path.name + ".__tmp__")
        if tmp.exists():
            if tmp.is_file():
                tmp.unlink()
            else:
                shutil.rmtree(tmp)
        old_path.rename(tmp)
        tmp.rename(new_path)

# -----------------------------
# REPLACEMENT LOGIC
# -----------------------------
def build_replacements(old: str, new: str, new_camel: str):
    """
    Reglas específicas (prioridad alta) + reglas generales (Sehcontrol/sehcontrol).
    """
    old_lower = old.lower()
    old_title = old_lower.capitalize()          # sehcontrol -> Sehcontrol
    old_brand = "Sehcontrol"                      # marca típica
    old_impl = f"{old_title}Impl"               # SehcontrolImpl

    new_lower = new.lower()
    new_title = new_lower.capitalize()
    new_brand = new_camel                       # Sehcontrol
    new_impl = f"{new_camel}Impl"

    # (pattern, replacement) en orden. Usamos regex con sensibilidad exacta según patrón.
    rules = [
        # específicos visibles
        (re.compile(rf"\b{re.escape(old_impl)}\b"), new_impl),
        (re.compile(rf"\b{re.escape(old_brand)}\b"), new_brand),
        (re.compile(rf"\b{re.escape(old_title)}\b"), new_title),   # por si aparece Sehcontrol
        (re.compile(rf"\b{re.escape(old_lower)}\b"), new_lower),

        # exe/dll nombres
        (re.compile(rf"\b{re.escape(old_lower)}\.exe\b", re.IGNORECASE), f"{new_lower}.exe"),
        (re.compile(rf"\b{re.escape(old_lower)}\.msi\b", re.IGNORECASE), f"{new_lower}.msi"),
    ]
    return rules

def apply_rules_to_line(line: str, rules) -> Tuple[str, int]:
    """
    Aplica reglas en una línea, protegiendo URLs y líneas de dependencias.
    """
    if should_protect_line(line):
        return line, 0

    parts, urls = split_keep_urls(line)
    count = 0

    new_parts: List[str] = []
    for p in parts:
        before = p
        for rx, rep in rules:
            p2, n = rx.subn(rep, p)
            if n:
                count += n
            p = p2
        new_parts.append(p)

    return join_keep_urls(new_parts, urls), count

def apply_rules_to_text(text: str, rules) -> Tuple[str, int]:
    total = 0
    out_lines: List[str] = []
    for line in text.splitlines(keepends=True):
        new_line, n = apply_rules_to_line(line, rules)
        total += n
        out_lines.append(new_line)
    return "".join(out_lines), total

def should_skip_path(path: Path) -> bool:
    # no tocar cosas dentro de excluded dirs
    parts = set(path.parts)
    return any(p in DEFAULT_EXCLUDE_DIRS for p in parts)

# -----------------------------
# CORE ACTIONS
# -----------------------------
def run_revert(root: Path):
    print(f">>> Revirtiendo desde .bak en: {root}")
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in DEFAULT_EXCLUDE_DIRS]
        for name in filenames:
            if not name.endswith(".bak"):
                continue
            bak_path = Path(dirpath) / name
            orig_path = Path(dirpath) / name[:-4]
            rel = orig_path.relative_to(root)
            print(f"[RESTORE] {rel}")
            make_writable(bak_path)
            if orig_path.exists():
                make_writable(orig_path)
                orig_path.unlink()
            bak_path.rename(orig_path)

def run_rebrand(root: Path, rules, dry_run: bool, backup: bool):
    # 1) contenido
    print(">>> Paso 1: Reemplazo de contenido (modo seguro)...")
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in DEFAULT_EXCLUDE_DIRS]
        for fname in filenames:
            path = Path(dirpath) / fname
            if should_skip_path(path):
                continue
            if is_binary_file(path):
                continue

            try:
                content, enc = read_file_safe(path)
                new_content, n = apply_rules_to_text(content, rules)
                if n > 0:
                    rel = path.relative_to(root)
                    tag = "DRY" if dry_run else "APPLY"
                    print(f"[{tag} CONTENIDO] {rel} ({n} cambios)")
                    if not dry_run:
                        if backup:
                            backup_file(path)
                        make_writable(path)
                        path.write_text(new_content, encoding=enc)
            except Exception as e:
                print(f"[ERROR] {path.relative_to(root)}: {e}")

    # 2) renombrado archivos/carpetas (bottom-up)
    print("\n>>> Paso 2: Renombrando archivos y carpetas...")
    # Para renombrar nombres, aplicamos solo reglas simples sobre el nombre
    # (sin tocar URLs, y evitando excluded dirs/binarios).
    # Hacemos un reemplazo directo de tokens típicos.
    name_rules = [
        # prioriza Sehcontrol -> Sehcontrol
        ("Sehcontrol", rules[1][1]),
        # luego sehcontrol -> sehcontrol
        ("sehcontrol", rules[3][1]),
        # y Sehcontrol -> Sehcontrol/Title
        ("Sehcontrol", rules[2][1]),
    ]

    for dirpath, dirnames, filenames in os.walk(root, topdown=False):
        # renombra primero archivos y luego dirs
        for name in filenames + dirnames:
            old_path = Path(dirpath) / name
            if should_skip_path(old_path):
                continue
            if old_path.is_file() and is_binary_file(old_path):
                continue

            new_name = name
            for a, b in name_rules:
                new_name = new_name.replace(a, b)

            if new_name == name:
                continue

            new_path = Path(dirpath) / new_name
            rel_old = old_path.relative_to(root)
            rel_new = new_path.relative_to(root)
            tag = "DRY" if dry_run else "APPLY"
            print(f"[{tag} RENOMBRAR] {rel_old} -> {rel_new}")

            if not dry_run:
                make_writable(old_path)
                safe_rename(old_path, new_path)

# -----------------------------
# MAIN
# -----------------------------
def main():
    p = argparse.ArgumentParser(description="Rebrand seguro Sehcontrol -> Sehcontrol (no rompe deps).")
    p.add_argument("directory", help="Carpeta raíz del repo (ej: C:\\sehcontrol)")
    p.add_argument("--old", default="sehcontrol", help="Nombre original base (default: sehcontrol)")
    p.add_argument("--new", default="sehcontrol", help="Nombre nuevo base (default: sehcontrol)")
    p.add_argument("--new-camel", default="Sehcontrol", help='Nombre nuevo visible (default: "Sehcontrol")')

    mode = p.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true", help="Simula cambios (no escribe)")
    mode.add_argument("--apply", action="store_true", help="Aplica cambios")
    mode.add_argument("--revert", action="store_true", help="Restaura desde .bak")

    p.add_argument("--backup", action="store_true", help="Crea .bak antes de modificar (recomendado)")

    args = p.parse_args()
    root = Path(args.directory).resolve()

    if not root.exists():
        raise SystemExit(f"No existe la ruta: {root}")

    if args.revert:
        run_revert(root)
        return

    rules = build_replacements(args.old, args.new, args.new_camel)
    run_rebrand(root, rules, dry_run=args.dry_run, backup=args.backup)

    if args.dry_run:
        print("\n>>> DRY-RUN terminado: no se escribieron cambios.")
    else:
        print("\n>>> APPLY terminado.")
        if args.backup:
            print(">>> Se crearon .bak (puedes revertir con --revert).")

if __name__ == "__main__":
    main()
