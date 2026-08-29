import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/models/file_model.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:get/get.dart';
import 'package:path/path.dart' as p;
import 'package:path_provider/path_provider.dart';
import 'package:url_launcher/url_launcher.dart';

/// Broad classification of a file used to decide how (or whether) it can be
/// previewed inside the app.
enum PreviewKind {
  image,
  video,
  audio,
  pdf,
  text,
  code,
  archive,
  document,
  other,
}

class FileTypeInfo {
  final PreviewKind kind;

  /// Short human label, e.g. "Image (PNG)".
  final String label;
  final IconData icon;
  final Color color;

  const FileTypeInfo(this.kind, this.label, this.icon, this.color);

  bool get isMedia => kind == PreviewKind.video || kind == PreviewKind.audio;
  bool get canRenderInApp =>
      kind == PreviewKind.image ||
      kind == PreviewKind.text ||
      kind == PreviewKind.code;
}

const _imageExts = {
  'png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'ico', 'heic', 'heif' //
};
const _videoExts = {
  'mp4', 'mkv', 'mov', 'avi', 'webm', 'flv', 'wmv', 'm4v', '3gp', 'mpeg', 'mpg'
};
const _audioExts = {
  'mp3', 'wav', 'flac', 'aac', 'ogg', 'm4a', 'wma', 'opus', 'aiff'
};
const _textExts = {
  'txt', 'md', 'log', 'csv', 'ini', 'cfg', 'conf', 'rtf'
};
const _codeExts = {
  'dart', 'rs', 'py', 'js', 'ts', 'tsx', 'jsx', 'java', 'kt', 'c', 'cpp', 'h',
  'hpp', 'cs', 'go', 'rb', 'php', 'swift', 'sh', 'ps1', 'bat', 'html', 'css',
  'scss', 'json', 'yaml', 'yml', 'xml', 'toml', 'sql', 'gradle', 'lua', 'vue'
};
const _archiveExts = {
  'zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'iso', 'apk', 'dmg'
};
const _docExts = {
  'pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'odt', 'ods', 'odp'
};

/// Extension without the leading dot, lowercased.
String fileExtOf(String name) {
  final dot = name.lastIndexOf('.');
  if (dot < 0 || dot == name.length - 1) return '';
  return name.substring(dot + 1).toLowerCase();
}

FileTypeInfo fileTypeInfoOf(String name) {
  final ext = fileExtOf(name);
  final extLabel = ext.isEmpty ? '' : ' (${ext.toUpperCase()})';
  if (_imageExts.contains(ext)) {
    return FileTypeInfo(PreviewKind.image,
        '${translate('Image')}$extLabel', Icons.image_outlined, Colors.teal);
  }
  if (_videoExts.contains(ext)) {
    return FileTypeInfo(PreviewKind.video,
        '${translate('Video')}$extLabel', Icons.movie_outlined, Colors.indigo);
  }
  if (_audioExts.contains(ext)) {
    return FileTypeInfo(PreviewKind.audio, '${translate('Audio')}$extLabel',
        Icons.audiotrack_outlined, Colors.purple);
  }
  if (ext == 'pdf') {
    return FileTypeInfo(PreviewKind.pdf, 'PDF',
        Icons.picture_as_pdf_outlined, Colors.red);
  }
  if (_docExts.contains(ext)) {
    return FileTypeInfo(PreviewKind.document,
        '${translate('Document')}$extLabel',
        Icons.description_outlined, Colors.blue);
  }
  if (_codeExts.contains(ext)) {
    return FileTypeInfo(PreviewKind.code, '${translate('Code')}$extLabel',
        Icons.code, Colors.green);
  }
  if (_textExts.contains(ext)) {
    return FileTypeInfo(PreviewKind.text, '${translate('Text')}$extLabel',
        Icons.text_snippet_outlined, Colors.blueGrey);
  }
  if (_archiveExts.contains(ext)) {
    return FileTypeInfo(PreviewKind.archive,
        '${translate('Archive')}$extLabel',
        Icons.folder_zip_outlined, Colors.orange);
  }
  return FileTypeInfo(PreviewKind.other,
      ext.isEmpty ? translate('File') : ext.toUpperCase(),
      Icons.insert_drive_file_outlined, Colors.grey);
}

/// Type filter applied to a file listing.
enum FileFilter {
  all,
  folders,
  image,
  video,
  audio,
  document,
  code,
  archive,
  other,
}

String fileFilterLabel(FileFilter f) {
  switch (f) {
    case FileFilter.all:
      return translate('All');
    case FileFilter.folders:
      return translate('Folders');
    case FileFilter.image:
      return translate('Images');
    case FileFilter.video:
      return translate('Videos');
    case FileFilter.audio:
      return translate('Audio');
    case FileFilter.document:
      return translate('Documents');
    case FileFilter.code:
      return translate('Code');
    case FileFilter.archive:
      return translate('Archives');
    case FileFilter.other:
      return translate('Other');
  }
}

IconData fileFilterIcon(FileFilter f) {
  switch (f) {
    case FileFilter.all:
      return Icons.apps_outlined;
    case FileFilter.folders:
      return Icons.folder_outlined;
    case FileFilter.image:
      return Icons.image_outlined;
    case FileFilter.video:
      return Icons.movie_outlined;
    case FileFilter.audio:
      return Icons.audiotrack_outlined;
    case FileFilter.document:
      return Icons.description_outlined;
    case FileFilter.code:
      return Icons.code;
    case FileFilter.archive:
      return Icons.folder_zip_outlined;
    case FileFilter.other:
      return Icons.insert_drive_file_outlined;
  }
}

bool entryMatchesFilter(Entry entry, FileFilter f) {
  if (f == FileFilter.all) return true;
  if (f == FileFilter.folders) return entry.isDirectory || entry.isDrive;
  if (!entry.isFile) return false;
  final kind = fileTypeInfoOf(entry.name).kind;
  switch (f) {
    case FileFilter.image:
      return kind == PreviewKind.image;
    case FileFilter.video:
      return kind == PreviewKind.video;
    case FileFilter.audio:
      return kind == PreviewKind.audio;
    case FileFilter.document:
      return kind == PreviewKind.pdf ||
          kind == PreviewKind.document ||
          kind == PreviewKind.text;
    case FileFilter.code:
      return kind == PreviewKind.code;
    case FileFilter.archive:
      return kind == PreviewKind.archive;
    case FileFilter.other:
      return kind == PreviewKind.other;
    default:
      return true;
  }
}

/// Downloads remote files to a temporary cache so they can be previewed
/// without committing them to the user's chosen destination folder.
class RemotePreviewCache {
  RemotePreviewCache._();
  static final RemotePreviewCache instance = RemotePreviewCache._();

  Directory? _dir;

  /// Files larger than this are not auto-downloaded for preview; the user must
  /// explicitly request it.
  static const int autoDownloadLimit = 30 * 1024 * 1024; // 30 MB

  Future<Directory> _cacheDir() async {
    if (_dir != null) return _dir!;
    final tmp = await getTemporaryDirectory();
    final d = Directory(p.join(tmp.path, 'sehcontrol_preview'));
    if (!await d.exists()) {
      await d.create(recursive: true);
    }
    _dir = d;
    return d;
  }

  String _safeName(Entry entry) =>
      '${entry.modifiedTime}_${entry.size}_${p.basename(entry.path)}';

  /// Returns the local cached path for [entry] on the remote side controlled by
  /// [controller], downloading it if necessary. Returns null on failure.
  Future<String?> fetch(
    FileController controller,
    Entry entry, {
    void Function(double progress)? onProgress,
    Duration timeout = const Duration(seconds: 120),
    bool silent = false,
  }) async {
    try {
      final dir = await _cacheDir();
      final dest = p.join(dir.path, _safeName(entry));
      final cached = File(dest);
      if (await cached.exists() && await cached.length() == entry.size) {
        onProgress?.call(1.0);
        return dest;
      }

      final jobController = controller.jobController;
      final jobID = jobController.addTransferJob(entry, true); // remote -> local
      await bind.sessionSendFiles(
        sessionId: controller.sessionId,
        actId: jobID,
        path: entry.path,
        to: dest,
        fileNum: 0,
        includeHidden: false,
        isRemote: true,
        isDir: entry.isDirectory,
      );

      final completer = Completer<String?>();
      Worker? worker;
      Timer? timer;
      void finish(String? result) {
        if (completer.isCompleted) return;
        worker?.dispose();
        timer?.cancel();
        if (silent) {
          // Keep transient thumbnail downloads out of the transfer list.
          jobController.jobTable.removeWhere((j) => j.id == jobID);
          jobController.jobTable.refresh();
        }
        completer.complete(result);
      }

      worker = ever(jobController.jobTable, (_) {
        final idx = jobController.getJob(jobID);
        if (idx < 0) return;
        final job = jobController.jobTable[idx];
        onProgress?.call(job.percent);
        if (job.state == JobState.done) {
          finish(dest);
        } else if (job.state == JobState.error) {
          finish(null);
        }
      });
      timer = Timer(timeout, () => finish(null));
      return completer.future;
    } catch (e) {
      debugPrint('RemotePreviewCache.fetch failed: $e');
      return null;
    }
  }

  // Throttle for on-demand grid thumbnails so browsing a folder full of remote
  // images doesn't spawn dozens of simultaneous downloads.
  static const int _maxConcurrentThumbs = 3;
  int _activeThumbs = 0;
  final _thumbQueue = <Completer<void>>[];

  Future<String?> fetchThumbnail(FileController controller, Entry entry) async {
    if (_activeThumbs >= _maxConcurrentThumbs) {
      final c = Completer<void>();
      _thumbQueue.add(c);
      await c.future;
    }
    _activeThumbs++;
    try {
      return await fetch(controller, entry, silent: true);
    } finally {
      _activeThumbs--;
      if (_thumbQueue.isNotEmpty) {
        _thumbQueue.removeAt(0).complete();
      }
    }
  }

  Future<void> clear() async {
    try {
      final d = _dir;
      if (d != null && await d.exists()) {
        await d.delete(recursive: true);
      }
      _dir = null;
    } catch (e) {
      debugPrint('RemotePreviewCache.clear failed: $e');
    }
  }
}

/// Reads the pixel dimensions of a local image file. Returns null on failure.
Future<Size?> readImageDimensions(String path) async {
  try {
    final bytes = await File(path).readAsBytes();
    final codec = await ui.instantiateImageCodec(bytes);
    final frame = await codec.getNextFrame();
    final size = Size(
        frame.image.width.toDouble(), frame.image.height.toDouble());
    frame.image.dispose();
    return size;
  } catch (e) {
    debugPrint('readImageDimensions failed: $e');
    return null;
  }
}

/// Reads a bounded chunk of a text file for inline preview.
Future<String?> readTextSnippet(String path,
    {int maxBytes = 200 * 1024}) async {
  try {
    final f = File(path);
    final len = await f.length();
    if (len <= maxBytes) {
      return await f.readAsString();
    }
    final raf = await f.open();
    try {
      final bytes = await raf.read(maxBytes);
      return String.fromCharCodes(bytes);
    } finally {
      await raf.close();
    }
  } catch (e) {
    debugPrint('readTextSnippet failed: $e');
    return null;
  }
}

class DiskUsage {
  final int total;
  final int free;
  const DiskUsage(this.total, this.free);
  int get used => (total - free).clamp(0, total);
  double get usedFraction => total > 0 ? used / total : 0;
}

/// Best-effort local disk usage for the filesystem containing [path].
/// Uses OS tools (no extra dependency); returns null when unavailable.
Future<DiskUsage?> getDiskUsage(String path) async {
  try {
    if (Platform.isWindows) {
      var drive = 'C:';
      if (path.length >= 2 && path[1] == ':') {
        drive = path.substring(0, 2);
      }
      final psCmd =
          "\$d=[System.IO.DriveInfo]::new('$drive\\'); \$d.TotalSize; \$d.AvailableFreeSpace";
      final res = await Process.run(
          'powershell', ['-NoProfile', '-Command', psCmd]);
      if (res.exitCode != 0) return null;
      final parts = res.stdout
          .toString()
          .trim()
          .split(RegExp(r'\s+'))
          .where((e) => e.isNotEmpty)
          .toList();
      if (parts.length < 2) return null;
      final total = int.tryParse(parts[0]);
      final free = int.tryParse(parts[1]);
      if (total == null || free == null || total <= 0) return null;
      return DiskUsage(total, free);
    } else {
      final res = await Process.run('df', ['-kP', path]);
      if (res.exitCode != 0) return null;
      final lines = res.stdout
          .toString()
          .trim()
          .split('\n')
          .where((e) => e.isNotEmpty)
          .toList();
      if (lines.length < 2) return null;
      final cols =
          lines[1].split(RegExp(r'\s+')).where((e) => e.isNotEmpty).toList();
      // Filesystem 1024-blocks Used Available Capacity Mounted
      if (cols.length < 4) return null;
      final blocks = int.tryParse(cols[1]);
      final avail = int.tryParse(cols[3]);
      if (blocks == null || avail == null || blocks <= 0) return null;
      return DiskUsage(blocks * 1024, avail * 1024);
    }
  } catch (e) {
    debugPrint('getDiskUsage failed: $e');
    return null;
  }
}

class VideoMeta {
  final Duration? duration;
  final int? width;
  final int? height;
  const VideoMeta({this.duration, this.width, this.height});
}

final Map<String, String> _toolPathCache = {};

/// Resolves an ffmpeg/ffprobe binary: prefers one bundled next to the app
/// executable (see build.py), then falls back to the PATH.
String resolveMediaTool(String name) {
  final cached = _toolPathCache[name];
  if (cached != null) return cached;
  final exe = Platform.isWindows ? '$name.exe' : name;
  String resolved = name; // PATH fallback
  try {
    final dir = File(Platform.resolvedExecutable).parent.path;
    final candidates = <String>[
      p.join(dir, exe),
      if (Platform.isMacOS) p.normalize(p.join(dir, '..', 'Resources', exe)),
    ];
    for (final c in candidates) {
      if (File(c).existsSync()) {
        resolved = c;
        break;
      }
    }
  } catch (e) {
    debugPrint('resolveMediaTool failed: $e');
  }
  _toolPathCache[name] = resolved;
  return resolved;
}

/// Reads duration/resolution of a local video via `ffprobe` when present.
Future<VideoMeta?> readVideoMeta(String path) async {
  try {
    final res = await Process.run(resolveMediaTool('ffprobe'), [
      '-v',
      'quiet',
      '-print_format',
      'json',
      '-show_format',
      '-show_streams',
      path,
    ]);
    if (res.exitCode != 0) return null;
    final data = jsonDecode(res.stdout.toString()) as Map<String, dynamic>;
    Duration? duration;
    final durStr = (data['format']?['duration'])?.toString();
    final durSecs = durStr == null ? null : double.tryParse(durStr);
    if (durSecs != null) {
      duration = Duration(milliseconds: (durSecs * 1000).round());
    }
    int? width, height;
    final streams = data['streams'];
    if (streams is List) {
      for (final s in streams) {
        if (s is Map && s['codec_type'] == 'video') {
          width = s['width'] is int ? s['width'] as int : null;
          height = s['height'] is int ? s['height'] as int : null;
          break;
        }
      }
    }
    return VideoMeta(duration: duration, width: width, height: height);
  } catch (e) {
    debugPrint('readVideoMeta failed (ffprobe missing?): $e');
    return null;
  }
}

/// Extracts a single still frame from [srcPath] via `ffmpeg` when present.
/// Returns the generated image path, or null on failure.
Future<String?> extractVideoThumbnail(String srcPath) async {
  try {
    final dir = await RemotePreviewCache.instance._cacheDir();
    final dest = p.join(dir.path, 'thumb_${srcPath.hashCode}.jpg');
    final existing = File(dest);
    if (await existing.exists() && await existing.length() > 0) {
      return dest;
    }
    final res = await Process.run(resolveMediaTool('ffmpeg'), [
      '-y',
      '-ss',
      '1',
      '-i',
      srcPath,
      '-frames:v',
      '1',
      '-vf',
      'scale=640:-2',
      dest,
    ]);
    if (res.exitCode == 0 && await File(dest).exists()) {
      return dest;
    }
    return null;
  } catch (e) {
    debugPrint('extractVideoThumbnail failed (ffmpeg missing?): $e');
    return null;
  }
}

String formatDuration(Duration d) {
  String two(int n) => n.toString().padLeft(2, '0');
  final h = d.inHours;
  final m = d.inMinutes.remainder(60);
  final s = d.inSeconds.remainder(60);
  return h > 0 ? '$h:${two(m)}:${two(s)}' : '${two(m)}:${two(s)}';
}

/// Opens [path] with the operating system's default application.
Future<bool> openWithSystem(String path) async {
  try {
    return await launchUrl(Uri.file(path));
  } catch (e) {
    debugPrint('openWithSystem failed: $e');
    return false;
  }
}
