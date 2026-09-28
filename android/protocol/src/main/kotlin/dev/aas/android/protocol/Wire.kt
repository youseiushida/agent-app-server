package dev.aas.android.protocol

import kotlinx.serialization.KSerializer
import kotlinx.serialization.descriptors.PrimitiveKind
import kotlinx.serialization.descriptors.PrimitiveSerialDescriptor
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.descriptors.buildClassSerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonEncoder
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive

/**
 * The JSON configuration of the wire protocol.
 *
 * * Unknown fields are ignored (the server may add fields within protocol v1).
 * * Absent optional fields are `null` and are not written back (`explicitNulls = false`).
 * * Required fields with a Kotlin default are still written (`encodeDefaults = true`).
 */
val AasJson: Json = Json {
    ignoreUnknownKeys = true
    explicitNulls = false
    encodeDefaults = true
    coerceInputValues = false
}

/** An enum whose wire representation is a camelCase string. */
interface WireEnum {
    val wire: String
}

/**
 * Serializer for [WireEnum]s that decodes values it does not know to [unknown] instead of
 * failing — the server may add enum values within protocol v1.
 */
open class WireEnumSerializer<E>(
    name: String,
    private val values: List<E>,
    private val unknown: E,
) : KSerializer<E> where E : Enum<E>, E : WireEnum {
    override val descriptor: SerialDescriptor = PrimitiveSerialDescriptor("dev.aas.android.protocol.$name", PrimitiveKind.STRING)

    override fun serialize(encoder: Encoder, value: E) = encoder.encodeString(value.wire)

    override fun deserialize(decoder: Decoder): E {
        val text = decoder.decodeString()
        return values.firstOrNull { it !== unknown && it.wire == text } ?: unknown
    }
}

/**
 * Serializer for a union tagged by a discriminator field inside the object
 * (`{"kind": "...", ...fields}`). Variants are plain classes without the discriminator;
 * unknown tags decode to an explicit fallback that keeps the raw object, so a newer server
 * never breaks an older client.
 */
abstract class TaggedUnionSerializer<T : Any>(
    serialName: String,
    private val discriminator: String,
) : KSerializer<T> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("dev.aas.android.protocol.$serialName")

    /** Tag of a known variant, or `null` for the fallback. */
    protected abstract fun tagOf(value: T): String?

    /** Serializer of the variant with [tag], or `null` when the tag is unknown. */
    protected abstract fun serializerFor(tag: String): KSerializer<out T>?

    /** The fallback value for an unknown tag. */
    protected abstract fun unknown(tag: String, raw: JsonObject): T

    /** Raw object of a fallback value (written back verbatim). */
    protected abstract fun rawOf(value: T): JsonObject?

    @Suppress("UNCHECKED_CAST")
    override fun serialize(encoder: Encoder, value: T) {
        val json = encoder as? JsonEncoder ?: error("$discriminator-tagged unions are JSON-only")
        val raw = rawOf(value)
        if (raw != null) {
            json.encodeJsonElement(raw)
            return
        }
        val tag = tagOf(value) ?: error("variant without tag: $value")
        val serializer = serializerFor(tag) as KSerializer<T>? ?: error("no serializer for tag $tag")
        val fields = json.json.encodeToJsonElement(serializer, value).jsonObject
        json.encodeJsonElement(JsonObject(fields + (discriminator to JsonPrimitive(tag))))
    }

    override fun deserialize(decoder: Decoder): T {
        val json = decoder as? JsonDecoder ?: error("$discriminator-tagged unions are JSON-only")
        val obj = json.decodeJsonElement().jsonObject
        val tag = obj[discriminator]?.let { (it as? JsonPrimitive)?.contentOrNull }
        val serializer = tag?.let { serializerFor(it) } ?: return unknown(tag.orEmpty(), obj)
        return json.json.decodeFromJsonElement(serializer, JsonObject(obj - discriminator))
    }
}

/** Reads a string field of a raw object (used by fallback variants). */
internal fun JsonObject.str(key: String): String = (this[key] as? JsonPrimitive)?.contentOrNull.orEmpty()

internal fun JsonObject.strOrNull(key: String): String? = (this[key] as? JsonPrimitive)?.contentOrNull

internal fun JsonObject.long(key: String): Long = (this[key] as? JsonPrimitive)?.contentOrNull?.toLongOrNull() ?: 0L

internal fun JsonObject.longOrNull(key: String): Long? = (this[key] as? JsonPrimitive)?.contentOrNull?.toLongOrNull()

/** Semantic JSON equality helper for tests and diagnostics. */
fun JsonElement.sameJson(other: JsonElement): Boolean = this == other

internal fun JsonElement.primitiveContent(): String? = (this as? JsonPrimitive)?.jsonPrimitive?.contentOrNull
