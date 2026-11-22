import 'package:material_ui/material_ui.dart';

import 'computers_page.dart';
import 'i18n.dart';
import 'rust/api/app.dart' as api;
import 'sign_in_page.dart';

/// "Change server": the accounts saved on the phone (one per server and e-mail), the one in
/// use checked. Choosing another shows its computers; one whose sign-in ended asks for it
/// again. Sessions already open keep the account they were opened with.
Future<void> showServerSwitcher(BuildContext context) async {
  final accounts = await api.accounts();
  if (!context.mounted) return;
  final nav = Navigator.of(context);
  final choice = await showModalBottomSheet<_Choice>(
    context: context,
    showDragHandle: true,
    isScrollControlled: true,
    builder: (ctx) => _Switcher(accounts: accounts),
  );
  if (choice == null) return;
  switch (choice) {
    case _Use(:final account):
      final now = await api.useAccount(key: account.key);
      if (now == null) {
        // Its sign-in ended: sign in to the same server again.
        nav.pushAndRemoveUntil(
          MaterialPageRoute(builder: (_) => SignInPage(server: account.server)),
          (_) => false,
        );
      } else {
        nav.pushAndRemoveUntil(MaterialPageRoute(builder: (_) => const ComputersPage()), (_) => false);
      }
    case _Add():
      nav.push(MaterialPageRoute(builder: (_) => const SignInPage(adding: true)));
    case _Remove(:final account):
      await api.removeAccount(key: account.key);
      final left = await api.account();
      if (left == null) {
        nav.pushAndRemoveUntil(MaterialPageRoute(builder: (_) => const SignInPage()), (_) => false);
      } else if (context.mounted) {
        await showServerSwitcher(context);
      }
  }
}

sealed class _Choice {}

class _Use extends _Choice {
  _Use(this.account);
  final api.Account account;
}

class _Add extends _Choice {}

class _Remove extends _Choice {
  _Remove(this.account);
  final api.Account account;
}

class _Switcher extends StatelessWidget {
  const _Switcher({required this.accounts});
  final List<api.SavedAccount> accounts;

  Future<void> _confirmRemove(BuildContext context, api.Account a) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(t('Sign out of ${a.serverName}?', 'Sair de ${a.serverName}?')),
        content: Text(t(
          '${a.email} on ${a.server}. To use its computers again you sign in again.',
          '${a.email} em ${a.server}. Para usar os computadores dele de novo, você entra outra vez.',
        )),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx, false), child: Text(t('Cancel', 'Cancelar'))),
          FilledButton(onPressed: () => Navigator.pop(ctx, true), child: Text(t('Sign out', 'Sair'))),
        ],
      ),
    );
    if (ok == true && context.mounted) Navigator.pop(context, _Remove(a));
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return SafeArea(
      child: SingleChildScrollView(
        child: Column(mainAxisSize: MainAxisSize.min, crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: Text(t('Change server', 'Trocar servidor'), style: theme.textTheme.titleMedium),
          ),
          for (final s in accounts)
            ListTile(
              leading: Icon(s.active ? Icons.radio_button_checked : Icons.radio_button_unchecked,
                  color: s.active ? theme.colorScheme.primary : null),
              title: Text(s.account.serverName),
              subtitle: Text(s.expired
                  ? t('${s.account.email} · sign in again', '${s.account.email} · entre de novo')
                  : s.account.email),
              trailing: IconButton(
                tooltip: t('Sign out', 'Sair'),
                icon: const Icon(Icons.logout),
                onPressed: () => _confirmRemove(context, s.account),
              ),
              onTap: s.active && !s.expired ? () => Navigator.pop(context) : () => Navigator.pop(context, _Use(s.account)),
            ),
          const Divider(),
          ListTile(
            leading: const Icon(Icons.add),
            title: Text(t('Add a server', 'Adicionar servidor')),
            onTap: () => Navigator.pop(context, _Add()),
          ),
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 4, 16, 12),
            child: Text(
              t(
                'Each server keeps its own e-mail and its computers stay apart. Sessions already open keep their server.',
                'Cada servidor tem o seu e-mail e os computadores de um não aparecem no outro. Sessões já abertas continuam no servidor delas.',
              ),
              style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
            ),
          ),
        ]),
      ),
    );
  }
}
