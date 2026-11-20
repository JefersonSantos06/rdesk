package dev.rdesk.rdesk

import android.content.pm.PackageManager
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.security.Signature
import java.security.spec.ECGenParameterSpec

/**
 * The phone's own key (channel `dev.rdesk/device_key`): ECDSA P-256 in the Android
 * Keystore (StrongBox when the phone has one). The private key never leaves the hardware;
 * every signature needs the fingerprint, face or screen lock.
 */
class DeviceKey(private val activity: FragmentActivity) : MethodChannel.MethodCallHandler {
    private companion object {
        const val ALIAS = "rdesk-device-key"
    }

    private fun store(): KeyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    override fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        try {
            when (call.method) {
                // SubjectPublicKeyInfo (X.509), as the Rust side expects from Android.
                "publicKey" -> result.success(store().getCertificate(ALIAS)?.publicKey?.encoded)
                "create" -> result.success(create())
                "delete" -> {
                    store().deleteEntry(ALIAS)
                    result.success(null)
                }
                "sign" -> sign(call.argument<ByteArray>("data")!!, call.argument<String>("reason") ?: "rdesk", result)
                else -> result.notImplemented()
            }
        } catch (e: Exception) {
            result.error("device_key", e.message, null)
        }
    }

    private fun create(): ByteArray {
        if (Build.VERSION.SDK_INT < 30) throw IllegalStateException("needs Android 11 or newer")
        store().deleteEntry(ALIAS)
        fun spec(strongBox: Boolean) = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_SIGN)
            .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
            .setDigests(KeyProperties.DIGEST_SHA256)
            .setUserAuthenticationRequired(true)
            .setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG or KeyProperties.AUTH_DEVICE_CREDENTIAL)
            .setIsStrongBoxBacked(strongBox)
            .build()
        val gen = KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, "AndroidKeyStore")
        val strongBox = activity.packageManager.hasSystemFeature(PackageManager.FEATURE_STRONGBOX_KEYSTORE)
        return try {
            gen.initialize(spec(strongBox))
            gen.generateKeyPair().public.encoded
        } catch (e: StrongBoxUnavailableException) {
            gen.initialize(spec(false))
            gen.generateKeyPair().public.encoded
        }
    }

    private fun sign(data: ByteArray, reason: String, result: MethodChannel.Result) {
        val key = store().getKey(ALIAS, null) as? PrivateKey
            ?: return result.error("device_key", "no key on this phone", null)
        val signature = Signature.getInstance("SHA256withECDSA").apply { initSign(key) }
        var answered = false
        fun answer(bytes: ByteArray) {
            if (!answered) {
                answered = true
                result.success(bytes)
            }
        }
        val prompt = BiometricPrompt(
            activity,
            ContextCompat.getMainExecutor(activity),
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(r: BiometricPrompt.AuthenticationResult) {
                    try {
                        val s = r.cryptoObject!!.signature!!
                        s.update(data)
                        answer(s.sign())
                    } catch (e: Exception) {
                        answer(ByteArray(0))
                    }
                }

                // Cancelled, locked out…: the Rust side reports "did not sign".
                override fun onAuthenticationError(code: Int, msg: CharSequence) = answer(ByteArray(0))
            },
        )
        val info = BiometricPrompt.PromptInfo.Builder()
            .setTitle("rdesk")
            .setSubtitle(reason)
            .setAllowedAuthenticators(BIOMETRIC_STRONG or DEVICE_CREDENTIAL)
            .build()
        activity.runOnUiThread { prompt.authenticate(info, BiometricPrompt.CryptoObject(signature)) }
    }
}
