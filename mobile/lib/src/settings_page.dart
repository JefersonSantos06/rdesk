import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';

import 'accounts.dart';
import 'device_key.dart';
import 'help.dart';
import 'i18n.dart';
import 'prefs.dart';
import 'rust/api/app.dart' as api;
import 'rust/api/device_key.dart' as dk;
import 'sign_in_page.dart';
import 'touch.dart';

/// The account, this phone's key, and about.
class SettingsPage extends StatefulWidget {
  const SettingsPage({super.key});

  @override
  State<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends State<SettingsPage> {
  api.Account? _account;
  String? _keyLine;
  String? _keyFingerprint;
  String _version = '';
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    final account = await api.account();
    final version = await api.appVersion();
    String? line, fp;
    try {
      final pk = await DeviceKey.publicKey();
      if (pk != null) {
        line = await dk.deviceKeyLine(publicKey: pk, comment: 'rdesk-phone');
        fp = await dk.deviceKeyFingerprint(publicKey: pk);
      }
    } catch (e) {
      _error = e.toString();
    }
    if (!mounted) return;
    setState(() {
      _account = account;
      _version = version;
      _keyLine = line;
      _keyFingerprint = fp;
    });
  }

  Future<void> _createKey() async {
    if (_keyLine != null) {
      final ok = await showDialog<bool>(
        context: context,
        builder: (ctx) => AlertDialog(
          title: Text(t('Make a new key?', 'Criar uma chave nova?')),
          content: Text(t(
            'Computers that authorized the current key will no longer accept this phone until you add the new one.',
            'Os computadores que autorizaram a chave atual não vão mais aceitar este celular até você adicionar a nova.',
          )),
          actions: [
            TextButton(onPressed: () => Navigator.pop(ctx, false), child: Text(t('Cancel', 'Cancelar'))),
            FilledButton(onPressed: () => Navigator.pop(ctx, true), child: Text(t('Make it', 'Criar'))),
          ],
        ),
      );
      if (ok != true) return;
    }
    try {
      await DeviceKey.create();
      await _load();
    } catch (e) {
      setState(() => _error = e.toString());
    }
  }

  Future<void> _deleteKey() async {
    await DeviceKey.delete();
    await _load();
  }

  Future<void> _signOut() async {
    await api.signOut();
    if (!mounted) return;
    Navigator.of(context).pushAndRemoveUntil(MaterialPageRoute(builder: (_) => const SignInPage()), (_) => false);
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final a = _account;
    return Scaffold(
      appBar: AppBar(title: Text(t('Settings', 'Configurações'))),
      body: ListView(children: [
        _Section(t('Account', 'Conta')),
        if (a != null) ...[
          ListTile(
            leading: const Icon(Icons.person_outline),
            title: Text(a.name.isEmpty ? a.email : a.name),
            subtitle: Text('${a.email}\n${a.serverName} · ${t('${a.daysLeft} more days', 'mais ${a.daysLeft} dias')}'),
            isThreeLine: true,
          ),
          ListTile(
            leading: const Icon(Icons.swap_horiz),
            title: Text(t('Change server', 'Trocar servidor')),
            subtitle: Text(t('Or add one: each keeps its own e-mail', 'Ou adicione um: cada um tem o seu e-mail')),
            onTap: () => showServerSwitcher(context),
          ),
          ListTile(leading: const Icon(Icons.logout), title: Text(t('Sign out', 'Sair')), onTap: _signOut),
        ] else
          ListTile(
            leading: const Icon(Icons.login),
            title: Text(t('Sign in to a server', 'Entrar num servidor')),
            onTap: () => Navigator.of(context)
                .pushAndRemoveUntil(MaterialPageRoute(builder: (_) => const SignInPage()), (_) => false),
          ),
        _Section(t('Remote control', 'Controle remoto')),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 4, 16, 8),
          child: SegmentedButton<TouchMode>(
            segments: [
              ButtonSegment(value: TouchMode.touchpad, icon: const Icon(Icons.mouse_outlined), label: Text(t('Touchpad', 'Touchpad'))),
              ButtonSegment(value: TouchMode.direct, icon: const Icon(Icons.touch_app_outlined), label: Text(t('Direct touch', 'Toque direto'))),
            ],
            selected: {Prefs.touchMode},
            onSelectionChanged: (s) => setState(() => Prefs.touchMode = s.first),
          ),
        ),
        ListTile(
          title: Text(t('Pointer speed (touchpad)', 'Velocidade do ponteiro (touchpad)')),
          subtitle: Slider(
            value: Prefs.sensitivity,
            min: 0.5,
            max: 2,
            divisions: 6,
            label: '${Prefs.sensitivity.toStringAsFixed(2)}×',
            onChanged: (v) => setState(() => Prefs.sensitivity = v),
          ),
        ),
        ListTile(
          leading: const Icon(Icons.help_outline),
          title: Text(t('Gestures', 'Gestos')),
          onTap: () => showGestureHelp(context, Prefs.touchMode),
        ),
        _Section(t('This phone\'s key', 'Chave deste celular')),
        Padding(
          padding: const EdgeInsets.symmetric(horizontal: 16),
          child: Text(
            t(
              'For computers with the extra lock: a key kept in the phone\'s secure hardware, used with your fingerprint or face. Add it on the computer in This computer → Security → SSH keys (or `rdesk host add-key "…"`).',
              'Para computadores com o cadeado extra: uma chave guardada no hardware seguro do celular, usada com a sua digital ou rosto. Adicione no computador em Este computador → Segurança → Chaves SSH (ou `rdesk host add-key "…"`).',
            ),
            style: theme.textTheme.bodySmall,
          ),
        ),
        if (_keyLine != null) ...[
          ListTile(
            leading: const Icon(Icons.key),
            title: Text(_keyFingerprint ?? ''),
            subtitle: Text(_keyLine!, maxLines: 3, overflow: TextOverflow.ellipsis),
            trailing: IconButton(
              tooltip: t('Copy', 'Copiar'),
              icon: const Icon(Icons.copy),
              onPressed: () {
                Clipboard.setData(ClipboardData(text: _keyLine!));
                ScaffoldMessenger.of(context)
                    .showSnackBar(SnackBar(content: Text(t('Key copied.', 'Chave copiada.'))));
              },
            ),
          ),
          ListTile(leading: const Icon(Icons.refresh), title: Text(t('Make a new key', 'Criar uma chave nova')), onTap: _createKey),
          ListTile(leading: const Icon(Icons.delete_outline), title: Text(t('Delete the key', 'Apagar a chave')), onTap: _deleteKey),
        ] else
          ListTile(leading: const Icon(Icons.add), title: Text(t('Make this phone\'s key', 'Criar a chave deste celular')), onTap: _createKey),
        if (_error != null)
          Padding(
            padding: const EdgeInsets.all(16),
            child: Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
          ),
        _Section(t('About', 'Sobre')),
        ListTile(leading: const Icon(Icons.info_outline), title: Text('rdesk $_version')),
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
