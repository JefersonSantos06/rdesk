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
}
