pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "aas-android"

// Pure Kotlin/JVM modules: build and test without the Android SDK.
include(":protocol")
include(":sync")

// The Android app needs the SDK. An SDK location is known through ANDROID_HOME /
// ANDROID_SDK_ROOT, or sdk.dir in local.properties; `gradlew :protocol:test :sync:test` works
// on a machine without it.
fun sdkLocation(): String? {
    System.getenv("ANDROID_HOME")?.takeIf { it.isNotBlank() && file(it).isDirectory }?.let { return it }
    System.getenv("ANDROID_SDK_ROOT")?.takeIf { it.isNotBlank() && file(it).isDirectory }?.let { return it }
    val props = file("local.properties")
    if (props.isFile) {
        // java.util.Properties is the format AGP and the Kotlin plugin read: backslashes in the
        // path must be escaped (`sdk.dir=C\:\\Users\\me\\AppData\\Local\\Android\\Sdk`).
        val sdkDir = java.util.Properties()
            .apply { props.reader(Charsets.ISO_8859_1).use { load(it) } }
            .getProperty("sdk.dir")
        if (sdkDir != null && file(sdkDir).isDirectory) return sdkDir
    }
    return null
}

// The app module is included only when both the SDK and the module itself exist, so a checkout
// without `android/app` (or a machine without the SDK) still builds and tests the JVM modules.
val appBuildScript = file("app/build.gradle.kts")
when {
    sdkLocation() == null ->
        logger.lifecycle("Android SDK not found: building :protocol and :sync only (see docs/android.md).")
    !appBuildScript.isFile ->
        logger.lifecycle("android/app/build.gradle.kts not found: building :protocol and :sync only.")
    else -> {
        include(":app")
        // The app's device tests (a self-instrumenting test APK; docs/android.md 23.1).
        include(":e2e")
    }
}
