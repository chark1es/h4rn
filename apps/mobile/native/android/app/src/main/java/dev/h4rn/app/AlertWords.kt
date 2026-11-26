package dev.h4rn.app

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.os.Build

/**
 * Alert words from the app's string resources (each `push_strings.xml` under `res/`).
 *
 * Android picks the file for the phone's language, and falls back to English
 * for a language h4rn does not have.
 */
class ResourceAlertWords(private val context: Context) : AlertWords {
  private fun text(id: Int): String = context.getString(id)

  override fun untitledThread(): String = text(R.string.push_untitled_thread)

  override fun withProject(project: String, word: String): String =
    context.getString(R.string.push_with_project, project, word)

  override fun working(): String = text(R.string.push_working)

  override fun finished(): String = text(R.string.push_finished)

  override fun failed(): String = text(R.string.push_failed)

  override fun needsApproval(): String = text(R.string.push_needs_approval)

  override fun asksForInput(): String = text(R.string.push_asks_for_input)

  override fun stoppedAtLimit(): String = text(R.string.push_stopped_at_limit)

  override fun waitingForApproval(): String = text(R.string.push_waiting_for_approval)

  override fun askedAQuestion(): String = text(R.string.push_asked_a_question)

  override fun agentsNeedAttention(count: Int): String =
    context.resources.getQuantityString(R.plurals.push_agents_need_attention, count, count)

  override fun agentsFinished(count: Int): String =
    context.resources.getQuantityString(R.plurals.push_agents_finished, count, count)
}

/** The one notification channel h4rn's alerts use. */
object AlertChannel {
  /**
   * Creates the channel, or renames it.
   *
   * Creating a channel that already exists only updates its name and
   * description, so calling this each time keeps them in the phone's current
   * language after the person changes it. The person's own importance and
   * sound choices are kept.
   */
  fun ensure(context: Context) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val manager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    val channel = NotificationChannel(
      PushContent.CHANNEL_ID,
      context.getString(R.string.push_channel_name),
      NotificationManager.IMPORTANCE_HIGH,
    ).apply {
      description = context.getString(R.string.push_channel_description)
      setShowBadge(true)
    }
    manager.createNotificationChannel(channel)
  }
}
