import org.gradle.api.tasks.testing.logging.TestExceptionFormat
import org.gradle.api.tasks.testing.logging.TestLogEvent

plugins {
    // AGP 9 compiles Kotlin itself (built-in Kotlin); org.jetbrains.kotlin.android is not applied.
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
    alias(libs.plugins.ksp)
    alias(libs.plugins.room)
}

/**
 * The release signing key lives outside the repository (CLAUDE.md): its keystore file, alias and
 * passwords come from Gradle properties (`~/.gradle/gradle.properties`, never the project's own
 * file) or, when a property is not set, from environment variables (CI). docs/android.md 19.
 */
data class ReleaseSigning(val storeFile: String, val storePassword: String, val keyAlias: String, val keyPassword: String)

/** Each part of [ReleaseSigning]: its Gradle property and the environment variable used when the property is not set. */
val releaseSigningKeys = linkedMapOf(
    "aasReleaseStoreFile" to "AAS_RELEASE_STORE_FILE",
    "aasReleaseStorePassword" to "AAS_RELEASE_STORE_PASSWORD",
    "aasReleaseKeyAlias" to "AAS_RELEASE_KEY_ALIAS",
    "aasReleaseKeyPassword" to "AAS_RELEASE_KEY_PASSWORD",
)

fun releaseSigningValue(property: String): String? =
    (providers.gradleProperty(property).orNull ?: providers.environmentVariable(releaseSigningKeys.getValue(property)).orNull)
        ?.takeIf { it.isNotBlank() }

val releaseSigningMissing: List<String> = releaseSigningKeys.keys.filter { releaseSigningValue(it) == null }
val releaseSigning: ReleaseSigning? = if (releaseSigningMissing.isEmpty()) {
    ReleaseSigning(
        storeFile = releaseSigningValue("aasReleaseStoreFile")!!,
        storePassword = releaseSigningValue("aasReleaseStorePassword")!!,
        keyAlias = releaseSigningValue("aasReleaseKeyAlias")!!,
        keyPassword = releaseSigningValue("aasReleaseKeyPassword")!!,
    )
} else {
    null
}

android {
    namespace = "dev.aas.android"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.aas.android"
        minSdk = 29
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
    }

    signingConfigs {
        releaseSigning?.let { key ->
            create("release") {
                storeFile = file(key.storeFile)
                storePassword = key.storePassword
                keyAlias = key.keyAlias
                keyPassword = key.keyPassword
            }
        }
    }

    buildTypes {
        debug {
            // Debug builds may talk to a daemon on the emulator host (10.0.2.2) or through
            // `adb reverse` (127.0.0.1) without TLS: src/debug/res/xml/network_security_config.xml.
        }
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            // Signed with the key from outside the repository; without it the release tasks fail
            // with the list of what is missing (verifyReleaseSigning below).
            signingConfig = signingConfigs.findByName("release")
        }
        create("staging") {
            // The release build (R8 with the same rules, resource shrinking, not debuggable) for
            // the device tests (the :e2e module, docs/android.md 23.1): they exercise what R8
            // produced. Signed with the debug key so no release key is needed, installed next to
            // a release build (".staging"), and allowed cleartext to loopback only: the tests'
            // local server and the test daemon through `adb reverse`
            // (src/staging/res/xml/network_security_config.xml).
            initWith(getByName("release"))
            signingConfig = signingConfigs.getByName("debug")
            applicationIdSuffix = ".staging"
            versionNameSuffix = "-staging"
            matchingFallbacks += "release"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    testOptions {
        unitTests {
            // Robolectric reads the merged manifest and resources.
            isIncludeAndroidResources = true
            all { test ->
                test.maxHeapSize = "1g"
                // SchemaPolicyTest reads the exported Room schemas.
                test.inputs.dir(layout.projectDirectory.dir("schemas")).withPathSensitivity(PathSensitivity.RELATIVE)
                // Item rendering and view-model tests use the protocol's golden fixtures.
                val fixtures = rootProject.layout.projectDirectory.dir("../fixtures/protocol")
                test.inputs.dir(fixtures).withPropertyName("protocolFixtures").withPathSensitivity(PathSensitivity.RELATIVE)
                test.systemProperty("aas.fixtures", fixtures.asFile.absolutePath)
                test.testLogging {
                    events(TestLogEvent.FAILED, TestLogEvent.SKIPPED)
                    exceptionFormat = TestExceptionFormat.FULL
                }
            }
        }
    }

    lint {
        abortOnError = true
        checkReleaseBuilds = true
        // Version-catalog freshness is checked by hand against Google Maven / Maven Central
        // (docs/android.md); lint's offline guesses would only add noise.
        disable += setOf("GradleDependency", "NewerVersionAvailable", "AndroidGradlePluginVersion")
        // targetSdk 36 is the design's choice (design.md §15), not an oversight.
        disable += "OldTargetApi"
    }

    packaging {
        resources {
            excludes += setOf("/META-INF/{AL2.0,LGPL2.1}", "/META-INF/LICENSE*", "/META-INF/NOTICE*")
        }
    }
}

androidComponents {
    beforeVariants { variant ->
        // The JVM unit tests (Robolectric) run on the debug build only (not on release or staging).
        variant.hostTests[com.android.build.api.variant.HostTestBuilder.UNIT_TEST_TYPE]?.enable = variant.buildType == "debug"
        // Device tests live in the :e2e module (a self-instrumenting test APK, docs/android.md 23.1).
        variant.deviceTests[com.android.build.api.variant.DeviceTestBuilder.ANDROID_TEST_TYPE]?.enable = false
    }
}

room {
    // Exported schemas are the migration baseline (docs/android.md, "Room のスキーマと移行").
    schemaDirectory("$projectDir/schemas")
}

dependencies {
    implementation(project(":sync"))

    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.lifecycle.process)
    implementation(libs.androidx.navigation.compose)
    implementation(libs.androidx.datastore.preferences)

    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.foundation)
    implementation(libs.androidx.compose.material3)
    // Only the core icons; the others the app shows are copied into ui/icons (the extended
    // artifact is every Material icon, 35 MB that a debug build carries unshrunk).
    implementation(libs.androidx.compose.material.icons.core)
    implementation(libs.androidx.compose.ui.tooling.preview)
    debugImplementation(libs.androidx.compose.ui.tooling)

    implementation(libs.androidx.room.runtime)
    implementation(libs.androidx.room.ktx)
    ksp(libs.androidx.room.compiler)

    implementation(libs.androidx.camera.core)
    implementation(libs.androidx.camera.camera2)
    implementation(libs.androidx.camera.lifecycle)
    implementation(libs.androidx.camera.view)
    implementation(libs.zxing.core)

    testImplementation(testFixtures(project(":sync")))
    testImplementation(libs.junit)
    testImplementation(libs.kotlin.test)
    testImplementation(libs.kotlinx.coroutines.test)
    testImplementation(libs.robolectric)
    testImplementation(libs.androidx.test.core.ktx)
    testImplementation(libs.androidx.test.ext.junit.ktx)
    testImplementation(libs.okhttp.mockwebserver)
    testImplementation(platform(libs.androidx.compose.bom))
    testImplementation(libs.androidx.compose.ui.test.junit4)
    // Registers the empty test activity Compose's test rules may launch (debug builds only).
    debugImplementation(libs.androidx.compose.ui.test.manifest)
}

/**
 * Fails every release packaging task with the list of missing signing values instead of
 * producing an unsigned APK that cannot be installed (docs/android.md 19).
 */
val verifyReleaseSigning by tasks.registering {
    // Decided while configuring, so the action captures only this message.
    val problem: String? = when {
        releaseSigningMissing.isNotEmpty() ->
            "The release signing key is not configured. Set these in ~/.gradle/gradle.properties (or as environment variables):\n" +
                releaseSigningMissing.joinToString("\n") { "  $it (or ${releaseSigningKeys.getValue(it)})" } +
                "\nThe keystore must be outside the repository, e.g. %APPDATA%\\agent-app-server\\android\\release.jks."
        !file(releaseSigning!!.storeFile).isFile ->
            "The release keystore ${releaseSigning.storeFile} (aasReleaseStoreFile) does not exist."
        else -> null
    }
    doLast {
        if (problem != null) throw GradleException(problem)
    }
}

tasks.configureEach {
    if (name == "preReleaseBuild") dependsOn(verifyReleaseSigning)
}
