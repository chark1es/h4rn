package dev.h4rn.app

/**
 * The words an alert is written in, in the phone's language.
 *
 * The service reads them from string resources ([ResourceAlertWords]); tests
 * pass their own, so the wording rules run on the JVM without a device.
 */
interface AlertWords {
  /** A thread with no title. */
  fun untitledThread(): String

  /** One state word after the project: "h4rn · Finished". */
  fun withProject(project: String, word: String): String

  fun working(): String

  fun finished(): String

  fun failed(): String

  fun needsApproval(): String

  fun asksForInput(): String

  fun stoppedAtLimit(): String

  /** A Live Update's word while an approval waits. */
  fun waitingForApproval(): String

  /** A Live Update's word while a question waits. */
  fun askedAQuestion(): String

  /** A grouped alert's title when every agent in it needs the person. */
  fun agentsNeedAttention(count: Int): String

  /** A grouped alert's title when every agent in it finished or failed. */
  fun agentsFinished(count: Int): String
}

/**
 * The fixed alert fields Connect sends in an FCM data message.
 *
 * Android composes the notification itself so the message can be a data
 * message: there is no server-authored notification payload in the tray before
 * the app runs, and a suppressed foreground alert never appears. Only these
 * fields are read; anything else in the map is ignored.
 *
 * [title] and [body] are Connect's English wording. A Connect that also sends
 * the raw thread title ([threadTitle] is then non-null) lets the phone word the
 * alert itself in its own language from the state, project, count and titles;
 * an older Connect's alert is shown in its English words as before.
 */
data class PushContent(
  val title: String,
  val body: String,
  val threadId: String?,
  val project: String = "",
  val state: String = "",
  val count: Int = 1,
  /** The thread's raw title, empty when it has none; null from an older Connect. */
  val threadTitle: String? = null,
  /** A group's raw titles in order, empty for an untitled thread; null when not sent. */
  val threadTitles: List<String>? = null,
) {
  /**
   * Whether this transition belongs in the ongoing Live Update (ADR-053).
   *
   * A running turn, a waiting approval and a question are all "still going";
   * only a finish, a failure or a limit becomes the normal dismissible alert.
   * A grouped update names no thread, so it is never a Live Update.
   */
  fun isLiveUpdate(): Boolean =
    threadId != null && (state == STATE_WORKING || state == STATE_APPROVAL || state == STATE_INPUT)

  /**
   * Whether this transition may only appear as a Live Update.
   *
   * A turn start is not news on its own, so with the switch off it must not
   * become a normal notification that looks like a finish. A waiting approval
   * or a question is still an ordinary alert when the switch is off.
   */
  fun isLiveOnly(): Boolean = state == STATE_WORKING

  /** Whether Connect sent the fields this phone words the alert from. */
  private fun structured(): Boolean = threadTitle != null

  /** Whether the alert stands for several threads. */
  private fun grouped(): Boolean = count > 1

  /** The notification title, in the phone's language when Connect allows it. */
  fun displayTitle(words: AlertWords): String {
    if (!structured()) return title
    if (!grouped()) return threadTitle.orEmpty().ifEmpty { words.untitledThread() }
    return when (state) {
      STATE_APPROVAL, STATE_INPUT, STATE_LIMIT -> words.agentsNeedAttention(count)
      STATE_COMPLETED, STATE_FAILED -> words.agentsFinished(count)
      else -> title
    }
  }

  /** The notification text, in the phone's language when Connect allows it. */
  fun displayBody(words: AlertWords): String {
    if (!structured()) return body
    if (grouped()) {
      // Too large a group arrives without its titles; Connect's body then
      // lists them, with "A thread" in English for an untitled one.
      val titles = threadTitles ?: return body
      return titles.joinToString(" · ") { it.ifEmpty { words.untitledThread() } }
    }
    return when (state) {
      STATE_WORKING -> project(words, words.working())
      STATE_COMPLETED -> project(words, words.finished())
      STATE_FAILED -> project(words, words.failed())
      STATE_APPROVAL -> words.needsApproval()
      STATE_INPUT -> words.asksForInput()
      STATE_LIMIT -> words.stoppedAtLimit()
      else -> body
    }
  }

  /** The Live Update line: the project and what the agent is doing now. */
  fun liveBody(words: AlertWords): String = when (state) {
    STATE_WORKING -> project(words, words.working())
    STATE_APPROVAL -> project(words, words.waitingForApproval())
    STATE_INPUT -> project(words, words.askedAQuestion())
    else -> displayBody(words)
  }

  private fun project(words: AlertWords, word: String): String =
    if (project.isEmpty()) word else words.withProject(project, word)

  companion object {
    const val CHANNEL_ID = "h4rn_alerts"

    const val STATE_WORKING = "working"
    const val STATE_APPROVAL = "approval"
    const val STATE_INPUT = "input"
    const val STATE_LIMIT = "limit"
    const val STATE_COMPLETED = "completed"
    const val STATE_FAILED = "failed"

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
      val project = data["project"].orEmpty().trim()
      val state = data["state"].orEmpty().trim()
      val count = data["count"]?.trim()?.toIntOrNull()?.coerceAtLeast(1) ?: 1
      // Only presence matters: an empty value is an untitled thread.
      val threadTitle = data["thread_title"]?.trim()
      // One title per line; an empty line is an untitled thread.
      val threadTitles = data["thread_titles"]?.split('\n')?.map { it.trim() }
      return PushContent(
        title = title,
        body = body,
        threadId = thread,
        project = project,
        state = state,
        count = count,
        threadTitle = threadTitle,
        threadTitles = threadTitles,
      )
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

    /**
     * The tracked Live Updates whose turn has ended.
     *
     * [stopped] is every thread the connected machine lists with no turn in
     * progress. A tracked id in it is a turn that ended without replacing its
     * entry: a Stop, a grouped finish, or one that ended while the app was
     * closed. A tracked id the machine does not list may be another machine's
     * running agent, so it stays.
     */
    fun staleLiveThreads(tracked: Set<String>, stopped: Set<String>): Set<String> =
      tracked intersect stopped

    /**
     * Whether a turn start that is not shown as a Live Update clears the
     * thread's notification.
     *
     * Only a Live Update this app is tracking is cleared: it belongs to a turn
     * that has ended. Otherwise the thread's notification id may hold its last
     * Finished, Failed or limit alert, which the person has not read yet, so the
     * frame is dropped without touching the tray.
     */
    fun turnStartClearsEntry(threadId: String?, tracked: Set<String>): Boolean =
      threadId != null && threadId in tracked

    /** The deep link a tap opens. */
    fun deepLink(threadId: String): String = "h4rn://thread/$threadId"

    /**
     * The notification id for one thread's turn.
     *
     * A Live Update for a turn and its final alert share this id, so the
     * finish, failure or limit replaces the ongoing notification instead of
     * leaving it behind. A grouped alert names no thread; it gets a fresh id
     * from the caller's sequence so two groups do not replace each other.
     */
    fun notificationId(threadId: String?, groupSequence: Int): Int =
      threadId?.hashCode() ?: (GROUP_NOTIFICATION_BASE + groupSequence)
  }
}
