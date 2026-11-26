package dev.h4rn.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/**
 * Receives Connect's FCM data messages and shows one alert (ADR-051, ADR-053).
 *
 * A data message is used rather than a notification payload so the app can
 * decide about the foreground: the thread already on screen stays quiet, and
 * every other thread still alerts. The local switch is checked here too, from
 * a flag the plugin persists, so a phone whose removal from Connect failed
 * stays silent while its switch reads Off. Tapping opens
 * `h4rn://thread/<id>` through the deep-link plugin, which the shared React app
 * already follows.
 *
 * A turn that is still running (working, waiting for approval, asking a
 * question) becomes an ongoing Live Update when the separate switch is on.
 * Its notification id is the thread's own, so the turn's finish, failure or
 * limit replaces it with the normal dismissible alert instead of stacking a
 * second notification. Only the working state is silent; a raised hand still
 * gets the person's attention.
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
    // A process started by a message has no UI to ask, so no explicit Off is
    // an on switch: only a stored Off silences delivery here.
    val preferences = getSharedPreferences(PushPlugin.PREFERENCES, Context.MODE_PRIVATE)
    val enabled = preferences.getBoolean(PushPlugin.ENABLED_KEY, true)
    val content = PushContent.from(data)
    if (PushContent.shouldDrop(data, enabled, H4rnPushState.foreground, H4rnPushState.threadId)) {
      // Opening the thread already on screen removes its ongoing entry; a
      // data message for it must not bring the entry back.
      if (H4rnPushState.foreground && content.threadId != null &&
        content.threadId == H4rnPushState.threadId
      ) {
        H4rnLiveNotifications.cancel(this, content.threadId)
      }
      return
    }
    ensureChannel()
    val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    val live = content.isLiveUpdate() &&
      preferences.getBoolean(PushPlugin.LIVE_UPDATES_KEY, false)
    // A turn that started is never an alert by itself: with the Live Updates
    // switch off it says nothing, and any leftover entry for it goes away.
    if (content.isLiveOnly() && !live) {
      content.threadId?.let { H4rnLiveNotifications.cancel(this, it) }
      return
    }
    if (live) {
      val threadId = content.threadId ?: return
      H4rnLiveNotifications.track(this, threadId)
      manager.notify(
        PushContent.notificationId(threadId, 0),
        buildNotification(content, live = true),
      )
      return
    }
    // The final alert reuses the thread's id, so it replaces a Live Update
    // that is still showing. Forget it either way: the turn is over.
    content.threadId?.let { H4rnLiveNotifications.forget(this, it) }
    val groupSequence = if (content.threadId == null) {
      val next = preferences.getInt(PushPlugin.GROUP_SEQUENCE_KEY, 0) + 1
      preferences.edit().putInt(PushPlugin.GROUP_SEQUENCE_KEY, next).apply()
      next
    } else {
      0
    }
    manager.notify(
      PushContent.notificationId(content.threadId, groupSequence),
      buildNotification(content, live = false),
    )
  }

  private fun ensureChannel() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    if (manager.getNotificationChannel(PushContent.CHANNEL_ID) != null) return
    manager.createNotificationChannel(
      NotificationChannel(
        PushContent.CHANNEL_ID,
        PushContent.CHANNEL_NAME,
        NotificationManager.IMPORTANCE_HIGH,
      ),
    )
  }

  private fun buildNotification(content: PushContent, live: Boolean): Notification {
    val text = if (live) content.liveBody() else content.body
    val builder = NotificationCompat.Builder(this, PushContent.CHANNEL_ID)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(content.title)
      .setContentText(text)
      .setPriority(NotificationCompat.PRIORITY_HIGH)
      .setContentIntent(intentFor(content.threadId))
    if (!live) {
      return builder
        .setStyle(NotificationCompat.BigTextStyle().bigText(text))
        .setAutoCancel(true)
        .build()
    }
    // A turn starting is news on the lock screen, not a reason to buzz; an
    // approval or a question still alerts.
    val silent = content.state == PushContent.STATE_WORKING
    builder
      .setStyle(NotificationCompat.BigTextStyle().bigText(text))
      .setOngoing(true)
      .setAutoCancel(false)
      .setOnlyAlertOnce(silent)
      .setSilent(silent)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.BAKLAVA &&
      NotificationManagerCompat.from(this).canPostPromotedNotifications()
    ) {
      // Android 16's Live Update surface: a promoted ongoing notification
      // with an indeterminate progress style. It needs the
      // POST_PROMOTED_NOTIFICATIONS permission, and the person can turn
      // promotion off for h4rn in system settings; either way the same
      // ongoing notification still shows, only without promotion, as it does
      // on older releases.
      builder
        .setRequestPromotedOngoing(true)
        .setStyle(
          NotificationCompat.ProgressStyle()
            .setStyledByProgress(false)
            .setProgressIndeterminate(true),
        )
    }
    return builder.build()
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
