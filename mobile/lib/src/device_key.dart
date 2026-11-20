import 'package:flutter/services.dart';

/// The phone's own key for computers with the extra lock: an ECDSA P-256 key made in the
/// secure hardware (Android Keystore / Secure Enclave). The private half never leaves it;
/// each signature asks the phone's lock (fingerprint, face, PIN).
///
/// Native side: `DeviceKey.kt` (Android) and `DeviceKey.swift` (iOS).
class DeviceKey {
  static const _ch = MethodChannel('dev.rdesk/device_key');

  /// The public key (Android: SubjectPublicKeyInfo; iOS: the uncompressed point), or
  /// null if none was made yet.
  static Future<Uint8List?> publicKey() async {
    final r = await _ch.invokeMethod<Uint8List>('publicKey');
    return (r == null || r.isEmpty) ? null : r;
  }

  /// Make the key (replacing an existing one) and return its public half.
  static Future<Uint8List> create() async => (await _ch.invokeMethod<Uint8List>('create'))!;

  static Future<void> delete() => _ch.invokeMethod('delete');

  /// Sign `data` (ECDSA with SHA-256, DER). Shows the phone's prompt; returns an empty
  /// list if the user cancels.
  static Future<Uint8List> sign(Uint8List data, String reason) async {
    try {
      final r = await _ch.invokeMethod<Uint8List>('sign', {'data': data, 'reason': reason});
      return r ?? Uint8List(0);
    } on PlatformException {
      return Uint8List(0);
    }
  }
}
