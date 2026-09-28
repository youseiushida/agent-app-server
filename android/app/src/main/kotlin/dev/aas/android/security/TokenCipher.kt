package dev.aas.android.security

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.IOException
import java.nio.ByteBuffer
import java.security.GeneralSecurityException
import java.security.KeyStore
import java.security.ProviderException
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Source of the AES key that protects the device token. Behind an interface so tests can use a
 * software key ([TokenCipher] is plain JCA and runs on the JVM).
 */
interface SecretKeyProvider {
    /** Returns the key, creating it on first use. */
    fun getOrCreateKey(): SecretKey

    /** Deletes the key (unpairing): anything encrypted with it becomes unreadable. */
    fun deleteKey()
}

/**
 * A 256-bit AES-GCM key in the Android Keystore. The key material never leaves the keystore
 * (hardware-backed where the device has it); the app only holds a handle. No user
 * authentication is required: the service must decrypt the token after a reboot without the
 * user unlocking anything in the app.
 */
class AndroidKeystoreKeyProvider(private val alias: String = DEFAULT_ALIAS) : SecretKeyProvider {
    override fun getOrCreateKey(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (keyStore.getKey(alias, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(KEY_BITS)
                // The keystore picks a fresh IV for every encryption (never reused with GCM).
                .setRandomizedEncryptionRequired(true)
                .build(),
        )
        return generator.generateKey()
    }

    override fun deleteKey() {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        if (keyStore.containsAlias(alias)) keyStore.deleteEntry(alias)
    }

    companion object {
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val DEFAULT_ALIAS = "aas-device-token-v1"
        private const val KEY_BITS = 256
    }
}

/** Why a stored token could not be decrypted. */
class TokenCipherException(val reason: Reason, message: String, cause: Throwable? = null) : Exception(message, cause) {
    enum class Reason {
        /** The blob is not in the format this class writes (truncated, other version). */
        Corrupt,

        /**
         * The GCM tag did not verify: the blob was changed, or the key is not the one it was
         * encrypted with (e.g. app data restored onto another device, where the keystore key
         * does not exist).
         */
        AuthenticationFailed,

        /** The keystore could not provide the key or run the cipher. */
        KeystoreFailure,
    }
}

/**
 * Encrypts the device token with AES-256-GCM.
 *
 * Blob format (version 1): `[1][ivLength][iv][ciphertext || 128-bit tag]`. The associated data
 * binds the blob to its purpose, so a blob of another purpose encrypted with the same key would
 * not verify.
 */
class TokenCipher(private val keys: SecretKeyProvider) {
    /** Encrypts [plaintext]; throws [TokenCipherException] (`KeystoreFailure`) on keystore errors. */
    fun encrypt(plaintext: String): ByteArray {
        try {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, keys.getOrCreateKey())
            cipher.updateAAD(AAD)
            val iv = cipher.iv
            val sealed = cipher.doFinal(plaintext.toByteArray(Charsets.UTF_8))
            return ByteBuffer.allocate(2 + iv.size + sealed.size)
                .put(VERSION)
                .put(iv.size.toByte())
                .put(iv)
                .put(sealed)
                .array()
        } catch (e: GeneralSecurityException) {
            throw keystoreFailure("encrypting", e)
        } catch (e: IOException) {
            throw keystoreFailure("encrypting", e)
        } catch (e: ProviderException) {
            // The keystore reports hardware and service failures as ProviderException.
            throw keystoreFailure("encrypting", e)
        }
    }

    /** Decrypts a blob made by [encrypt]. */
    fun decrypt(blob: ByteArray): String {
        if (blob.size < 2 || blob[0] != VERSION) {
            throw TokenCipherException(TokenCipherException.Reason.Corrupt, "unknown token blob format")
        }
        val ivLength = blob[1].toInt() and 0xff
        if (ivLength !in MIN_IV_BYTES..MAX_IV_BYTES || blob.size < 2 + ivLength + TAG_BYTES) {
            throw TokenCipherException(TokenCipherException.Reason.Corrupt, "truncated token blob")
        }
        val iv = blob.copyOfRange(2, 2 + ivLength)
        val sealed = blob.copyOfRange(2 + ivLength, blob.size)
        try {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.DECRYPT_MODE, keys.getOrCreateKey(), GCMParameterSpec(TAG_BYTES * 8, iv))
            cipher.updateAAD(AAD)
            return cipher.doFinal(sealed).toString(Charsets.UTF_8)
        } catch (e: AEADBadTagException) {
            throw TokenCipherException(TokenCipherException.Reason.AuthenticationFailed, "the device token does not verify with this device's key", e)
        } catch (e: GeneralSecurityException) {
            throw keystoreFailure("decrypting", e)
        } catch (e: IOException) {
            throw keystoreFailure("decrypting", e)
        } catch (e: ProviderException) {
            throw keystoreFailure("decrypting", e)
        }
    }

    private fun keystoreFailure(what: String, e: Exception) =
        TokenCipherException(TokenCipherException.Reason.KeystoreFailure, "$what the device token failed: ${e.message ?: e.javaClass.simpleName}", e)

    private companion object {
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val VERSION: Byte = 1
        const val TAG_BYTES = 16

        /** GCM IVs the keystore produces are 12 bytes; anything outside this range is not ours. */
        const val MIN_IV_BYTES = 12
        const val MAX_IV_BYTES = 16
        val AAD = "dev.aas.android/device-token/v1".toByteArray(Charsets.UTF_8)
    }
}
