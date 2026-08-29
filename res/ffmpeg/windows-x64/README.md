# ffmpeg / ffprobe bundled tools (Windows x64)

Drop `ffmpeg.exe` and `ffprobe.exe` here to bundle them with the Windows build.
They power the **video preview** in the file-transfer window (still frame +
duration/resolution metadata).

- Binaries are **not committed** to keep the repository small.
- During `python build.py --flutter` (Windows), `prepare_ffmpeg_tools()` copies
  whatever is present here next to the packaged `sehcontrol.exe`.
- At runtime the app looks for `ffmpeg`/`ffprobe` **next to the executable**
  first, then falls back to the system `PATH`. If neither is found, the preview
  degrades gracefully to a plain video icon (no error).

## Where to get them

Use a static build for Windows x64, e.g. from:
- https://www.gyan.dev/ffmpeg/builds/ (release "essentials" static)
- https://github.com/BtbN/FFmpeg-Builds/releases (win64 gpl/lgpl static)

Copy only `bin\ffmpeg.exe` and `bin\ffprobe.exe` from the archive into this
folder. Prefer LGPL builds if licensing matters for redistribution.

## Other platforms

- Linux: place `ffmpeg`/`ffprobe` beside the bundled binary (or install them via
  the package manager); the runtime resolver checks the executable directory and
  then `PATH`.
- macOS: place them in `Contents/Resources/` of the app bundle, or rely on a
  system install on `PATH`.
