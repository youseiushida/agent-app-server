package dev.aas.android

/**
 * The installed app's version: `versionName` and `versionCode` come from the git commit the APK
 * was built from (app/build.gradle.kts, docs/android.md 19.3), so a report from a phone names
 * the exact commit.
 */
data class AppVersion(val name: String, val code: Int, val buildType: String) {
    /** One line for logs and reports: `0.1.0+1a2b3c4 (42, debug)`. */
    override fun toString(): String = "$name ($code, $buildType)"

    companion object {
        val Current = AppVersion(BuildConfig.VERSION_NAME, BuildConfig.VERSION_CODE, BuildConfig.BUILD_TYPE)
    }
}
