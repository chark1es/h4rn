package dev.h4rn.app

import android.app.Activity
import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@InvokeArg
internal class SecretGetArgs {
  lateinit var key: String
}

@InvokeArg
internal class SecretSetArgs {
  lateinit var key: String
  lateinit var value: String
}

@InvokeArg
internal class SecretDeleteArgs {
  lateinit var key: String
}

/**
 * Small Android Keystore-backed secret store (ADR-016).
 *
 * A single AES-256/GCM master key lives in the Android Keystore (hardware
 * backed where the device offers it) and never leaves it. Each value is
 * encrypted with a fresh IV, bound to its logical key as AAD, and the
 * ciphertext lives in private SharedPreferences. The daemon pairing token is
 * the only caller today; the Rust commands are `secret_get`, `secret_set` and
 * `secret_delete`.
 *
 * This is deliberately not a general purpose credential vault: there is no
 * biometric gate yet (Android Keystore-backed is the Phase 3 requirement) and
 * values are capped by the caller in Rust.
 */
@TauriPlugin
class SecretStorePlugin(private val activity: Activity) : Plugin(activity) {
  private companion object {
    const val ANDROID_KEYSTORE = "AndroidKeyStore"
    const val KEY_ALIAS = "h4rn.secret-store.v1"
    const val PREFERENCES = "h4rn_secrets"
    const val TRANSFORMATION = "AES/GCM/NoPadding"
    const val GCM_TAG_BITS = 128
    const val IV_BYTES = 12
  }

  private val preferences by lazy {
    activity.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
  }

  private val keyStore by lazy {
    KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
  }

  private fun masterKey(): SecretKey {
    val existing = keyStore.getKey(KEY_ALIAS, null) as? SecretKey
    if (existing != null) return existing

    val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
    generator.init(
      KeyGenParameterSpec.Builder(
        KEY_ALIAS,
        KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
      )
        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
        .setKeySize(256)
        .setUserAuthenticationRequired(false)
        .build(),
    )
    return generator.generateKey()
  }

  @Command
  fun secretGet(invoke: Invoke) {
    val args = invoke.parseArgs(SecretGetArgs::class.java)
    val stored = preferences.getString(args.key, null)
    if (stored == null) {
      val result = JSObject()
      result.put("value", null)
      invoke.resolve(result)
      return
    }

    try {
      val parts = stored.split(":", limit = 2)
      require(parts.size == 2) { "stored secret is malformed" }
      val iv = Base64.decode(parts[0], Base64.NO_WRAP)
      val ciphertext = Base64.decode(parts[1], Base64.NO_WRAP)
      val cipher = Cipher.getInstance(TRANSFORMATION)
      cipher.init(
        Cipher.DECRYPT_MODE,
        masterKey(),
        GCMParameterSpec(GCM_TAG_BITS, iv),
      )
      cipher.updateAAD(args.key.toByteArray(Charsets.UTF_8))
      val clear = cipher.doFinal(ciphertext).toString(Charsets.UTF_8)
      val result = JSObject()
      result.put("value", clear)
      invoke.resolve(result)
    } catch (error: Exception) {
      // A key invalidated by a lock-screen change cannot be recovered. Drop the
      // unreadable entry so the UI can pair again instead of failing forever.
      preferences.edit().remove(args.key).apply()
      invoke.reject("could not read the stored secret: ${error.message}")
    }
  }

  @Command
  fun secretSet(invoke: Invoke) {
    val args = invoke.parseArgs(SecretSetArgs::class.java)
    try {
      val cipher = Cipher.getInstance(TRANSFORMATION)
      cipher.init(Cipher.ENCRYPT_MODE, masterKey())
      cipher.updateAAD(args.key.toByteArray(Charsets.UTF_8))
      val ciphertext = cipher.doFinal(args.value.toByteArray(Charsets.UTF_8))
      val encoded =
        Base64.encodeToString(cipher.iv, Base64.NO_WRAP) +
          ":" +
          Base64.encodeToString(ciphertext, Base64.NO_WRAP)
      preferences.edit().putString(args.key, encoded).apply()
      invoke.resolve()
    } catch (error: Exception) {
      invoke.reject("could not store the secret: ${error.message}")
    }
  }

  @Command
  fun secretDelete(invoke: Invoke) {
    val args = invoke.parseArgs(SecretDeleteArgs::class.java)
    preferences.edit().remove(args.key).apply()
    invoke.resolve()
  }
}
