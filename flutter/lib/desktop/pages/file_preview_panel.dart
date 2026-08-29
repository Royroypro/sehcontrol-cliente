import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/common/widgets/file_preview.dart';
import 'package:flutter_hbb/models/file_model.dart';

enum _PreviewStatus { loading, ready, needFetch, error }

/// Right-hand "Vista previa" panel for the desktop file manager.
///
/// Shows metadata for the selected [entry] and, when possible, an inline
/// preview: images and text render directly, media files expose a play button.
/// Remote files are fetched to a temporary cache on demand.
class FilePreviewPanel extends StatefulWidget {
  final Entry entry;
  final bool isLocal;
  final FileController controller;
  final VoidCallback onClose;

  const FilePreviewPanel({
    Key? key,
    required this.entry,
    required this.isLocal,
    required this.controller,
    required this.onClose,
  }) : super(key: key);

  @override
  State<FilePreviewPanel> createState() => _FilePreviewPanelState();
}

class _FilePreviewPanelState extends State<FilePreviewPanel> {
  _PreviewStatus _status = _PreviewStatus.loading;
  String? _localPath;
  Size? _dimensions;
  String? _text;
  String? _videoThumb;
  VideoMeta? _videoMeta;
  double _progress = 0;
  int _requestId = 0;

  FileTypeInfo get _info => fileTypeInfoOf(widget.entry.name);

  @override
  void initState() {
    super.initState();
    _prepare();
  }

  @override
  void didUpdateWidget(covariant FilePreviewPanel oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.entry.path != widget.entry.path ||
        oldWidget.entry.modifiedTime != widget.entry.modifiedTime ||
        oldWidget.isLocal != widget.isLocal) {
      _prepare();
    }
  }

  void _reset(_PreviewStatus status) {
    _localPath = null;
    _dimensions = null;
    _text = null;
    _videoThumb = null;
    _videoMeta = null;
    _progress = 0;
    _status = status;
  }

  Future<void> _prepare({bool forceFetch = false}) async {
    final id = ++_requestId;
    final info = _info;
    if (widget.isLocal) {
      setState(() => _reset(_PreviewStatus.loading));
      _localPath = widget.entry.path;
      await _loadDerived(id, info);
      return;
    }

    // Remote entry: needs a cache download to preview its contents.
    final canPreview = info.canRenderInApp || info.isMedia;
    final withinLimit =
        widget.entry.size <= RemotePreviewCache.autoDownloadLimit;
    if (!canPreview || (!withinLimit && !forceFetch)) {
      setState(() => _reset(
          canPreview ? _PreviewStatus.needFetch : _PreviewStatus.ready));
      return;
    }

    setState(() => _reset(_PreviewStatus.loading));
    final path = await RemotePreviewCache.instance.fetch(
      widget.controller,
      widget.entry,
      onProgress: (p) {
        if (id == _requestId && mounted) {
          setState(() => _progress = p);
        }
      },
    );
    if (id != _requestId || !mounted) return;
    if (path == null) {
      setState(() => _status = _PreviewStatus.error);
      return;
    }
    _localPath = path;
    await _loadDerived(id, info);
  }

  Future<void> _loadDerived(int id, FileTypeInfo info) async {
    if (info.kind == PreviewKind.image && _localPath != null) {
      final size = await readImageDimensions(_localPath!);
      if (id != _requestId || !mounted) return;
      setState(() {
        _dimensions = size;
        _status = _PreviewStatus.ready;
      });
      return;
    }
    if ((info.kind == PreviewKind.text || info.kind == PreviewKind.code) &&
        _localPath != null) {
      final text = await readTextSnippet(_localPath!);
      if (id != _requestId || !mounted) return;
      setState(() {
        _text = text;
        _status = _PreviewStatus.ready;
      });
      return;
    }
    if (info.kind == PreviewKind.video && _localPath != null) {
      // Mark ready first (icon fallback), then enrich with a still frame and
      // metadata via ffmpeg/ffprobe if they are installed on this machine.
      if (id != _requestId || !mounted) return;
      setState(() => _status = _PreviewStatus.ready);
      final meta = await readVideoMeta(_localPath!);
      if (id != _requestId || !mounted) return;
      if (meta != null) setState(() => _videoMeta = meta);
      final thumb = await extractVideoThumbnail(_localPath!);
      if (id != _requestId || !mounted) return;
      if (thumb != null) setState(() => _videoThumb = thumb);
      return;
    }
    if (id != _requestId || !mounted) return;
    setState(() => _status = _PreviewStatus.ready);
  }

  void _sendEntry() {
    final sel = SelectedItems(isLocal: widget.controller.isLocal)
      ..add(widget.entry);
    widget.controller
        .sendFiles(sel, widget.controller.getOtherSideDirectoryData());
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Container(
      margin: const EdgeInsets.all(16.0),
      decoration: BoxDecoration(
        color: theme.cardColor,
        borderRadius: BorderRadius.circular(15.0),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _buildHeader(theme),
          const Divider(height: 1),
          Expanded(child: _buildPreviewArea(theme)),
          _buildMetadata(theme),
          _buildActions(theme),
        ],
      ),
    );
  }

  Widget _buildHeader(ThemeData theme) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 12, 8, 12),
      child: Row(
        children: [
          Text(
            translate('Preview'),
            style: theme.textTheme.titleMedium
                ?.copyWith(fontWeight: FontWeight.w600),
          ),
          const SizedBox(width: 6),
          Icon(Icons.info_outline, size: 16, color: MyTheme.darkGray),
          const Spacer(),
          if (_localPath != null)
            IconButton(
              tooltip: translate('Open'),
              icon: const Icon(Icons.open_in_full, size: 18),
              onPressed: () => openWithSystem(_localPath!),
            ),
          IconButton(
            tooltip: translate('Close'),
            icon: const Icon(Icons.close, size: 18),
            onPressed: widget.onClose,
          ),
        ],
      ),
    );
  }

  Widget _buildPreviewArea(ThemeData theme) {
    final info = _info;
    Widget content;
    switch (_status) {
      case _PreviewStatus.loading:
        content = Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            SizedBox(
              width: 40,
              height: 40,
              child: CircularProgressIndicator(
                strokeWidth: 3,
                value: _progress > 0 && _progress < 1 ? _progress : null,
              ),
            ),
            const SizedBox(height: 12),
            Text(
              _progress > 0
                  ? '${translate('Downloading')} ${(_progress * 100).toStringAsFixed(0)}%'
                  : translate('Loading'),
              style: TextStyle(color: MyTheme.darkGray, fontSize: 12),
            ),
          ],
        );
        break;
      case _PreviewStatus.error:
        content = _iconPlaceholder(
            Icons.error_outline, translate('Preview failed'), Colors.redAccent);
        break;
      case _PreviewStatus.needFetch:
        content = Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(info.icon, size: 72, color: info.color),
            const SizedBox(height: 16),
            Text(
              readableFileSize(widget.entry.size.toDouble()),
              style: TextStyle(color: MyTheme.darkGray, fontSize: 12),
            ),
            const SizedBox(height: 12),
            ElevatedButton.icon(
              icon: const Icon(Icons.visibility_outlined, size: 18),
              label: Text(translate('Preview')),
              onPressed: () => _prepare(forceFetch: true),
            ),
          ],
        );
        break;
      case _PreviewStatus.ready:
        content = _buildReadyPreview(theme, info);
        break;
    }
    return Container(
      width: double.infinity,
      alignment: Alignment.center,
      padding: const EdgeInsets.all(16),
      child: content,
    );
  }

  Widget _buildReadyPreview(ThemeData theme, FileTypeInfo info) {
    if (info.kind == PreviewKind.image && _localPath != null) {
      return ClipRRect(
        borderRadius: BorderRadius.circular(8),
        child: InteractiveViewer(
          maxScale: 5,
          child: Image.file(
            File(_localPath!),
            fit: BoxFit.contain,
            errorBuilder: (_, __, ___) => _iconPlaceholder(
                Icons.broken_image_outlined,
                translate('Preview failed'),
                MyTheme.darkGray),
          ),
        ),
      );
    }
    if ((info.kind == PreviewKind.text || info.kind == PreviewKind.code) &&
        _text != null) {
      return Container(
        width: double.infinity,
        padding: const EdgeInsets.all(10),
        decoration: BoxDecoration(
          color: theme.scaffoldBackgroundColor,
          borderRadius: BorderRadius.circular(8),
        ),
        child: SingleChildScrollView(
          child: SelectableText(
            _text!,
            style: const TextStyle(
                fontFamily: 'monospace', fontSize: 12, height: 1.4),
          ),
        ),
      );
    }
    if (info.isMedia && _localPath != null) {
      // Video with an extracted still frame: show the frame with a play overlay.
      if (info.kind == PreviewKind.video && _videoThumb != null) {
        return GestureDetector(
          onTap: () => openWithSystem(_localPath!),
          child: ClipRRect(
            borderRadius: BorderRadius.circular(8),
            child: Stack(
              alignment: Alignment.center,
              children: [
                Image.file(File(_videoThumb!), fit: BoxFit.contain),
                Container(
                  width: 56,
                  height: 56,
                  decoration: BoxDecoration(
                    color: Colors.black54,
                    shape: BoxShape.circle,
                  ),
                  child: const Icon(Icons.play_arrow,
                      size: 32, color: Colors.white),
                ),
              ],
            ),
          ),
        );
      }
      return Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(info.icon, size: 72, color: info.color),
          const SizedBox(height: 16),
          ElevatedButton.icon(
            icon: const Icon(Icons.play_arrow, size: 20),
            label: Text(translate('Play')),
            onPressed: () => openWithSystem(_localPath!),
          ),
        ],
      );
    }
    // Non-previewable or media without a local copy yet.
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(info.icon, size: 72, color: info.color),
        const SizedBox(height: 12),
        Text(info.label,
            style: TextStyle(color: MyTheme.darkGray, fontSize: 12)),
        if (_localPath != null) ...[
          const SizedBox(height: 12),
          OutlinedButton.icon(
            icon: const Icon(Icons.open_in_new, size: 18),
            label: Text(translate('Open')),
            onPressed: () => openWithSystem(_localPath!),
          ),
        ],
      ],
    );
  }

  Widget _iconPlaceholder(IconData icon, String label, Color color) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(icon, size: 64, color: color),
        const SizedBox(height: 10),
        Text(label, style: TextStyle(color: MyTheme.darkGray, fontSize: 12)),
      ],
    );
  }

  Widget _buildMetadata(ThemeData theme) {
    final info = _info;
    final entry = widget.entry;
    final rows = <Widget>[
      _metaRow(translate('Type'), info.label),
      _metaRow(
        translate('Size'),
        '${readableFileSize(entry.size.toDouble())} (${_thousands(entry.size)} bytes)',
      ),
      if (_dimensions != null)
        _metaRow(translate('Resolution'),
            '${_dimensions!.width.toInt()} x ${_dimensions!.height.toInt()}'),
      if (_videoMeta?.duration != null)
        _metaRow(translate('Duration'), formatDuration(_videoMeta!.duration!)),
      if (_videoMeta?.width != null && _videoMeta?.height != null)
        _metaRow(translate('Resolution'),
            '${_videoMeta!.width} x ${_videoMeta!.height}'),
      _metaRow(translate('Modified'), _fmtDate(entry.lastModified())),
      _metaRow(translate('Path'), entry.path, mono: true),
    ];
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 8, 16, 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Icon(info.icon, size: 18, color: info.color),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  widget.entry.name,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(fontWeight: FontWeight.w600),
                ),
              ),
            ],
          ),
          const SizedBox(height: 8),
          ...rows,
        ],
      ),
    );
  }

  Widget _metaRow(String label, String value, {bool mono = false}) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 3),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 96,
            child: Text(label,
                style: TextStyle(color: MyTheme.darkGray, fontSize: 12)),
          ),
          Expanded(
            child: Text(
              value,
              style: TextStyle(
                fontSize: 12,
                fontFamily: mono ? 'monospace' : null,
              ),
            ),
          ),
        ],
      ),
    );
  }

  String? get _openablePath => widget.isLocal ? widget.entry.path : _localPath;

  Widget _buildActions(ThemeData theme) {
    final isLocal = widget.isLocal;
    final canOpen = _openablePath != null;
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 4, 16, 16),
      child: Column(
        children: [
          SizedBox(
            width: double.infinity,
            child: ElevatedButton.icon(
              style: ButtonStyle(
                backgroundColor: MaterialStateProperty.all(MyTheme.accent),
                foregroundColor: MaterialStateProperty.all(Colors.white),
              ),
              icon: Icon(
                  isLocal ? Icons.arrow_forward : Icons.arrow_back, size: 18),
              label: Text(translate(isLocal ? 'Send' : 'Receive')),
              onPressed: _sendEntry,
            ),
          ),
          const SizedBox(height: 8),
          Row(
            children: [
              if (canOpen)
                Expanded(
                  child: OutlinedButton.icon(
                    icon: const Icon(Icons.open_in_new, size: 18),
                    label: Text(translate('Open')),
                    onPressed: () => openWithSystem(_openablePath!),
                  ),
                ),
              if (canOpen) const SizedBox(width: 8),
              _buildMoreOptions(canOpen),
            ],
          ),
        ],
      ),
    );
  }

  Widget _buildMoreOptions(bool canOpen) {
    return PopupMenuButton<String>(
      tooltip: translate('More options'),
      position: PopupMenuPosition.under,
      onSelected: (value) {
        switch (value) {
          case 'copy_path':
            Clipboard.setData(ClipboardData(text: widget.entry.path));
            showToast('${widget.entry.path}\n${translate('Copied')}');
            break;
          case 'open':
            if (_openablePath != null) openWithSystem(_openablePath!);
            break;
        }
      },
      itemBuilder: (context) => [
        PopupMenuItem<String>(
          value: 'copy_path',
          child: Row(children: [
            const Icon(Icons.copy_outlined, size: 18),
            const SizedBox(width: 10),
            Text(translate('Copy path')),
          ]),
        ),
        if (canOpen)
          PopupMenuItem<String>(
            value: 'open',
            child: Row(children: [
              const Icon(Icons.open_in_new, size: 18),
              const SizedBox(width: 10),
              Text(translate('Open')),
            ]),
          ),
      ],
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 9),
        decoration: BoxDecoration(
          border: Border.all(color: Theme.of(context).dividerColor),
          borderRadius: BorderRadius.circular(6),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            const Icon(Icons.more_horiz, size: 18),
            const SizedBox(width: 6),
            Text(translate('More options')),
          ],
        ),
      ),
    );
  }

  String _thousands(int n) {
    final s = n.toString();
    final buf = StringBuffer();
    for (int i = 0; i < s.length; i++) {
      if (i > 0 && (s.length - i) % 3 == 0) buf.write(',');
      buf.write(s[i]);
    }
    return buf.toString();
  }

  String _fmtDate(DateTime d) {
    String two(int n) => n.toString().padLeft(2, '0');
    return '${two(d.day)}/${two(d.month)}/${d.year} ${two(d.hour)}:${two(d.minute)}';
  }
}
