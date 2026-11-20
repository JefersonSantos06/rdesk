import 'dart:ui';

/// The app speaks Brazilian Portuguese when the phone does, English otherwise (like the
/// desktop app's "system" setting).
final bool pt = PlatformDispatcher.instance.locale.languageCode == 'pt';

/// `t('Connect', 'Conectar')`.
String t(String en, String ptBr) => pt ? ptBr : en;

/// "3 h ago" / "há 3 h".
String ago(int unixSeconds) {
  final s = DateTime.now().millisecondsSinceEpoch ~/ 1000 - unixSeconds;
  String v;
  if (s < 90) {
    v = t('a minute', '1 min');
  } else if (s < 3600) {
    v = '${s ~/ 60} min';
  } else if (s < 86400) {
    v = '${s ~/ 3600} h';
  } else {
    final d = s ~/ 86400;
    v = pt ? '$d ${d == 1 ? 'dia' : 'dias'}' : '$d ${d == 1 ? 'day' : 'days'}';
  }
  return pt ? 'há $v' : '$v ago';
}

String bytes(int n) {
  if (n < 1000) return '$n B';
  if (n < 1000 * 1000) return '${(n / 1000).toStringAsFixed(0)} KB';
  if (n < 1000 * 1000 * 1000) return '${(n / 1e6).toStringAsFixed(1)} MB';
  return '${(n / 1e9).toStringAsFixed(1)} GB';
}
