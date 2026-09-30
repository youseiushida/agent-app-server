package dev.aas.android.protocol

import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.descriptors.PolymorphicKind
import kotlinx.serialization.descriptors.PrimitiveKind
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.descriptors.StructureKind
import kotlinx.serialization.serializerOrNull
import java.io.File
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue
import kotlin.test.fail

/**
 * [StoredModels.VERSION] follows the shape of [StoredModels.TYPES] (docs/android.md 15.2).
 *
 * The shape is written out as text: every class reachable from the stored types with its fields
 * and their types, every union with its variants, every enum with its wire values. The text of
 * each version is kept in `android/protocol/stored-models/<version>.txt`; the current classes
 * must have exactly the shape recorded for [StoredModels.VERSION]. A change of the models
 * therefore fails here until the version is raised and the new shape recorded
 * (`AAS_UPDATE_STORED_MODELS=1 ./gradlew :protocol:test`), and the app then reads everything
 * again once after the update instead of trusting what an older build stored.
 */
class StoredModelsTest {
    @Test
    fun theStoredModelsHaveTheShapeRecordedForTheirVersion() {
        val shape = StoredModelShape.of(StoredModels.TYPES.map { it.descriptor })
        val file = File(dir, "${StoredModels.VERSION}.txt")
        val update = System.getenv(UPDATE_ENV) == "1"
        if (!file.isFile) {
            if (!update) fail("no recorded shape for StoredModels.VERSION ${StoredModels.VERSION}: record it with $UPDATE_ENV=1 (${file.path})")
            file.writeText(header(StoredModels.VERSION) + shape)
            return
        }
        val recorded = file.readText().lineSequence().filterNot { it.startsWith("#") }.joinToString("\n")
        assertEquals(
            recorded,
            shape,
            "the stored models changed since StoredModels.VERSION ${StoredModels.VERSION} was recorded: raise the version " +
                "and record the new shape with $UPDATE_ENV=1 (a recorded version is never rewritten)",
        )
    }

    @Test
    fun everyVersionUpToTheCurrentOneIsRecordedAndDiffersFromTheOneBefore() {
        val files = dir.listFiles().orEmpty().filter { it.isFile }
        assertTrue(files.all { it.name.matches(Regex("[1-9][0-9]*\\.txt")) }, "unexpected files in $dir: ${files.map { it.name }}")
        val versions = files.map { it.nameWithoutExtension.toInt() }.sorted()
        assertEquals((1..StoredModels.VERSION).toList(), versions, "one recorded shape per version, none beyond the current one")
        val shapes = versions.map { v -> File(dir, "$v.txt").readText().lineSequence().filterNot { it.startsWith("#") }.joinToString("\n") }
        shapes.zipWithNext().forEachIndexed { i, (before, after) ->
            assertTrue(before != after, "version ${versions[i + 1]} records the same shape as version ${versions[i]}")
        }
    }

    /** The walk finds what the models contain: fields, nested classes, union variants and enum values. */
    @Test
    fun theShapeNamesFieldsUnionVariantsAndEnumValues() {
        val shape = StoredModelShape.of(StoredModels.TYPES.map { it.descriptor })
        assertTrue("  features: dev.aas.android.protocol.HarnessFeatures" in shape, shape)
        assertTrue("  permissionModes: List<kotlin.String>?" in shape, "Model.permissionModes")
        assertTrue("  output: kotlin.String?" in shape, "BackgroundTask.output")
        assertTrue(Regex("union dev\\.aas\\.android\\.protocol\\.Item = .*dev\\.aas\\.android\\.protocol\\.Item\\.ProposedPlan").containsMatchIn(shape), "Item's variants")
        assertTrue(Regex("enum dev\\.aas\\.android\\.protocol\\.BackgroundTaskStatus = .*lost").containsMatchIn(shape), "enum values")
        assertTrue("class dev.aas.android.protocol.PlanModeFeature" in shape, "classes reached through fields")
    }

    private fun header(version: Int) =
        "# The shape of StoredModels.TYPES at StoredModels.VERSION $version (docs/android.md 15.2).\n" +
            "# Written by StoredModelsTest with $UPDATE_ENV=1; never edited, never rewritten.\n"

    private companion object {
        const val UPDATE_ENV = "AAS_UPDATE_STORED_MODELS"

        val dir: File = File(System.getProperty("aas.storedModels") ?: error("system property aas.storedModels is not set (run through Gradle)"))
    }
}

/**
 * The shape of serializable types as text, one block per named type, sorted by name. Only what
 * decides what a stored copy keeps: field names and their types (with nullability), union
 * variants (the protocol's tagged unions: `TaggedUnionSerializer`, whose descriptors do not list
 * them, found as the union's nested serializable classes) and enum values (`WireEnum`, by wire
 * name). Optionality and defaults are left out: they do not change what is stored.
 */
internal object StoredModelShape {
    private const val PACKAGE = "dev.aas.android.protocol."

    fun of(roots: List<SerialDescriptor>): String {
        val blocks = sortedMapOf<String, String>()
        val pending = ArrayDeque(roots)
        while (pending.isNotEmpty()) {
            val d = pending.removeFirst()
            val name = nameOf(d)
            if (name in blocks || isLeaf(d)) continue
            when (d.kind) {
                // Collections are named by their element types (typeOf); only those are walked.
                StructureKind.LIST, StructureKind.MAP -> (0 until d.elementsCount).forEach { pending += d.getElementDescriptor(it) }
                PrimitiveKind.STRING -> blocks[name] = "enum $name = " + enumValues(name).joinToString(" | ")
                StructureKind.OBJECT -> blocks[name] = "object $name"
                StructureKind.CLASS -> if (d.elementsCount == 0 && isUnion(name)) {
                    val variants = unionVariants(name)
                    blocks[name] = "union $name = " + variants.map { nameOf(it) }.sorted().joinToString(", ")
                    pending += variants
                } else {
                    blocks[name] = buildString {
                        append("class ").append(name)
                        for (i in 0 until d.elementsCount) {
                            val element = d.getElementDescriptor(i)
                            append("\n  ").append(d.getElementName(i)).append(": ").append(typeOf(element))
                            pending += element
                        }
                    }
                }
                else -> error("unexpected kind ${d.kind} of $name")
            }
        }
        return blocks.values.joinToString("\n")
    }

    private fun nameOf(d: SerialDescriptor): String = d.serialName.removeSuffix("?")

    private fun typeOf(d: SerialDescriptor): String {
        val base = when (d.kind) {
            StructureKind.LIST -> "List<${typeOf(d.getElementDescriptor(0))}>"
            StructureKind.MAP -> "Map<${typeOf(d.getElementDescriptor(0))}, ${typeOf(d.getElementDescriptor(1))}>"
            else -> nameOf(d)
        }
        return if (d.isNullable) "$base?" else base
    }

    /** Kotlin's primitives and the raw JSON types (kept verbatim, whatever they contain). */
    @OptIn(ExperimentalSerializationApi::class) // PolymorphicKind
    private fun isLeaf(d: SerialDescriptor): Boolean {
        val name = nameOf(d)
        if (name.startsWith("kotlinx.serialization.json.")) return true
        if (d.kind is PolymorphicKind) error("polymorphic $name: stored types use tagged unions")
        return d.kind is PrimitiveKind && !name.startsWith(PACKAGE)
    }

    private fun protocolClass(name: String): Class<*> {
        require(name.startsWith(PACKAGE)) { "$name is not a protocol type" }
        return try {
            Class.forName(name)
        } catch (e: ClassNotFoundException) {
            throw AssertionError("the protocol type $name is not a top-level class of that name", e)
        }
    }

    private fun isUnion(name: String): Boolean = name.startsWith(PACKAGE) && protocolClass(name).isInterface

    /** The wire values of the `WireEnum` [name], in declaration order. */
    private fun enumValues(name: String): List<String> {
        val cls = protocolClass(name)
        val constants = cls.enumConstants ?: throw AssertionError("$name is a string but not an enum")
        return constants.map { (it as? WireEnum)?.wire ?: throw AssertionError("$name is not a WireEnum") }
    }

    /**
     * The variants of the tagged union [name]: its nested classes that implement it and are
     * serializable. The only other implementation allowed is the fallback `Unknown`, which keeps
     * the raw object and writes it back verbatim.
     */
    private fun unionVariants(name: String): List<SerialDescriptor> {
        val union = protocolClass(name)
        val implementations = union.declaredClasses.filter { union.isAssignableFrom(it) && it != union }
        val variants = implementations.mapNotNull { cls ->
            val serializer = serializerOrNull(cls)
            if (serializer == null && cls.simpleName != "Unknown") throw AssertionError("${cls.name} implements $name but is not serializable")
            serializer?.descriptor
        }
        if (variants.isEmpty()) throw AssertionError("no variants of the union $name")
        return variants
    }
}
