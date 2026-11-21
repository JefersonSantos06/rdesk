import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:path_provider/path_provider.dart';

import 'touch.dart';

/// How the screen may turn during a session.
enum Turn { auto, landscape, portrait }

/// The phone's own choices (touch mode, pointer speed…), kept in a small JSON file next to
/// the app's data.
class Prefs {
  static File? _file;
  static Map<String, dynamic> _m = {};
  static Future<void> _writing = Future.value();

  static Future<void> load() async {
    final dir = await getApplicationSupportDirectory();
    final file = File('${dir.path}/phone.json');
    _file = file;
    try {
      _m = jsonDecode(await file.readAsString()) as Map<String, dynamic>;
    } catch (_) {
      _m = {};
    }
  }

  static void _set(String key, Object value) {
    _m[key] = value;
    final file = _file;
    if (file == null) return;
    final text = jsonEncode(_m);
    _writing = _writing.then((_) => file.writeAsString(text)).catchError((_) => file);
  }

  static TouchMode get touchMode => _m['touch_mode'] == 'direct' ? TouchMode.direct : TouchMode.touchpad;
  static set touchMode(TouchMode m) => _set('touch_mode', m.name);

  /// Touchpad pointer speed, 0.5..2.
  static double get sensitivity => ((_m['sensitivity'] as num?) ?? 1).toDouble().clamp(0.5, 2.0);
  static set sensitivity(double v) => _set('sensitivity', v);

  /// The gesture help was shown once.
  static bool get helpSeen => _m['help_seen'] == true;
  static set helpSeen(bool v) => _set('help_seen', v);

  static Turn get turn => Turn.values.firstWhere((t) => t.name == _m['turn'], orElse: () => Turn.auto);
  static set turn(Turn t) => _set('turn', t.name);

  /// Where the floating tools button rests: which side, and how far down (0..1).
  static bool get toolsLeft => _m['tools_left'] == true;
  static double get toolsY => ((_m['tools_y'] as num?) ?? 0.3).toDouble().clamp(0.0, 1.0);
  static void setToolsAt({required bool left, required double y}) {
    _set('tools_left', left);
    _set('tools_y', y);
  }
}
