package dev.h4rn.app

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import com.google.firebase.FirebaseApp
import com.google.firebase.messaging.FirebaseMessaging

@InvokeArg
internal class PushForegroundArgs {
  var threadId: String? = null
}

@InvokeArg
internal class PushEnabledArgs {
  var enabled: Boolean = true
}

/**
 * Android push bridge (ADR-051).
 *
 * Firebase is optional at build time: without `google-services.json` the SDK
 * has no app to initialise, [status] reports unavailable, and Settings says
 * why. No command pretends a token or a registration exists.
 */
@TauriPlugin
class PushPlugin(private val activity: android.app.Activity) : Plugin(activity) {
  companion object {
    const val PREFERENCES = "h4rn_push"
    const val TOKEN_KEY = "fcm_token"
    const val ASKED_KEY = "permission_asked"

    /**
     * The person's local switch, persisted for the notification service.
     *
     * The service runs even when the shared UI is not, so it cannot ask React;
     * this is the second line of defence for a relay row that survived a failed
     * removal.
     */
    const val ENABLED_KEY = "alerts_enabled"
  }

  private val preferences by lazy {
    activity.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
  }

  private fun available(): Boolean = FirebaseApp.getApps(activity).isNotEmpty()

  private fun permission(): String {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return "granted"
    val granted = activity.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
      PackageManager.PERMISSION_GRANTED
    if (granted) return "granted"
    return if (preferences.getBoolean(ASKED_KEY, false)) "denied" else "default"
  }

  private fun ensureChannel() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val manager = activity.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    if (manager.getNotificationChannel(PushContent.CHANNEL_ID) != null) return
    val channel = NotificationChannel(
      PushContent.CHANNEL_ID,
      PushContent.CHANNEL_NAME,
      NotificationManager.IMPORTANCE_HIGH,
    ).apply {
      description = "When an agent finishes, fails or needs you"
      setShowBadge(true)
    }
    manager.createNotificationChannel(channel)
  }

  @Command
  fun pushStatus(invoke: Invoke) {
    ensureChannel()
    val result = JSObject()
    result.put("available", available())
    result.put("permission", permission())
    result.put("token", preferences.getString(TOKEN_KEY, null))
    result.put("channel", true)
    invoke.resolve(result)
  }

  @Command
  fun pushRequestPermission(invoke: Invoke) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU || permission() == "granted") {
      invoke.resolve()
      return
    }
    preferences.edit().putBoolean(ASKED_KEY, true).apply()
    H4rnPushPermission.ask(activity, invoke)
  }

  @Command
  fun pushToken(invoke: Invoke) {
    if (!available()) {
      invoke.reject("This build has no Firebase configuration.")
      return
    }
    FirebaseMessaging.getInstance().token.addOnCompleteListener { task ->
      if (!task.isSuccessful) {
        invoke.reject("Could not get the FCM token: ${task.exception?.message}")
        return@addOnCompleteListener
      }
      val token = task.result ?: ""
      preferences.edit().putString(TOKEN_KEY, token).apply()
      val result = JSObject()
      result.put("token", token)
      invoke.resolve(result)
    }
  }

  @Command
  fun pushSetForegroundThread(invoke: Invoke) {
    val args = invoke.parseArgs(PushForegroundArgs::class.java)
    H4rnPushState.threadId = args.threadId?.trim()?.ifEmpty { null }
    invoke.resolve()
  }

  /** Mirrors the shared UI's switch so the service can drop alerts when off. */
  @Command
  fun pushSetEnabled(invoke: Invoke) {
    val args = invoke.parseArgs(PushEnabledArgs::class.java)
    preferences.edit().putBoolean(ENABLED_KEY, args.enabled).apply()
    invoke.resolve()
  }
}
