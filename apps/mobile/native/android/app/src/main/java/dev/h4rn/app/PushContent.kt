package dev.h4rn.app

/**
 * The fixed alert fields Connect sends in an FCM data message.
 *
 * Android composes the notification itself so the message can be a data
 * message: there is no server-authored notification payload in the tray before
 * the app runs, and a suppressed foreground alert never appears. Only these
 * fields are read; anything else in the map is ignored.
 */
data class PushContent(
  val title: String,
  val body: String,
  val threadId: String?,
) {
  companion object {
    const val CHANNEL_ID = "h4rn_alerts"
    const val CHANNEL_NAME = "Agent alerts"

    /**
     * Group notification ids start here and increase by one per group.
     *
     * Thread ids are hashed, so this only has to be a range groups can use
     * without colliding with each other; a group id is never reused, so a
     * second group adds a notification instead of replacing the first.
     */
    const val GROUP_NOTIFICATION_BASE = 1_000_000

    /** Builds the notification from the data map, with quiet fallbacks. */
    fun from(data: Map<String, String>): PushContent {
      val title = data["title"].orEmpty().trim().ifEmpty { "h4rn" }
      val body = data["body"].orEmpty().trim()
      val thread = data["thread_id"].orEmpty().trim().ifEmpty { null }
      return PushContent(title = title, body = body, threadId = thread)
    }

    /**
     * Whether this alert must be dropped.
     *
     * The local switch off means drop: Connect's row may still exist after a
     * failed removal, so the phone must not alert while its own switch reads
     * Off. A grouped alert (more than one thread) is never dropped for the
     * foreground: naming one thread would lose the other agents in the group.
     * Otherwise only the foreground with that exact thread is quiet. Any other
     * screen, or a backgrounded app, still alerts; suppressing by visibility on
     * some other device would hide work the person is not looking at.
     */
    fun shouldDrop(
      data: Map<String, String>,
      enabled: Boolean,
      foreground: Boolean,
      openThread: String?,
    ): Boolean {
      if (!enabled) return true
      if (!foreground) return false
      val count = data["count"]?.trim()?.toIntOrNull() ?: 1
      if (count > 1) return false
      val thread = data["thread_id"].orEmpty().trim()
      return thread.isNotEmpty() && thread == openThread
    }

    /** The deep link a tap opens. */
    fun deepLink(threadId: String): String = "h4rn://thread/$threadId"

    /**
     * The notification id for one alert.
     *
     * A single-thread alert keeps an id derived from its thread, so a later
     * alert for the same thread replaces the earlier one. A grouped alert names
     * no thread; it gets a fresh id from the caller's sequence so two groups do
     * not replace each other in the tray.
     */
    fun notificationId(threadId: String?, groupSequence: Int): Int =
      threadId?.hashCode() ?: (GROUP_NOTIFICATION_BASE + groupSequence)
  }
}
