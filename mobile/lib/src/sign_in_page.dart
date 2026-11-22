import 'package:material_ui/material_ui.dart';
import 'package:flutter_web_auth_2/flutter_web_auth_2.dart';

import 'computers_page.dart';
import 'i18n.dart';
import 'rust/api/app.dart' as api;

/// Sign in to an rdesk server in the phone's browser (Google/GitHub + second factor), use a
/// server already saved, or use the app without a server (computers on this network only).
class SignInPage extends StatefulWidget {
  const SignInPage({super.key, this.server, this.adding = false});

  /// Pre-filled server (one whose sign-in ended).
  final String? server;

  /// Signing in to one more server while another is in use: a way back, nothing about
  /// using the app without a server.
  final bool adding;

  @override
  State<SignInPage> createState() => _SignInPageState();
}

class _SignInPageState extends State<SignInPage> {
  late final _server = TextEditingController(text: widget.server);
  bool _busy = false;
  String? _error;
  List<api.SavedAccount> _saved = [];

  @override
  void initState() {
    super.initState();
    if (!widget.adding && widget.server == null) {
      api.lastServer().then((s) {
        if (s != null && _server.text.isEmpty && mounted) setState(() => _server.text = s);
      });
    }
    api.accounts().then((a) {
      if (mounted) setState(() => _saved = a);
    });
  }

  Future<void> _use(api.SavedAccount s) async {
    final now = await api.useAccount(key: s.account.key);
    if (!mounted) return;
    if (now == null) {
      setState(() {
        _server.text = s.account.server;
        _error = t('Your sign-in to ${s.account.serverName} ended. Sign in again.',
            'Seu acesso a ${s.account.serverName} terminou. Entre de novo.');
      });
      return;
    }
    Navigator.of(context).pushAndRemoveUntil(MaterialPageRoute(builder: (_) => const ComputersPage()), (_) => false);
  }

  Future<void> _signIn() async {
    final server = _server.text.trim();
    if (server.isEmpty) {
      setState(() => _error = t('Type your rdesk server\'s address.', 'Digite o endereço do seu servidor rdesk.'));
      return;
    }
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final url = await api.beginSignIn(server: server);
      final back = await FlutterWebAuth2.authenticate(url: url, callbackUrlScheme: 'rdesk');
      await api.finishSignIn(callbackUrl: back);
      if (!mounted) return;
      Navigator.of(context).pushAndRemoveUntil(MaterialPageRoute(builder: (_) => const ComputersPage()), (_) => false);
    } catch (e) {
      final msg = e.toString();
      final cancelled = msg.contains('CANCELED') || msg.contains('canceled') || msg.contains('cancelled');
      setState(() => _error = cancelled ? null : msg);
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _withoutServer() async {
    await api.setLocalOnly(enabled: true);
    if (!mounted) return;
    Navigator.of(context).pushReplacement(
      MaterialPageRoute(builder: (_) => const ComputersPage(localOnly: true)),
    );
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final others = _saved.where((s) => widget.adding || !s.active).toList();
    return Scaffold(
      appBar: widget.adding ? AppBar(title: Text(t('Add a server', 'Adicionar servidor'))) : null,
      body: SafeArea(
        child: Center(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(24),
            child: ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 420),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Icon(Icons.desktop_windows_outlined, size: 56, color: theme.colorScheme.primary),
                  const SizedBox(height: 12),
                  Text('rdesk', textAlign: TextAlign.center, style: theme.textTheme.headlineMedium),
                  const SizedBox(height: 4),
                  Text(
                    widget.adding
                        ? t('Sign in to one more rdesk server; you change between them on the computers page.',
                            'Entre em mais um servidor rdesk; você troca entre eles na página de computadores.')
                        : t('Reach your computers from the phone.', 'Acesse seus computadores pelo celular.'),
                    textAlign: TextAlign.center,
                    style: theme.textTheme.bodyMedium?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                  ),
                  const SizedBox(height: 32),
                  TextField(
                    controller: _server,
                    enabled: !_busy,
                    keyboardType: TextInputType.url,
                    autocorrect: false,
                    textInputAction: TextInputAction.go,
                    onSubmitted: (_) => _signIn(),
                    decoration: InputDecoration(
                      labelText: t('Server', 'Servidor'),
                      hintText: 'rdesk.example.com',
                      border: const OutlineInputBorder(),
                    ),
                  ),
                  const SizedBox(height: 12),
                  FilledButton.icon(
                    onPressed: _busy ? null : _signIn,
                    icon: _busy
                        ? const SizedBox(width: 18, height: 18, child: CircularProgressIndicator(strokeWidth: 2))
                        : const Icon(Icons.login),
                    label: Text(t('Sign in with the browser', 'Entrar pelo navegador')),
                  ),
                  if (_error != null) ...[
                    const SizedBox(height: 12),
                    Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
                  ],
                  if (others.isNotEmpty) ...[
                    const SizedBox(height: 24),
                    Text(t('Or use a saved server:', 'Ou use um servidor salvo:'), style: theme.textTheme.labelLarge),
                    for (final s in others)
                      ListTile(
                        contentPadding: EdgeInsets.zero,
                        leading: const Icon(Icons.dns_outlined),
                        title: Text(s.account.serverName),
                        subtitle: Text(s.expired
                            ? t('${s.account.email} · sign in again', '${s.account.email} · entre de novo')
                            : s.account.email),
                        onTap: _busy ? null : () => _use(s),
                      ),
                  ],
                  if (widget.adding) const SizedBox.shrink() else ...[
                  const SizedBox(height: 32),
                  const Divider(),
                  const SizedBox(height: 8),
                  OutlinedButton.icon(
                    onPressed: _busy ? null : _withoutServer,
                    icon: const Icon(Icons.wifi),
                    label: Text(t('Use on this network, without a server', 'Usar nesta rede, sem servidor')),
                  ),
                  const SizedBox(height: 6),
                  Text(
                    t(
                      'Computers on the local network or Tailscale, each with its own password or key.',
                      'Computadores da rede local ou do Tailscale, cada um com a própria senha ou chave.',
                    ),
                    textAlign: TextAlign.center,
                    style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                  ),
                  ],
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}
