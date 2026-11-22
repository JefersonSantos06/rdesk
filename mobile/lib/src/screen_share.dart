import 'package:flutter/services.dart';

/// Sharing this phone's screen with the rdesk host (Android: `ScreenShare.kt`).
class ScreenShare {
  static const _channel = MethodChannel('dev.rdesk/screen');

  /// Ask Android for the screen (its consent dialog) and start sharing; `false` if the
  /// person said no.
  static Future<bool> start() async => await _channel.invokeMethod<bool>('start') ?? false;

  /// Stop sharing (stop the host first, or it reports the screen as lost).
  static Future<void> stop() => _channel.invokeMethod('stop');

  static Future<bool> isSharing() async => await _channel.invokeMethod<bool>('isSharing') ?? false;

  /// Touch control (the accessibility service) is on in the system's settings.
  static Future<bool> touchEnabled() async => await _channel.invokeMethod<bool>('touchEnabled') ?? false;

  /// Open the system's accessibility settings (rdesk's entry where Android allows).
  static Future<void> openTouchSettings() => _channel.invokeMethod('openTouchSettings');

  /// Whoever is connected may browse all of the phone's files (not only Download/rdesk).
  static Future<bool> allFiles() async => await _channel.invokeMethod<bool>('allFiles') ?? false;

  /// Ask Android for it ("All files access", or the storage permission before Android 11).
  static Future<void> openAllFiles() => _channel.invokeMethod('openAllFiles');

  /// "Start on its own": share the screen and start the host when the phone starts or unlocks.
  static Future<bool> autoStart() async => await _channel.invokeMethod<bool>('autoStart') ?? false;

  static Future<void> setAutoStart(bool on) => _channel.invokeMethod('setAutoStart', on);

  /// Open rdesk's app info (Android 13+ keeps "Allow restricted settings" in its menu).
  static Future<void> openAppInfo() => _channel.invokeMethod('openAppInfo');
}
