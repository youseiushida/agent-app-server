// Device tests of the app (docs/android.md 23.1): a separate test module that instruments itself
// and drives the app from outside, like a user, with UI Automator, intents and shell commands.
//
// Why not the app's own androidTest: those tests run inside the app's process, so a test cannot
// clear, force-stop or kill the app (process death, app restarts, a fresh install per test) without
// ending itself, and on the R8-processed staging build the test APK would have to link against
// renamed and removed classes. A self-instrumenting test APK runs in its own process and needs
// nothing of the app but its package name and its string resources (read by name). AGP only
// packages a self-instrumenting test APK with all of its own dependencies in a com.android.test
// module (for an app's androidTest it leaves out what the app already has, e.g. the Kotlin
// standard library, and the APK cannot start on its own).
plugins {
    // From the Android Gradle plugin the root project puts on the classpath (built-in Kotlin).
    id("com.android.test")
}

/** The app's application id per build type of this module (the target the tests drive). */
val targetApplicationIds = mapOf(
    "debug" to "dev.aas.android",
    "staging" to "dev.aas.android.staging",
)

android {
    namespace = "dev.aas.android.e2e"
    compileSdk = 36
    targetProjectPath = ":app"

    defaultConfig {
        minSdk = 29
        targetSdk = 36
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        debug {
            buildConfigField("String", "APP_PACKAGE", "\"${targetApplicationIds.getValue("debug")}\"")
        }
        // Tests the app's staging build: release's R8 and resource shrinking, the debug key.
        create("staging") {
            initWith(getByName("debug"))
            buildConfigField("String", "APP_PACKAGE", "\"${targetApplicationIds.getValue("staging")}\"")
        }
    }

    buildFeatures {
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    testOptions {
        // Each test in its own instrumentation run: a test that hangs or crashes the test process
        // does not take the rest of the suite with it.
        execution = "ANDROIDX_TEST_ORCHESTRATOR"
    }

    // The test APK instruments its own package and keeps its own dependencies.
    experimentalProperties["android.experimental.self-instrumenting"] = true
}

androidComponents {
    beforeVariants { variant ->
        // The release app is signed with the user's key and allows no cleartext: the suite runs
        // against debug and staging (docs/android.md 19.2).
        variant.enable = variant.buildType in targetApplicationIds
    }
}

dependencies {
    implementation(libs.androidx.test.runner)
    implementation(libs.androidx.test.ext.junit.ktx)
    implementation(libs.androidx.test.uiautomator)
    implementation(libs.junit)
    androidTestUtil(libs.androidx.test.orchestrator)
}
