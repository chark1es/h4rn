package dev.h4rn.app

import android.app.NotificationManager
import android.content.Context

/** Whether the exact thread is on screen in the foreground. */
object H4rnPushState {
  @Volatile var foreground: Boolean = false
  @Volatile var threadId: String? = null
}

/**
 * The ongoing Live Update notifications currently in the tray (ADR-053).
 *
 * The ids are persisted because the notification service can outlive the
 * WebView: sign-out, the live-updates switch and the shared UI's list of
 * running threads all have to cancel what is already showing, even after a
 * cold start. Only per-thread ids are tracked; a grouped alert never is one.
 */
object H4rnLiveNotifications {
  private fun ids(context: Context): MutableSet<String> =
    context
      .getSharedPreferences(PushPlugin.PREFERENCES, Context.MODE_PRIVATE)
      .getStringSet(PushPlugin.LIVE_THREADS_KEY, emptySet())
      ?.toMutableSet()
      ?: mutableSetOf()

  private fun manager(context: Context): NotificationManager =
    context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

  /** Remembers that this thread has a Live Update showing. */
  fun track(context: Context, threadId: String) {
    val ids = ids(context)
    if (!ids.add(threadId)) return
    context
      .getSharedPreferences(PushPlugin.PREFERENCES, Context.MODE_PRIVATE)
      .edit()
      .putStringSet(PushPlugin.LIVE_THREADS_KEY, ids)
      .apply()
  }

  /** Forgets a thread's Live Update, whether or not it was showing. */
  fun forget(context: Context, threadId: String) {
    val ids = ids(context)
    if (!ids.remove(threadId)) return
    context
      .getSharedPreferences(PushPlugin.PREFERENCES, Context.MODE_PRIVATE)
      .edit()
      .putStringSet(PushPlugin.LIVE_THREADS_KEY, ids)
      .apply()
  }

  /** Cancels one thread's Live Update and forgets it. */
  fun cancel(context: Context, threadId: String) {
    manager(context).cancel(PushContent.notificationId(threadId, 0))
    forget(context, threadId)
  }

  /**
   * Cancels every tracked Live Update whose thread is not [running].
   *
   * [running] is the shared UI's list of top-level threads with a turn in
   * progress; an entry for any other thread outlived its turn.
   */
  fun retainOnly(context: Context, running: Set<String>) {
    for (threadId in PushContent.staleLiveThreads(ids(context), running)) {
      cancel(context, threadId)
    }
  }

  /** Cancels every Live Update this app posted. */
  fun cancelAll(context: Context) {
    val manager = manager(context)
    for (threadId in ids(context)) {
      manager.cancel(PushContent.notificationId(threadId, 0))
    }
    context
      .getSharedPreferences(PushPlugin.PREFERENCES, Context.MODE_PRIVATE)
      .edit()
      .remove(PushPlugin.LIVE_THREADS_KEY)
      .apply()
  }
}

/**
 * Pending notification permission request.
 *
 * A Tauri command must answer only after the system dialog is gone, so the
 * shared object hands the result back from the activity's callback instead of
 * reporting a permission the person has not decided yet.
 */
object H4rnPushPermission {
  const val REQUEST_CODE = 4821
  private var pending: app.tauri.plugin.Invoke? = null

  fun ask(activity: android.app.Activity, invoke: app.tauri.plugin.Invoke) {
    pending?.reject("a notification permission request is already open")
    pending = invoke
    activity.requestPermissions(
      arrayOf(android.Manifest.permission.POST_NOTIFICATIONS),
      REQUEST_CODE,
    )
  }

  fun resolved() {
    val invoke = pending ?: return
    pending = null
    invoke.resolve()
  }
}
