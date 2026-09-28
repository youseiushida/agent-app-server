// Plugins are declared here once (not applied) so all modules share one classloader.
//
// :app uses AGP 9's built-in Kotlin support: the Kotlin Gradle plugin version declared here (via
// kotlin.jvm) is the one AGP compiles with, and `org.jetbrains.kotlin.android` must not be
// applied (it would register a second `kotlin` extension). The Compose compiler, serialization
// and KSP plugins are applied on top of the built-in support.
plugins {
    alias(libs.plugins.kotlin.jvm) apply false
    alias(libs.plugins.kotlin.serialization) apply false
    alias(libs.plugins.kotlin.compose) apply false
    alias(libs.plugins.ksp) apply false
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.room) apply false
}
