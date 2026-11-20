import Flutter
import LocalAuthentication
import Security
import UIKit

@main
@objc class AppDelegate: FlutterAppDelegate, FlutterImplicitEngineDelegate {
  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    return super.application(application, didFinishLaunchingWithOptions: launchOptions)
  }

  func didInitializeImplicitFlutterEngine(_ engineBridge: FlutterImplicitEngineBridge) {
    GeneratedPluginRegistrant.register(with: engineBridge.pluginRegistry)
    if let registrar = engineBridge.pluginRegistry.registrar(forPlugin: "RdeskDeviceKey") {
      DeviceKey.register(registrar.messenger())
    }
  }
}

/// The phone's own key (channel `dev.rdesk/device_key`): ECDSA P-256 in the Secure Enclave.
/// The private key never leaves it; every signature asks for Face ID / Touch ID / the code.
enum DeviceKey {
  private static let tag = "dev.rdesk.device-key".data(using: .utf8)!

  static func register(_ messenger: FlutterBinaryMessenger) {
    let channel = FlutterMethodChannel(name: "dev.rdesk/device_key", binaryMessenger: messenger)
    channel.setMethodCallHandler { call, result in
      switch call.method {
      case "publicKey":
        result(publicKey().map { FlutterStandardTypedData(bytes: $0) })
      case "create":
        do {
          result(FlutterStandardTypedData(bytes: try create()))
        } catch {
          result(FlutterError(code: "device_key", message: error.localizedDescription, details: nil))
        }
      case "delete":
        delete()
        result(nil)
      case "sign":
        guard let args = call.arguments as? [String: Any],
          let data = args["data"] as? FlutterStandardTypedData
        else {
          result(FlutterError(code: "device_key", message: "no data", details: nil))
          return
        }
        let reason = args["reason"] as? String ?? "rdesk"
        // Signing waits for Face ID: off the main thread.
        DispatchQueue.global(qos: .userInitiated).async {
          let signature = sign(data.data, reason: reason) ?? Data()
          DispatchQueue.main.async { result(FlutterStandardTypedData(bytes: signature)) }
        }
      default:
        result(FlutterMethodNotImplemented)
      }
    }
  }

  private static func privateKey(reason: String? = nil) -> SecKey? {
    var query: [String: Any] = [
      kSecClass as String: kSecClassKey,
      kSecAttrApplicationTag as String: tag,
      kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
      kSecReturnRef as String: true,
    ]
    if let reason {
      let context = LAContext()
      context.localizedReason = reason
      query[kSecUseAuthenticationContext as String] = context
    }
    var item: CFTypeRef?
    guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess, let item else { return nil }
    return (item as! SecKey)
  }

  /// The uncompressed point (65 bytes), as the Rust side expects from iOS.
  static func publicKey() -> Data? {
    guard let key = privateKey(), let pub = SecKeyCopyPublicKey(key) else { return nil }
    return SecKeyCopyExternalRepresentation(pub, nil) as Data?
  }

  static func delete() {
    SecItemDelete([kSecClass as String: kSecClassKey, kSecAttrApplicationTag as String: tag] as CFDictionary)
  }

  static func create() throws -> Data {
    delete()
    var error: Unmanaged<CFError>?
    #if targetEnvironment(simulator)
      let flags: SecAccessControlCreateFlags = [.userPresence]
    #else
      let flags: SecAccessControlCreateFlags = [.privateKeyUsage, .userPresence]
    #endif
    guard
      let access = SecAccessControlCreateWithFlags(
        kCFAllocatorDefault, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, flags, &error)
    else { throw error!.takeRetainedValue() as Error }
    var attributes: [String: Any] = [
      kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
      kSecAttrKeySizeInBits as String: 256,
      kSecPrivateKeyAttrs as String: [
        kSecAttrIsPermanent as String: true,
        kSecAttrApplicationTag as String: tag,
        kSecAttrAccessControl as String: access,
      ],
    ]
    #if !targetEnvironment(simulator)
      attributes[kSecAttrTokenID as String] = kSecAttrTokenIDSecureEnclave
    #endif
    guard let key = SecKeyCreateRandomKey(attributes as CFDictionary, &error),
      let pub = SecKeyCopyPublicKey(key),
      let data = SecKeyCopyExternalRepresentation(pub, &error) as Data?
    else { throw error!.takeRetainedValue() as Error }
    return data
  }

  /// ECDSA P-256 with SHA-256, DER (X9.62), or nil if cancelled.
  static func sign(_ data: Data, reason: String) -> Data? {
    guard let key = privateKey(reason: reason) else { return nil }
    var error: Unmanaged<CFError>?
    return SecKeyCreateSignature(key, .ecdsaSignatureMessageX962SHA256, data as CFData, &error) as Data?
  }
}
