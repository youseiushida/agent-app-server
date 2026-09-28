# R8 rules of the release build (and of staging, which is release signed with the debug key for
# the instrumented tests; docs/android.md 19).
#
# The libraries ship consumer rules for their own reflection (kotlinx.serialization,
# kotlinx.coroutines, Room, DataStore's protobuf-lite, CameraX, lifecycle/startup, OkHttp's
# optional TLS providers). The rules below state what the app itself relies on, so a library
# changing its rules cannot silently break it. `./gradlew :app:assembleRelease` runs R8 over the
# whole app and fails on missing classes; the instrumented tests (src/androidTest) run the
# result on a device.

# ----- kotlinx.serialization: protocol types and typed navigation routes ------------------------
# Serializers are looked up at run time by class: Navigation's `navigate(route)` and saved-state
# restoration go through `route::class.serializer()`, which reflects on the companion's (or the
# object's) `serializer()`. The protocol types (:protocol) are also decoded that way when a
# serializer is resolved from a KType.
-keepclassmembers @kotlinx.serialization.Serializable class dev.aas.android.** {
    static ** Companion;
    public static ** INSTANCE;
    kotlinx.serialization.KSerializer serializer(...);
}
-if @kotlinx.serialization.Serializable class dev.aas.android.**
-keepclassmembers class <1>$Companion {
    kotlinx.serialization.KSerializer serializer(...);
}
# The generated serializers read these at run time (sealed and polymorphic hierarchies, the
# `@Serializable(with = …)` of the protocol's enums and tagged unions).
-keepattributes RuntimeVisibleAnnotations,AnnotationDefault,InnerClasses,EnclosingMethod,Signature

# ----- Room -------------------------------------------------------------------------------------
# Room instantiates the generated database by name (`AasDatabase_Impl`, no-argument constructor).
-keep class dev.aas.android.data.db.AasDatabase_Impl {
    <init>();
}

# ----- settings persisted by enum name (SettingsRepository) --------------------------------------
# DataStore holds `Enum.name` of these; the names must stay what earlier versions wrote (and R8
# must not unbox them into ints).
-keepclassmembers enum dev.aas.android.settings.TurnNotificationMode,
                       dev.aas.android.domain.composer.FollowUpDelivery,
                       dev.aas.android.domain.ProjectSort {
    <fields>;
}

# ----- OkHttp ------------------------------------------------------------------------------------
# OkHttp probes optional TLS providers by class name; they are not part of the app.
-dontwarn org.conscrypt.**
-dontwarn org.bouncycastle.**
-dontwarn org.openjsse.**

# ----- CameraX and ZXing -------------------------------------------------------------------------
# CameraX keeps its device quirks (its own rules). ZXing core (QR decoding in QrCodeAnalyzer) uses
# no reflection: nothing to keep beyond what the code references.
