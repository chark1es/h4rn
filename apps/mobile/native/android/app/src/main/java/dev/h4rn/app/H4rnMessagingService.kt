package dev.h4rn.app

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import androidx.core.app.NotificationCompat
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/**
 * Receives Connect's FCM data messages and shows one alert (ADR-050).
 *
 * A data message is used rather than a notification payload so the app can
 * decide about the foreground: the thread already on screen stays quiet, and
 * every other thread still alerts. Tapping opens `h4rn://thread/<id>` through
 * the deep-link plugin, which the shared React app already follows.
 */
class H4rnMessagingService : FirebaseMessagingService() {
  override fun onNewToken(token: String) {
    getSharedPreferences(PushPlugin.PREFERENCES, Context.MODE_PRIVATE)
      .edit()
      .putString(PushPlugin.TOKEN_KEY, token)
      .apply()
  }

  override fun onMessageReceived(message: RemoteMessage) {
    val data = message.data
    if (PushContent.isSuppressed(data, H4rnPushState.foreground, H4rnPushState.threadId)) {
      return
    }
    val content = PushContent.from(data)
    val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      if (manager.getNotificationChannel(PushContent.CHANNEL_ID) == null) {
        manager.createNotificationChannel(
          NotificationChannel(
            PushContent.CHANNEL_ID,
            PushContent.CHANNEL_NAME,
            NotificationManager.IMPORTANCE_HIGH,
          ),
        )
      }
    }
    val notification = NotificationCompat.Builder(this, PushContent.CHANNEL_ID)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(content.title)
      .setContentText(content.body)
      .setStyle(NotificationCompat.BigTextStyle().bigText(content.body))
      .setPriority(NotificationCompat.PRIORITY_HIGH)
      .setAutoCancel(true)
      .setContentIntent(intentFor(content.threadId))
      .build()
    manager.notify((content.threadId ?: content.title).hashCode(), notification)
  }

  private fun intentFor(threadId: String?): PendingIntent {
    val intent = if (threadId == null) {
      Intent(this, MainActivity::class.java)
    } else {
      Intent(Intent.ACTION_VIEW, Uri.parse(PushContent.deepLink(threadId)), this, MainActivity::class.java)
    }
    intent.addFlags(
      Intent.FLAG_ACTIVITY_NEW_TASK or
        Intent.FLAG_ACTIVITY_CLEAR_TOP or
        Intent.FLAG_ACTIVITY_SINGLE_TOP,
    )
    return PendingIntent.getActivity(
      this,
      threadId?.hashCode() ?: 0,
      intent,
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
  }
}
