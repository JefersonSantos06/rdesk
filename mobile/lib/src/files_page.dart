import 'dart:async';
import 'dart:io';

import 'package:file_picker/file_picker.dart';
import 'package:material_ui/material_ui.dart';
import 'package:path_provider/path_provider.dart';
import 'package:share_plus/share_plus.dart';

import 'i18n.dart';
import 'rust/api/session.dart';
import 'session_hub.dart';

/// The computer's files: browse, download (then share or open on the phone), upload.
class FilesPage extends StatefulWidget {
  const FilesPage({super.key, required this.hub});
  final SessionHub hub;

  @override
  State<FilesPage> createState() => _FilesPageState();
}

class _Transfer {
  _Transfer(this.name);
  final String name;
  int done = 0;
  int total = 0;
  bool? ok;
  String? result;
}

class _FilesPageState extends State<FilesPage> {
  StreamSubscription<SessionEvent>? _sub;
  String _path = '';
  List<DirItem> _entries = [];
  String? _error;
  bool _loading = true;
  final Map<int, _Transfer> _transfers = {};

  @override
  void initState() {
    super.initState();
    _sub = widget.hub.events.listen(_onEvent);
    widget.hub.addListener(_maybeList);
    _maybeList();
  }

  @override
  void dispose() {
    _sub?.cancel();
    widget.hub.removeListener(_maybeList);
    super.dispose();
  }

  bool _listedOnce = false;

  /// List the start folder once the session is up.
  void _maybeList() {
    if (!_listedOnce && widget.hub.connected) {
      _listedOnce = true;
      _list('');
    }
  }

  void _list(String path) {
    setState(() => _loading = true);
    widget.hub.session.listDir(path: path);
  }

  void _onEvent(SessionEvent e) {
    if (!mounted) return;
    switch (e.kind) {
      case EventKind.dirListing:
        setState(() {
          _loading = false;
          if (e.ok) {
            _path = e.text;
            _entries = [...e.entries]..sort((a, b) {
                if (a.isDir != b.isDir) return a.isDir ? -1 : 1;
                return a.name.toLowerCase().compareTo(b.name.toLowerCase());
              });
            _error = null;
          } else {
            _error = e.detail;
          }
        });
      case EventKind.transferProgress:
        setState(() {
          final tr = _transfers.putIfAbsent(e.id, () => _Transfer(e.text));
          tr.done = e.x;
          tr.total = e.y;
        });
      case EventKind.transferDone:
        setState(() {
          final tr = _transfers.putIfAbsent(e.id, () => _Transfer(e.text));
          tr.ok = e.ok;
          tr.result = e.text;
        });
        if (e.ok && File(e.text).existsSync()) _offerShare(e.text);
      default:
    }
  }

  String _join(String dir, String name) {
    if (dir.isEmpty) return name;
    final sep = dir.contains('\\') && !dir.contains('/') ? '\\' : '/';
    return dir.endsWith(sep) ? '$dir$name' : '$dir$sep$name';
  }

  String _parent(String p) {
    final s = p.replaceAll('\\', '/');
    final trimmed = s.endsWith('/') && s.length > 1 ? s.substring(0, s.length - 1) : s;
    final i = trimmed.lastIndexOf('/');
    if (i <= 0) return trimmed.startsWith('/') ? '/' : '';
    final parent = trimmed.substring(0, i);
    // Windows drive root: "C:" → "C:/".
    return parent.endsWith(':') ? '$parent/' : parent;
  }

  Future<void> _download(DirItem e) async {
    final dir = await getApplicationDocumentsDirectory();
    final into = Directory('${dir.path}/Downloads');
    await into.create(recursive: true);
    widget.hub.session.download(remote: _join(_path, e.name), localDir: into.path);
    if (mounted) {
      ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(t('Downloading ${e.name}…', 'Baixando ${e.name}…'))));
    }
  }

  Future<void> _upload() async {
    final picked = await FilePicker.pickFiles();
    for (final f in picked) {
      final p = f.path;
      if (p != null) widget.hub.session.upload(path: p);
    }
  }

  void _offerShare(String path) {
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
      content: Text(t('Downloaded: ${path.split('/').last}', 'Baixado: ${path.split('/').last}')),
      action: SnackBarAction(
        label: t('Share', 'Compartilhar'),
        onPressed: () => SharePlus.instance.share(ShareParams(files: [XFile(path)])),
      ),
    ));
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final access = widget.hub.access;
    final canFiles = access?.files ?? true;
    return Scaffold(
      appBar: AppBar(
        title: Text(t('Files', 'Arquivos')),
        actions: [
          IconButton(tooltip: t('Refresh', 'Atualizar'), icon: const Icon(Icons.refresh), onPressed: () => _list(_path)),
        ],
      ),
      floatingActionButton: canFiles && widget.hub.connected
          ? FloatingActionButton.extended(
              onPressed: _upload,
              icon: const Icon(Icons.upload),
              label: Text(t('Send a file', 'Enviar arquivo')),
            )
          : null,
      body: Column(children: [
        Material(
          color: theme.colorScheme.surfaceContainerHighest,
          child: ListTile(
            dense: true,
            leading: IconButton(
              icon: const Icon(Icons.arrow_upward),
              onPressed: _path.isEmpty ? null : () => _list(_parent(_path)),
            ),
            title: Text(_path.isEmpty ? '…' : _path, maxLines: 2, overflow: TextOverflow.ellipsis),
          ),
        ),
        if (!widget.hub.connected)
          Padding(padding: const EdgeInsets.all(16), child: Text(widget.hub.fatal ?? widget.hub.status)),
        if (_error != null) Padding(padding: const EdgeInsets.all(16), child: Text(_error!, style: TextStyle(color: theme.colorScheme.error))),
        if (_loading && widget.hub.connected) const LinearProgressIndicator(),
        Expanded(
          child: ListView(padding: const EdgeInsets.only(bottom: 96), children: [
            for (final tr in _transfers.values.toList().reversed)
              ListTile(
                leading: Icon(
                  tr.ok == null ? Icons.sync : (tr.ok! ? Icons.check_circle_outline : Icons.error_outline),
                  color: tr.ok == false ? theme.colorScheme.error : null,
                ),
                title: Text(tr.name),
                subtitle: tr.ok == null
                    ? LinearProgressIndicator(value: tr.total > 0 ? tr.done / tr.total : null)
                    : Text(tr.result ?? '', maxLines: 2, overflow: TextOverflow.ellipsis),
                onTap: tr.ok == true && tr.result != null && File(tr.result!).existsSync()
                    ? () => SharePlus.instance.share(ShareParams(files: [XFile(tr.result!)]))
                    : null,
              ),
            if (_transfers.isNotEmpty) const Divider(),
            for (final e in _entries)
              ListTile(
                leading: Icon(e.isDir ? Icons.folder : Icons.insert_drive_file_outlined),
                title: Text(e.name),
                subtitle: e.isDir ? null : Text(bytes(e.size)),
                onTap: e.isDir ? () => _list(_join(_path, e.name)) : () => _download(e),
                trailing: e.isDir
                    ? IconButton(
                        tooltip: t('Download the folder', 'Baixar a pasta'),
                        icon: const Icon(Icons.download),
                        onPressed: () => _download(e),
                      )
                    : null,
              ),
          ]),
        ),
      ]),
    );
  }
}
