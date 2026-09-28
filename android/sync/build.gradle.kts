import org.gradle.api.tasks.testing.logging.TestExceptionFormat
import org.gradle.api.tasks.testing.logging.TestLogEvent

plugins {
    alias(libs.plugins.kotlin.jvm)
    alias(libs.plugins.kotlin.serialization)
    // Test helpers other modules reuse (`testImplementation(testFixtures(project(":sync")))`):
    // the scripted FakeServer, the aas-test-server driver and the SyncStore contract tests.
    `java-test-fixtures`
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    api(project(":protocol"))
    api(libs.kotlinx.coroutines.core)
    api(libs.okhttp)

    testFixturesApi(libs.okhttp.mockwebserver)
    testFixturesApi(libs.kotlin.test)
    testFixturesApi(libs.junit)

    testImplementation(libs.kotlin.test)
    testImplementation(libs.junit)
    testImplementation(libs.okhttp.mockwebserver)
}

// Integration tests against the real daemon (RealServerTest) run when AAS_TEST_SERVER names the
// aas-test-server executable (with aas-dummy-agent next to it); otherwise they are skipped.
val aasTestServer: String? = providers.environmentVariable("AAS_TEST_SERVER").orNull?.takeIf { it.isNotBlank() }

// Engine tests feed the protocol's golden fixtures (e.g. notifications/stream_batch_empty.json) to the engine.
val fixturesDir = rootProject.layout.projectDirectory.dir("../fixtures/protocol")

tasks.test {
    // The engine tests use real loopback sockets and the integration tests one server process
    // each: one fork keeps them from competing for ports and CPU.
    maxParallelForks = 1
    inputs.dir(fixturesDir).withPropertyName("fixtures").withPathSensitivity(PathSensitivity.RELATIVE)
    systemProperty("aas.fixtures", fixturesDir.asFile.absolutePath)
    inputs.property("aasTestServer", aasTestServer ?: "")
    if (aasTestServer != null) {
        environment("AAS_TEST_SERVER", aasTestServer)
        val exe = file(aasTestServer)
        // Rebuilt server binaries must rerun the tests even when no Kotlin source changed.
        inputs.files(listOf(exe, exe.resolveSibling("aas-dummy-agent.exe")).filter { it.isFile })
            .withPropertyName("aasTestServerBinaries")
            .withPathSensitivity(PathSensitivity.NONE)
    }
    testLogging {
        events(TestLogEvent.FAILED, TestLogEvent.SKIPPED)
        exceptionFormat = TestExceptionFormat.FULL
    }
}
