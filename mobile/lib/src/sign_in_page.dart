import 'package:material_ui/material_ui.dart';
import 'package:flutter_web_auth_2/flutter_web_auth_2.dart';

import 'computers_page.dart';
import 'i18n.dart';
import 'rust/api/app.dart' as api;

/// Sign in to the rdesk server in the phone's browser (Google/GitHub + second factor), or
/// use the app without a server (computers on this network only).
class SignInPage extends StatefulWidget {
  const SignInPage({super.key});

  @override
  State<SignInPage> createState() => _SignInPageState();
}

class _SignInPageState extends State<SignInPage> {
  final _server = TextEditingController();
  bool _busy = false;
  String? _error;

  @override
  void initState() {
    super.initState();
    api.lastServer().then((s) {
      if (s != null && _server.text.isEmpty && mounted) setState(() => _server.text = s);
    });
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
      Navigator.of(context).pushReplacement(MaterialPageRoute(builder: (_) => const ComputersPage()));
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
    return Scaffold(
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
                    t('Reach your computers from the phone.', 'Acesse seus computadores pelo celular.'),
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
              ),
            ),
          ),
        ),
      ),
    );
  }
}
