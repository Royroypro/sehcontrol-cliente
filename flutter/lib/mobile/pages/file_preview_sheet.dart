import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/common/widgets/file_preview.dart';
import 'package:flutter_hbb/models/file_model.dart';

/// Shows a bottom-sheet preview for [entry] on mobile: image/text render inline
/// (remote files are fetched to a temporary cache), other types show metadata.
Future<void> showMobileFilePreview(
  BuildContext context, {
  required Entry entry,
  required FileController controller,
  required bool isLocal,
}) {
  return showModalBottomSheet(
    context: context,
    isScrollControlled: true,
    backgroundColor: Theme.of(context).scaffoldBackgroundColor,
    shape: const RoundedRectangleBorder(
      borderRadius: BorderRadius.vertical(top: Radius.circular(16)),
    ),
    builder: (_) => _MobileFilePreview(
      entry: entry,
      controller: controller,
      isLocal: isLocal,
    ),
  );
}

enum _Status { loading, ready, needFetch, error }

class _MobileFilePreview extends StatefulWidget {
  final Entry entry;
  final FileController controller;
  final bool isLocal;

  const _MobileFilePreview({
    Key? key,
    required this.entry,
    required this.controller,
    required this.isLocal,
  }) : super(key: key);

  @override
  State<_MobileFilePreview> createState() => _MobileFilePreviewState();
}

class _MobileFilePreviewState extends State<_MobileFilePreview> {
  _Status _status = _Status.loading;
  String? _localPath;
  Size? _dimensions;
  String? _text;
  double _progress = 0;
  int _requestId = 0;

  FileTypeInfo get _info => fileTypeInfoOf(widget.entry.name);

  @override
  void initState() {
    super.initState();
    _prepare();
  }

  Future<void> _prepare({bool forceFetch = false}) async {
    final id = ++_requestId;
    final info = _info;
    if (widget.isLocal) {
      _localPath = widget.entry.path;
      await _loadDerived(id, info);
      return;
    }
    final canPreview = info.canRenderInApp;
    final withinLimit =
        widget.entry.size <= RemotePreviewCache.autoDownloadLimit;
    if (!canPreview || (!withinLimit && !forceFetch)) {
      setState(() => _status =
          canPreview ? _Status.needFetch : _Status.ready);
      return;
    }
    setState(() {
      _status = _Status.loading;
      _progress = 0;
    });
    final path = await RemotePreviewCache.instance.fetch(
      widget.controller,
      widget.entry,
      onProgress: (p) {
        if (id == _requestId && mounted) setState(() => _progress = p);
      },
    );
    if (id != _requestId || !mounted) return;
    if (path == null) {
      setState(() => _status = _Status.error);
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
        _status = _Status.ready;
      });
      return;
    }
    if ((info.kind == PreviewKind.text || info.kind == PreviewKind.code) &&
        _localPath != null) {
      final text = await readTextSnippet(_localPath!);
      if (id != _requestId || !mounted) return;
      setState(() {
        _text = text;
        _status = _Status.ready;
      });
      return;
    }
    if (id != _requestId || !mounted) return;
    setState(() => _status = _Status.ready);
  }

  void _transfer() {
    final sel = SelectedItems(isLocal: widget.controller.isLocal)
      ..add(widget.entry);
    widget.controller
        .sendFiles(sel, widget.controller.getOtherSideDirectoryData());
    Navigator.of(context).pop();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return FractionallySizedBox(
      heightFactor: 0.85,
      child: Column(
        children: [
          Container(
            width: 40,
            height: 4,
            margin: const EdgeInsets.symmetric(vertical: 8),
            decoration: BoxDecoration(
              color: MyTheme.darkGray,
              borderRadius: BorderRadius.circular(2),
            ),
          ),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16),
            child: Row(
              children: [
                Icon(_info.icon, size: 20, color: _info.color),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(
                    widget.entry.name,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: const TextStyle(fontWeight: FontWeight.w600),
                  ),
                ),
                IconButton(
                  icon: const Icon(Icons.close),
                  onPressed: () => Navigator.of(context).pop(),
                ),
              ],
            ),
          ),
          const Divider(height: 1),
          Expanded(child: _buildPreviewArea(theme)),
          _buildMetadata(theme),
          SafeArea(
            top: false,
            child: Padding(
              padding: const EdgeInsets.fromLTRB(16, 4, 16, 12),
              child: SizedBox(
                width: double.infinity,
                child: ElevatedButton.icon(
                  icon: Icon(
                      widget.isLocal ? Icons.arrow_upward : Icons.arrow_downward,
                      size: 18),
                  label: Text(translate(widget.isLocal ? 'Send' : 'Receive')),
                  onPressed: _transfer,
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildPreviewArea(ThemeData theme) {
    final info = _info;
    Widget content;
    switch (_status) {
      case _Status.loading:
        content = Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            SizedBox(
              width: 36,
              height: 36,
              child: CircularProgressIndicator(
                strokeWidth: 3,
                value: _progress > 0 && _progress < 1 ? _progress : null,
              ),
            ),
            const SizedBox(height: 10),
            Text(
              _progress > 0
                  ? '${translate('Downloading')} ${(_progress * 100).toStringAsFixed(0)}%'
                  : translate('Loading'),
              style: TextStyle(color: MyTheme.darkGray, fontSize: 12),
            ),
          ],
        );
        break;
      case _Status.error:
        content = _placeholder(
            Icons.error_outline, translate('Preview failed'), Colors.redAccent);
        break;
      case _Status.needFetch:
        content = Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(info.icon, size: 64, color: info.color),
            const SizedBox(height: 12),
            Text(readableFileSize(widget.entry.size.toDouble()),
                style: TextStyle(color: MyTheme.darkGray, fontSize: 12)),
            const SizedBox(height: 12),
            ElevatedButton.icon(
              icon: const Icon(Icons.visibility_outlined, size: 18),
              label: Text(translate('Preview')),
              onPressed: () => _prepare(forceFetch: true),
            ),
          ],
        );
        break;
      case _Status.ready:
        content = _buildReady(theme, info);
        break;
    }
    return Container(
      width: double.infinity,
      alignment: Alignment.center,
      padding: const EdgeInsets.all(16),
      child: content,
    );
  }

  Widget _buildReady(ThemeData theme, FileTypeInfo info) {
    if (info.kind == PreviewKind.image && _localPath != null) {
      return ClipRRect(
        borderRadius: BorderRadius.circular(8),
        child: InteractiveViewer(
          maxScale: 5,
          child: Image.file(File(_localPath!), fit: BoxFit.contain,
              errorBuilder: (_, __, ___) => _placeholder(
                  Icons.broken_image_outlined,
                  translate('Preview failed'),
                  MyTheme.darkGray)),
        ),
      );
    }
    if ((info.kind == PreviewKind.text || info.kind == PreviewKind.code) &&
        _text != null) {
      return Container(
        width: double.infinity,
        padding: const EdgeInsets.all(10),
        decoration: BoxDecoration(
          color: theme.cardColor,
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
    return _placeholder(info.icon, info.label, info.color);
  }

  Widget _placeholder(IconData icon, String label, Color color) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(icon, size: 60, color: color),
        const SizedBox(height: 10),
        Text(label, style: TextStyle(color: MyTheme.darkGray, fontSize: 12)),
      ],
    );
  }

  Widget _buildMetadata(ThemeData theme) {
    final entry = widget.entry;
    final rows = <Widget>[
      _metaRow(translate('Type'), _info.label),
      _metaRow(translate('Size'), readableFileSize(entry.size.toDouble())),
      if (_dimensions != null)
        _metaRow(translate('Resolution'),
            '${_dimensions!.width.toInt()} x ${_dimensions!.height.toInt()}'),
      _metaRow(translate('Modified'), _fmtDate(entry.lastModified())),
      _metaRow(translate('Path'), entry.path, mono: true),
    ];
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 8, 16, 4),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: rows,
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
            width: 92,
            child: Text(label,
                style: TextStyle(color: MyTheme.darkGray, fontSize: 12)),
          ),
          Expanded(
            child: Text(value,
                style: TextStyle(
                    fontSize: 12, fontFamily: mono ? 'monospace' : null)),
          ),
        ],
      ),
    );
  }

  String _fmtDate(DateTime d) {
    String two(int n) => n.toString().padLeft(2, '0');
    return '${two(d.day)}/${two(d.month)}/${d.year} ${two(d.hour)}:${two(d.minute)}';
  }
}
