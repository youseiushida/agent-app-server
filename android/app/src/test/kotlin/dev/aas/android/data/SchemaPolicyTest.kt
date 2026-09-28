package dev.aas.android.data

import dev.aas.android.data.db.AasDatabase
import dev.aas.android.data.db.Migrations
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Test
import java.io.File
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * The migration policy (docs/android.md 15.2) as a test: every schema version is exported and
 * checked in, and every upgrade step has an explicit migration (no destructive fallback).
 */
class SchemaPolicyTest {
    /** Gradle runs unit tests in the module directory (`android/app`). */
    private val schemas = File("schemas/${AasDatabase::class.java.name}")

    @Test
    fun everyVersionHasAnExportedSchema() {
        for (version in 1..AasDatabase.VERSION) {
            val file = File(schemas, "$version.json")
            assertTrue(file.isFile, "missing exported schema ${file.path}: build once and check it in")
            val exported = Json.parseToJsonElement(file.readText()).jsonObject.getValue("database").jsonObject
            assertEquals(version, exported.getValue("version").jsonPrimitive.int)
        }
    }

    @Test
    fun everyUpgradeStepHasAMigration() {
        val steps = Migrations.ALL.map { it.startVersion to it.endVersion }.toSet()
        for (version in 2..AasDatabase.VERSION) {
            assertTrue((version - 1 to version) in steps, "no Migration(${version - 1}, $version) in Migrations.ALL")
        }
        assertTrue(steps.all { (from, to) -> from in 1 until to && to <= AasDatabase.VERSION }, "migrations beyond the current version: $steps")
    }
}
