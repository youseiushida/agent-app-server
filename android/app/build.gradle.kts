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

/**
 * The app's version, from the git commit it is built from (docs/android.md 19.3):
 *
 * * versionCode: the number of commits reachable from HEAD. It grows with every commit on main,
 *   so Android accepts a newer build as an update of an older one.
 * * versionName: [APP_VERSION]`+`<short hash of HEAD>, with `.dirty` when files under android/
 *   differ from that commit (tracked changes or new files), so a build of uncommitted work never
 *   looks like the commit's own build.
 *
 * The same commit (and the same working tree) always gives the same values: nothing depends on
 * the time or the machine. Without the history the values are a clear fallback, never a guess:
 * git missing or not a repository → versionCode [FALLBACK_VERSION_CODE] and `+nogit`; a shallow
 * clone (CI's checkout), whose commit count is not the history's → versionCode
 * [FALLBACK_VERSION_CODE] with the hash. Either is reported as a build warning.
 */
data class AppVersion(val code: Int, val name: String)

/** The version the app's features are at (by hand); the build adds the commit. */
val APP_VERSION = "0.1.0"

/** versionCode when the commit count is unknown (Android needs at least 1). */
val FALLBACK_VERSION_CODE = 1

/**
 * Runs git in the repository; the trimmed output, or null when git fails or cannot run.
 *
 * Every call is read-only and runs with `--no-optional-locks`: this runs whenever Gradle
 * configures :app (every build, every IDE sync, builds running side by side), and a plain
 * `git status` refreshes the index and writes it back under `.git/index.lock`, which makes a
 * `git add` / `git commit` started at the same moment fail with "Unable to create
 * '.git/index.lock': File exists". The option (git 2.15+) skips that optional write; the
 * output is the same.
 */
fun git(vararg args: String): String? = try {
    val result = providers.exec {
        workingDir = rootDir
        commandLine(listOf("git", "--no-optional-locks") + args)
        isIgnoreExitValue = true
    }
    if (result.result.get().exitValue == 0) result.standardOutput.asText.get().trim() else null
} catch (e: Exception) {
    // git is not installed (the process cannot start): the fallback below says so.
    logger.info("git ${args.joinToString(" ")} could not run: ${e.message}")
    null
}

val appVersion: AppVersion = run {
    val hash = git("rev-parse", "--short=7", "HEAD")
    if (hash == null) {
        logger.warn("The git history is not available: building with versionCode $FALLBACK_VERSION_CODE and versionName $APP_VERSION+nogit.")
        return@run AppVersion(FALLBACK_VERSION_CODE, "$APP_VERSION+nogit")
    }
    // Changes under android/ (the app's sources and build scripts), untracked files included.
    // Without optional locks (git()), a stale index is only compared, never written back.
    val status = git("status", "--porcelain", "--untracked-files=normal", "--", ".")
    if (status == null) logger.warn("git status failed: the build is marked .dirty (its tree cannot be shown to be the commit's).")
    val name = "$APP_VERSION+$hash" + if (status == null || status.isNotEmpty()) ".dirty" else ""
    val shallow = git("rev-parse", "--is-shallow-repository") == "true"
    val count = git("rev-list", "--count", "HEAD")?.toIntOrNull()
    if (shallow || count == null) {
        val why = if (shallow) "a shallow clone has only part of the history" else "git rev-list failed"
        logger.warn("The commit count is unknown ($why): building with versionCode $FALLBACK_VERSION_CODE and versionName $name.")
        return@run AppVersion(FALLBACK_VERSION_CODE, name)
    }
    AppVersion(count, name)
}

android {
    namespace = "dev.aas.android"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.aas.android"
        minSdk = 29
        targetSdk = 36
        versionCode = appVersion.code
        versionName = appVersion.name
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
