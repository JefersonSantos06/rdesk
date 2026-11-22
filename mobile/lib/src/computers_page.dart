import 'dart:io';

import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';

import 'accounts.dart';
import 'i18n.dart';
import 'rust/api/app.dart' as api;
import 'rust/api/session.dart';
import 'session_page.dart';
import 'settings_page.dart';
import 'update.dart';

/// What to open in a session.
enum Open { screen, files, terminal }

/// The computers this account may reach on the server, and the ones saved on the phone.
class ComputersPage extends StatefulWidget {
  const ComputersPage({super.key, this.localOnly = false});

  /// Used without a server: only computers saved on the phone.
  final bool localOnly;

  @override
  State<ComputersPage> createState() => _ComputersPageState();
}

class _ComputersPageState extends State<ComputersPage> {
  List<api.Computer> _server = [];
  List<api.SavedComputer> _saved = [];
  api.Account? _account;
  String? _error;
  bool _loading = true;
  String _query = '';
  api.Update? _update;

  @override
  void initState() {
    super.initState();
    _refresh();
    if (Platform.isAndroid) {
      api.checkUpdate().then((u) {
        if (mounted) setState(() => _update = u);
      }).catchError((_) {});
    }
  }

  Future<void> _refresh() async {
    setState(() => _loading = true);
    String? error;
    final account = widget.localOnly ? null : await api.account();
    var server = <api.Computer>[];
    if (account != null) {
      try {
        server = await api.computers();
      } catch (e) {
        error = e.toString();
      }
    }
    final saved = await api.savedComputers().catchError((_) => <api.SavedComputer>[]);
    if (!mounted) return;
    setState(() {
      _account = account;
      _server = server;
      _saved = saved;
      _error = error;
      _loading = false;
    });
  }

  bool _matches(String name, [String extra = '']) {
    if (_query.isEmpty) return true;
    final q = _query.toLowerCase();
    return name.toLowerCase().contains(q) || extra.toLowerCase().contains(q);
  }

  void _open(Target target, Open what) {
    Navigator.of(context).push(MaterialPageRoute(builder: (_) => SessionPage(target: target, open: what)));
  }

  Target _targetOf(api.Computer c) =>
      Target(name: c.name, hostId: c.hostId, machineId: c.machineId, addrs: c.addrs);

  Target _targetOfSaved(api.SavedComputer c) =>
      Target(name: c.name, hostId: c.hostId, machineId: null, addrs: c.addr.isEmpty ? [] : [c.addr]);

  void _actions(api.Computer c) {
    showModalBottomSheet(
      context: context,
      showDragHandle: true,
      builder: (ctx) => SafeArea(
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          ListTile(
            title: Text(c.name, style: Theme.of(ctx).textTheme.titleMedium),
            subtitle: Text('${c.os} · ${c.owner}'),
          ),
          if (c.canSee)
            ListTile(
              leading: const Icon(Icons.desktop_windows_outlined),
              title: Text(t('Connect', 'Conectar')),
              onTap: () {
                Navigator.pop(ctx);
                _open(_targetOf(c), Open.screen);
              },
            ),
          if (c.canFiles)
            ListTile(
              leading: const Icon(Icons.folder_outlined),
              title: Text(t('Files', 'Arquivos')),
              onTap: () {
                Navigator.pop(ctx);
                _open(_targetOf(c), Open.files);
              },
            ),
          if (c.canTerminal)
            ListTile(
              leading: const Icon(Icons.terminal),
              title: Text(t('Terminal', 'Terminal')),
              onTap: () {
                Navigator.pop(ctx);
                _open(_targetOf(c), Open.terminal);
              },
            ),
          ListTile(
            leading: const Icon(Icons.copy),
            title: Text(t('Copy the host ID', 'Copiar o ID do host')),
            onTap: () {
              Clipboard.setData(ClipboardData(text: c.hostId));
              Navigator.pop(ctx);
            },
          ),
        ]),
      ),
    );
  }

  void _savedActions(api.SavedComputer c) {
    showModalBottomSheet(
      context: context,
      showDragHandle: true,
      builder: (ctx) => SafeArea(
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          ListTile(
            title: Text(c.name, style: Theme.of(ctx).textTheme.titleMedium),
            subtitle: Text(c.addr.isEmpty ? t('Found on this network', 'Achado nesta rede') : c.addr),
          ),
          for (final (icon, label, what) in [
            (Icons.desktop_windows_outlined, t('Connect', 'Conectar'), Open.screen),
            (Icons.folder_outlined, t('Files', 'Arquivos'), Open.files),
            (Icons.terminal, t('Terminal', 'Terminal'), Open.terminal),
          ])
            ListTile(
              leading: Icon(icon),
              title: Text(label),
              onTap: () {
                Navigator.pop(ctx);
                _open(_targetOfSaved(c), what);
              },
            ),
          ListTile(
            leading: const Icon(Icons.edit_outlined),
            title: Text(t('Edit', 'Editar')),
            onTap: () {
              Navigator.pop(ctx);
              _edit(c);
            },
          ),
          ListTile(
            leading: const Icon(Icons.delete_outline),
            title: Text(t('Forget on this phone', 'Esquecer neste celular')),
            onTap: () async {
              Navigator.pop(ctx);
              await api.forgetComputer(hostId: c.hostId);
              _refresh();
            },
          ),
        ]),
      ),
    );
  }

  Future<void> _edit([api.SavedComputer? c]) async {
    final saved = await showDialog<bool>(context: context, builder: (_) => _SavedDialog(existing: c));
    if (saved == true) _refresh();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final server = _server.where((c) => _matches(c.name, '${c.owner} ${c.hostId}')).toList();
    final saved = _saved.where((c) => _matches(c.name, '${c.addr} ${c.hostId}')).toList();
    return Scaffold(
      appBar: AppBar(
        title: InkWell(
          borderRadius: BorderRadius.circular(8),
          onTap: () => showServerSwitcher(context),
          child: Padding(
            padding: const EdgeInsets.symmetric(vertical: 4),
            child: Row(mainAxisSize: MainAxisSize.min, children: [
              Flexible(
                child: Column(crossAxisAlignment: CrossAxisAlignment.start, mainAxisSize: MainAxisSize.min, children: [
                  Text(t('Computers', 'Computadores')),
                  Text(
                    _account == null ? t('Without a server', 'Sem servidor') : '${_account!.serverName} · ${_account!.email}',
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                  ),
                ]),
              ),
              const Icon(Icons.arrow_drop_down),
            ]),
          ),
        ),
        actions: [
          IconButton(
            tooltip: t('Settings', 'Configurações'),
            icon: const Icon(Icons.settings_outlined),
            onPressed: () async {
              await Navigator.of(context).push(MaterialPageRoute(builder: (_) => const SettingsPage()));
              _refresh();
            },
          ),
        ],
        bottom: PreferredSize(
          preferredSize: const Size.fromHeight(64),
          child: Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 12),
            child: SearchBar(
              hintText: t('Search', 'Buscar'),
              leading: const Icon(Icons.search),
              elevation: const WidgetStatePropertyAll(0),
              onChanged: (v) => setState(() => _query = v.trim()),
            ),
          ),
        ),
      ),
      floatingActionButton: FloatingActionButton.extended(
        onPressed: () => _edit(),
        icon: const Icon(Icons.add),
        label: Text(t('On this network', 'Nesta rede')),
      ),
      body: RefreshIndicator(
        onRefresh: _refresh,
        child: ListView(
          padding: const EdgeInsets.only(bottom: 96),
          children: [
            if (_update != null) UpdateBanner(update: _update!),
            if (_error != null)
              ListTile(
                leading: Icon(Icons.error_outline, color: theme.colorScheme.error),
                title: Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
              ),
            if (_loading && _server.isEmpty && _saved.isEmpty)
              const Padding(padding: EdgeInsets.all(48), child: Center(child: CircularProgressIndicator())),
            if (_account != null) ...[
              _Header(t('On ${_account!.serverName}', 'Em ${_account!.serverName}')),
              if (!_loading && server.isEmpty)
                Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
                  child: Text(t('No computers.', 'Nenhum computador.')),
                ),
              for (final c in server)
                ListTile(
                  leading: _OsIcon(os: c.os, online: c.online),
                  title: Text(c.name),
                  subtitle: Text(_statusOf(c)),
                  trailing: c.direct ? Tooltip(message: t('Direct access on', 'Acesso direto ligado'), child: const Icon(Icons.warning_amber, size: 18)) : null,
                  enabled: c.canAccess,
                  onTap: c.canAccess
                      ? () => _open(_targetOf(c), c.canSee ? Open.screen : (c.canFiles ? Open.files : Open.terminal))
                      : null,
                  onLongPress: c.canAccess ? () => _actions(c) : null,
                ),
            ],
            if (saved.isNotEmpty || _account == null) ...[
              _Header(t('Saved on this phone', 'Salvos neste celular')),
              if (saved.isEmpty)
                Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
                  child: Text(t(
                    'Add a computer on this network with its host ID (the computer shows it in This computer).',
                    'Adicione um computador desta rede pelo ID do host (o computador mostra em Este computador).',
                  )),
                ),
              for (final c in saved)
                ListTile(
                  leading: const Icon(Icons.lan_outlined),
                  title: Text(c.name),
                  subtitle: Text(c.addr.isEmpty ? t('Found on this network', 'Achado nesta rede') : c.addr),
                  onTap: () => _open(_targetOfSaved(c), Open.screen),
                  onLongPress: () => _savedActions(c),
                ),
            ],
          ],
        ),
      ),
    );
  }

  String _statusOf(api.Computer c) {
    final who = c.isMine ? '' : ' · ${c.owner}';
    if (!c.canAccess) return t('Not shared with you', 'Não compartilhado com você') + who;
    if (c.inSession) return t('In use', 'Em uso') + who;
    if (c.online) return t('Online', 'Online') + who;
    final seen = c.lastSeen;
    return (seen == null ? t('Offline', 'Offline') : '${t('Offline', 'Offline')} · ${ago(seen)}') + who;
  }
}

class _Header extends StatelessWidget {
  const _Header(this.text);
  final String text;

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.fromLTRB(16, 16, 16, 4),
        child: Text(text, style: Theme.of(context).textTheme.labelLarge?.copyWith(color: Theme.of(context).colorScheme.primary)),
      );
}

class _OsIcon extends StatelessWidget {
  const _OsIcon({required this.os, required this.online});
  final String os;
  final bool online;

  @override
  Widget build(BuildContext context) {
    final o = os.toLowerCase();
    final icon = o.contains('windows')
        ? Icons.window
        : o.contains('mac')
            ? Icons.laptop_mac
            : o.contains('linux')
                ? Icons.computer
                : Icons.desktop_windows_outlined;
    final c = Theme.of(context).colorScheme;
    return Stack(clipBehavior: Clip.none, children: [
      Icon(icon, color: online ? c.onSurface : c.outline),
      Positioned(
        right: -2,
        bottom: -2,
        child: Container(
          width: 10,
          height: 10,
          decoration: BoxDecoration(
            color: online ? Colors.green : c.outline,
            shape: BoxShape.circle,
            border: Border.all(color: c.surface, width: 2),
          ),
        ),
      ),
    ]);
  }
}

class _SavedDialog extends StatefulWidget {
  const _SavedDialog({this.existing});
  final api.SavedComputer? existing;

  @override
  State<_SavedDialog> createState() => _SavedDialogState();
}

class _SavedDialogState extends State<_SavedDialog> {
  late final _name = TextEditingController(text: widget.existing?.name);
  late final _id = TextEditingController(text: widget.existing?.hostId);
  late final _addr = TextEditingController(text: widget.existing?.addr);
  String? _error;

  Future<void> _save() async {
    try {
      await api.saveComputer(name: _name.text, hostId: _id.text, addr: _addr.text);
      if (mounted) Navigator.pop(context, true);
    } catch (e) {
      setState(() => _error = e.toString());
    }
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Text(t('A computer on this network', 'Um computador nesta rede')),
      content: SingleChildScrollView(
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          TextField(controller: _name, decoration: InputDecoration(labelText: t('Name', 'Nome'))),
          TextField(
            controller: _id,
            enabled: widget.existing == null,
            autocorrect: false,
            decoration: InputDecoration(labelText: t('Host ID', 'ID do host')),
          ),
          TextField(
            controller: _addr,
            autocorrect: false,
            keyboardType: TextInputType.url,
            decoration: InputDecoration(
              labelText: t('Address (optional)', 'Endereço (opcional)'),
              helperText: t('IP, name or Tailscale name; empty: look on this network',
                  'IP, nome ou nome do Tailscale; vazio: procurar nesta rede'),
              helperMaxLines: 2,
            ),
          ),
          if (_error != null)
            Padding(
              padding: const EdgeInsets.only(top: 12),
              child: Text(_error!, style: TextStyle(color: Theme.of(context).colorScheme.error)),
            ),
        ]),
      ),
      actions: [
        TextButton(onPressed: () => Navigator.pop(context), child: Text(t('Cancel', 'Cancelar'))),
        FilledButton(onPressed: _save, child: Text(t('Save', 'Salvar'))),
      ],
    );
  }
}
