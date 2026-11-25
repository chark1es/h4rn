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

    /** Builds the notification from the data map, with quiet fallbacks. */
    fun from(data: Map<String, String>): PushContent {
      val title = data["title"].orEmpty().trim().ifEmpty { "h4rn" }
      val body = data["body"].orEmpty().trim()
      val thread = data["thread_id"].orEmpty().trim().ifEmpty { null }
      return PushContent(title = title, body = body, threadId = thread)
    }

    /**
     * Whether this alert is for the thread already on screen.
     *
     * Only the foreground with that exact thread is quiet. Any other screen,
     * or a backgrounded app, still alerts; suppressing by visibility on some
     * other device would hide work the person is not looking at.
     */
    fun isSuppressed(data: Map<String, String>, foreground: Boolean, openThread: String?): Boolean {
      if (!foreground) return false
      val thread = data["thread_id"].orEmpty().trim()
      return thread.isNotEmpty() && thread == openThread
    }

    /** The deep link a tap opens. */
    fun deepLink(threadId: String): String = "h4rn://thread/$threadId"
  }
}
