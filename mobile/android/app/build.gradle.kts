import java.util.Properties

plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// The release signing key (see mobile/README.md): android/key.properties, or the
// RDESK_ANDROID_* variables in CI. Every release must be signed with the same key, or
// Android refuses to update the installed app.
val keyProps = Properties().apply {
    val f = rootProject.file("key.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}
fun signing(name: String, env: String): String? = keyProps.getProperty(name) ?: System.getenv(env)

android {
    namespace = "dev.rdesk.rdesk"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "dev.rdesk.rdesk"
        // Android 8 (the Rust side and the Keystore APIs it relies on).
        minSdk = maxOf(flutter.minSdkVersion, 26)
        targetSdk = flutter.targetSdkVersion
        // Uses the version code from pubspec.yaml. When using split APKs, 1000 * ABI_VERSION
        // is added automatically by Flutter. (https://developer.android.com/studio/build/configure-apk-splits#configure-APK-versions)
        // You can force using the value of versionCode by specifying the `-P force-version-code-ignoring-abi=true`
        // flag during build.
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    signingConfigs {
        val store = signing("storeFile", "RDESK_ANDROID_KEYSTORE")
        if (store != null) {
            create("release") {
                storeFile = file(store)
                storePassword = signing("storePassword", "RDESK_ANDROID_KEYSTORE_PASSWORD")
                keyAlias = signing("keyAlias", "RDESK_ANDROID_KEY_ALIAS")
                keyPassword = signing("keyPassword", "RDESK_ANDROID_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            // Without the release key (a build on your own computer): the debug key, so
            // `flutter run --release` still works. Such an APK cannot update a released one.
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}

dependencies {
    // The device key's fingerprint / face prompt.
    implementation("androidx.biometric:biometric:1.1.0")
}
