import 'package:material_ui/material_ui.dart';
import 'package:open_filex/open_filex.dart';
import 'package:path_provider/path_provider.dart';

import 'i18n.dart';
import 'rust/api/app.dart' as api;

/// A newer version is released on the server (Android): download it, check it against the
/// signed release, and hand it to Android's installer (the user confirms there).
class UpdateBanner extends StatefulWidget {
  const UpdateBanner({super.key, required this.update});
  final api.Update update;

  @override
  State<UpdateBanner> createState() => _UpdateBannerState();
}

class _UpdateBannerState extends State<UpdateBanner> {
  bool _busy = false;
  String? _error;

  Future<void> _install() async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final dir = await getTemporaryDirectory();
      final path = await api.downloadUpdate(dir: dir.path);
      final r = await OpenFilex.open(path, type: 'application/vnd.android.package-archive');
      if (r.type != ResultType.done) setState(() => _error = r.message);
    } catch (e) {
      setState(() => _error = e.toString());
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final u = widget.update;
    return MaterialBanner(
      leading: const Icon(Icons.system_update),
      content: Text(_error ??
          t('rdesk ${u.version} is available (${bytes(u.size)}).', 'O rdesk ${u.version} está disponível (${bytes(u.size)}).')),
      actions: [
        TextButton(
          onPressed: _busy ? null : _install,
          child: _busy
              ? const SizedBox(width: 18, height: 18, child: CircularProgressIndicator(strokeWidth: 2))
              : Text(t('Update', 'Atualizar')),
        ),
      ],
    );
  }
}
