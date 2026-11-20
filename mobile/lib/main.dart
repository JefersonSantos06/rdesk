import 'dart:io';

import 'package:device_info_plus/device_info_plus.dart';
import 'package:material_ui/material_ui.dart';
import 'package:path_provider/path_provider.dart';

import 'src/computers_page.dart';
import 'src/i18n.dart';
import 'src/rust/api/app.dart' as api;
import 'src/rust/frb_generated.dart';
import 'src/sign_in_page.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  final dir = await getApplicationSupportDirectory();
  await api.start(dataDir: dir.path, deviceName: await _deviceName(), portuguese: pt);
  final signedIn = await api.account() != null;
  final localOnly = !signedIn && await api.localOnly();
  runApp(RdeskApp(home: signedIn || localOnly ? ComputersPage(localOnly: localOnly) : const SignInPage()));
}

/// The name the people at the computer see ("João's Pixel 9").
Future<String> _deviceName() async {
  final info = DeviceInfoPlugin();
  try {
    if (Platform.isAndroid) {
      final a = await info.androidInfo;
      return a.name.isNotEmpty ? a.name : '${a.manufacturer} ${a.model}';
    }
    if (Platform.isIOS) return (await info.iosInfo).name;
  } catch (_) {}
  return 'phone';
}

class RdeskApp extends StatelessWidget {
  const RdeskApp({super.key, required this.home});
  final Widget home;

  @override
  Widget build(BuildContext context) {
    const seed = Color(0xFF2F6FED);
    return MaterialApp(
      title: 'rdesk',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(colorScheme: ColorScheme.fromSeed(seedColor: seed)),
      darkTheme: ThemeData(colorScheme: ColorScheme.fromSeed(seedColor: seed, brightness: Brightness.dark)),
      home: home,
    );
  }
}
