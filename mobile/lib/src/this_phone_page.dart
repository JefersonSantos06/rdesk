import 'dart:async';

import 'package:flutter/services.dart';
import 'package:material_ui/material_ui.dart';

import 'i18n.dart';
import 'rust/api/app.dart' as app;
import 'rust/api/host.dart' as api;
import 'screen_share.dart';

/// This phone as a host: other devices connect to it and see its screen. Touches from the
/// other side are only logged for now (view only).
class ThisPhonePage extends StatefulWidget {
  const ThisPhonePage({super.key});

  @override
  State<ThisPhonePage> createState() => _ThisPhonePageState();
}

class _ThisPhonePageState extends State<ThisPhonePage> {
  api.HostView? _view;
  StreamSubscription<api.HostView>? _sub;
  bool _signedIn = false;
  bool _busy = false;
  String? _error;

  @override
  void initState() {
    super.initState();
    _view = api.hostStatus();
    _sub = api.watchHost().listen((v) {
      if (mounted) setState(() => _view = v);
    });
    app.account().then((a) {
      if (mounted) setState(() => _signedIn = a != null);
    });
  }

  @override
  void dispose() {
    _sub?.cancel();
    super.dispose();
  }

  /// Run `f`, show its error, and refresh the status.
  Future<void> _do(Future<void> Function() f) async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await f();
    } catch (e) {
      _error = e.toString();
    }
    if (!mounted) return;
    setState(() {
      _busy = false;
      _view = api.hostStatus();
    });
  }

  /// Share the screen (Android asks first), then accept connections.
  Future<void> _turnOn() async {
    if (!await ScreenShare.start()) {
      throw t(
        'Android did not allow sharing the screen, so access stays off.',
        'O Android não permitiu compartilhar a tela, então o acesso continua desligado.',
      );
    }
    await api.startHost();
  }

  /// The host first: stopping the screen while it runs reports the screen as lost.
  Future<void> _turnOff() async {
    await api.stopHost();
    await ScreenShare.stop();
  }

  /// Changes to the password or the server take effect when the host starts again.
  Future<void> _restartIfRunning() async {
    if (_view?.running != true) return;
    await api.stopHost();
    await api.startHost();
  }

  Future<void> _setPassword() async {
    final suggested = api.generateHostPassword();
    final ctl = TextEditingController(text: suggested);
    final password = await showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(t('This phone\'s password', 'Senha deste celular')),
        content: Column(mainAxisSize: MainAxisSize.min, crossAxisAlignment: CrossAxisAlignment.start, children: [
          Text(t(
            'Opens this phone from the local network or Tailscale, without the server. Note it down now: it is not shown again.',
            'Abre este celular pela rede local ou pelo Tailscale, sem o servidor. Anote agora: ela não aparece de novo.',
          )),
          const SizedBox(height: 12),
          TextField(controller: ctl, autocorrect: false, enableSuggestions: false),
        ]),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx), child: Text(t('Cancel', 'Cancelar'))),
          FilledButton(onPressed: () => Navigator.pop(ctx, ctl.text), child: Text(t('Save', 'Salvar'))),
        ],
      ),
    );
    if (password == null || password.isEmpty) return;
    await _do(() async {
      await api.setHostPassword(password: password);
      await _restartIfRunning();
    });
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final v = _view;
    final running = v?.running ?? false;
    final state = switch (v?.state) {
      'starting' => t('Starting…', 'Iniciando…'),
      'listening' => t('Waiting for connections', 'Esperando conexões'),
      'failed' => t('Stopped with an error', 'Parou com um erro'),
      _ => t('Off', 'Desligado'),
    };
    return Scaffold(
      appBar: AppBar(title: Text(t('This phone', 'Este celular'))),
      body: ListView(children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 12, 16, 4),
          child: Text(
            t(
              'Whoever connects sees this phone\'s screen while access is on. View only for now: their touches do not reach the phone yet. Android stops sharing when the screen locks.',
              'Quem se conectar vê a tela deste celular enquanto o acesso estiver ligado. Por enquanto só visualização: os toques de lá ainda não chegam ao celular. O Android para o compartilhamento quando a tela bloqueia.',
            ),
            style: theme.textTheme.bodySmall,
          ),
        ),
        SwitchListTile(
          secondary: const Icon(Icons.phonelink_ring_outlined),
          title: Text(t('Allow access to this phone', 'Permitir acesso a este celular')),
          subtitle: Text(state),
          value: running,
          onChanged: _busy ? null : (on) => _do(on ? _turnOn : _turnOff),
        ),
        if (v?.error != null)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16),
            child: Text(v!.error!, style: TextStyle(color: theme.colorScheme.error)),
          ),
        if (v?.sessionClient != null)
          ListTile(
            leading: const Icon(Icons.cast_connected),
            title: Text(t('${v!.sessionClient} is connected', '${v.sessionClient} está conectado')),
            trailing: TextButton(onPressed: api.disconnectHostSession, child: Text(t('Disconnect', 'Desconectar'))),
          ),
        if (v?.hostId != null)
          ListTile(
            leading: const Icon(Icons.fingerprint),
            title: Text(t('Host ID', 'ID do host')),
            subtitle: Text(v!.hostId!, style: const TextStyle(fontFamily: 'monospace')),
            trailing: IconButton(
              tooltip: t('Copy', 'Copiar'),
              icon: const Icon(Icons.copy),
              onPressed: () {
                Clipboard.setData(ClipboardData(text: v.hostId!));
                ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(t('ID copied.', 'ID copiado.'))));
              },
            ),
          ),
        _Section(t('Server', 'Servidor')),
        if (v?.registration != null) ...[
          ListTile(leading: const Icon(Icons.cloud_done_outlined), title: Text(v!.registration!)),
          ListTile(
            leading: const Icon(Icons.cloud_off_outlined),
            title: Text(t('Remove from the server', 'Remover do servidor')),
            enabled: !_busy && _signedIn,
            onTap: () => _do(() async {
              await api.unregisterThisPhone();
              await _restartIfRunning();
            }),
          ),
        ] else
          ListTile(
            leading: const Icon(Icons.cloud_upload_outlined),
            title: Text(t('Add to my server', 'Adicionar ao meu servidor')),
            subtitle: Text(_signedIn
                ? t('It shows up in your list of computers', 'Ele aparece na sua lista de computadores')
                : t('Sign in to a server first', 'Entre num servidor primeiro')),
            enabled: !_busy && _signedIn,
            onTap: () => _do(() async {
              await api.registerThisPhone();
              await _restartIfRunning();
            }),
          ),
        if (v?.registrationError != null)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16),
            child: Text(v!.registrationError!, style: TextStyle(color: theme.colorScheme.error)),
          ),
        _Section(t('Password', 'Senha')),
        ListTile(
          leading: const Icon(Icons.password),
          title: Text(v?.hasPassword == true
              ? t('Change the password', 'Trocar a senha')
              : t('Set a password', 'Definir uma senha')),
          subtitle: Text(t('For the local network, without the server', 'Para a rede local, sem o servidor')),
          enabled: !_busy,
          onTap: _setPassword,
        ),
        if (v?.hasPassword == true)
          ListTile(
            leading: const Icon(Icons.delete_outline),
            title: Text(t('Remove the password', 'Remover a senha')),
            enabled: !_busy,
            onTap: () => _do(() async {
              await api.clearHostPassword();
              await _restartIfRunning();
            }),
          ),
        if (_error != null)
          Padding(
            padding: const EdgeInsets.all(16),
            child: Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
          ),
        if (v != null && v.events.isNotEmpty) ...[
          _Section(t('Recent events', 'Eventos recentes')),
          for (final e in v.events.reversed.take(20))
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 2, 16, 2),
              child: Text(e, style: theme.textTheme.bodySmall),
            ),
          const SizedBox(height: 24),
        ],
      ]),
    );
  }
}

class _Section extends StatelessWidget {
  const _Section(this.text);
  final String text;

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.fromLTRB(16, 20, 16, 6),
        child: Text(text, style: Theme.of(context).textTheme.labelLarge?.copyWith(color: Theme.of(context).colorScheme.primary)),
      );
}
