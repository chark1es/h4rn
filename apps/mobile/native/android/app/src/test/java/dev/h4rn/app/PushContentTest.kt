package dev.h4rn.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Spanish words, so a test can tell local wording from Connect's English. */
private object SpanishWords : AlertWords {
  override fun untitledThread() = "Un hilo"
  override fun withProject(project: String, word: String) = "$project · $word"
  override fun working() = "En curso"
  override fun finished() = "Terminado"
  override fun failed() = "Fallido"
  override fun needsApproval() = "Requiere aprobación"
  override fun asksForInput() = "Pide una respuesta"
  override fun stoppedAtLimit() = "Se detuvo en un límite"
  override fun waitingForApproval() = "Esperando aprobación"
  override fun askedAQuestion() = "Hizo una pregunta"
  override fun agentsNeedAttention(count: Int) = "$count agentes necesitan atención"
  override fun agentsFinished(count: Int) = "$count agentes terminaron"
}

/** Pure alert content rules; no Firebase, credentials or emulator needed. */
class PushContentTest {
  private fun data(
    thread: String = "th_one",
    count: Int = 1,
    state: String = "approval",
  ): Map<String, String> = mapOf(
    "h4rn_alert" to "agent",
    "state" to state,
    "thread_id" to thread,
    "title" to "Fix the thing",
    "body" to "Needs approval",
    "project" to "h4rn",
    "count" to count.toString(),
  )

  /** What a Connect that sends the raw titles adds (ADR-054). */
  private fun structured(
    state: String,
    threadTitle: String = "Fix the thing",
    project: String = "h4rn",
  ): Map<String, String> = data(state = state) + mapOf(
    "thread_title" to threadTitle,
    "project" to project,
  )

  @Test
  fun theExtraDataFieldsAreIgnored() {
    val content = PushContent.from(mapOf("title" to "T", "body" to "B"))
    assertEquals("T", content.title)
    assertEquals("B", content.body)
    assertEquals(null, content.threadId)
    assertFalse(PushContent.from(emptyMap()).title.isEmpty())
  }

  @Test
  fun theOpenThreadIsQuietOnlyInTheForeground() {
    assertTrue(
      PushContent.shouldDrop(
        data("th_one"),
        enabled = true,
        foreground = true,
        openThread = "th_one",
      ),
    )
    assertFalse(
      PushContent.shouldDrop(
        data("th_one"),
        enabled = true,
        foreground = false,
        openThread = "th_one",
      ),
    )
    assertFalse(
      PushContent.shouldDrop(
        data("th_two"),
        enabled = true,
        foreground = true,
        openThread = "th_one",
      ),
    )
    assertFalse(
      PushContent.shouldDrop(data("th_one"), enabled = true, foreground = true, openThread = null),
    )
  }

  @Test
  fun aGroupedAlertIsNeverMutedAsTheOpenThread() {
    // The open thread is first in the group, but the other agent still needs
    // the person; the alert must not be dropped.
    assertFalse(
      PushContent.shouldDrop(
        data("th_one", count = 2),
        enabled = true,
        foreground = true,
        openThread = "th_one",
      ),
    )
  }

  @Test
  fun theLocalSwitchOffDropsEveryAlert() {
    // A failed removal can leave the relay row behind, so the phone's own
    // switch is the last word even while a data message arrives.
    assertTrue(
      PushContent.shouldDrop(
        data("th_two", count = 2),
        enabled = false,
        foreground = false,
        openThread = null,
      ),
    )
  }

  @Test
  fun aTapOpensTheThreadDeepLink() {
    assertEquals("h4rn://thread/th_one", PushContent.deepLink("th_one"))
  }

  @Test
  fun aSingleAlertKeepsItsThreadNotificationId() {
    assertEquals("th_one".hashCode(), PushContent.notificationId("th_one", 0))
    // The group sequence is irrelevant when the alert names a thread.
    assertEquals("th_one".hashCode(), PushContent.notificationId("th_one", 7))
  }

  @Test
  fun eachGroupedAlertGetsItsOwnNotificationId() {
    val first = PushContent.notificationId(null, 1)
    val second = PushContent.notificationId(null, 2)
    assertTrue("two groups shared a notification id", first != second)
    // Neither group id may replace a single-thread alert on the same phone.
    assertTrue(first != "th_one".hashCode())
  }

  @Test
  fun aRunningTurnIsAnOngoingLiveUpdate() {
    val working = PushContent.from(data(state = "working"))
    assertTrue(working.isLiveUpdate())
    assertEquals("h4rn · En curso", working.liveBody(SpanishWords))

    val approval = PushContent.from(data(state = "approval"))
    assertTrue(approval.isLiveUpdate())
    assertEquals("h4rn · Esperando aprobación", approval.liveBody(SpanishWords))

    val input = PushContent.from(data(state = "input"))
    assertTrue(input.isLiveUpdate())
    assertEquals("h4rn · Hizo una pregunta", input.liveBody(SpanishWords))

    val noProject = PushContent.from(structured("working", project = ""))
    assertEquals("En curso", noProject.liveBody(SpanishWords))
  }

  @Test
  fun aSingleAlertIsWordedInThePhonesLanguage() {
    val expected = mapOf(
      "completed" to "h4rn · Terminado",
      "failed" to "h4rn · Fallido",
      "approval" to "Requiere aprobación",
      "input" to "Pide una respuesta",
      "limit" to "Se detuvo en un límite",
      "working" to "h4rn · En curso",
    )
    for ((state, body) in expected) {
      val content = PushContent.from(structured(state))
      assertEquals(state, "Fix the thing", content.displayTitle(SpanishWords))
      assertEquals(state, body, content.displayBody(SpanishWords))
    }
    // Without a project a finish is the bare word.
    assertEquals(
      "Terminado",
      PushContent.from(structured("completed", project = "")).displayBody(SpanishWords),
    )
  }

  @Test
  fun anUntitledThreadIsNamedInThePhonesLanguage() {
    // Connect's English title for it is "A thread"; the raw title is empty.
    val content = PushContent.from(structured("approval", threadTitle = "") + ("title" to "A thread"))
    assertEquals("Un hilo", content.displayTitle(SpanishWords))
  }

  @Test
  fun anOlderConnectsAlertKeepsItsEnglishWords() {
    // No thread_title: the relay predates local wording, so its title and
    // body are shown as sent, for a single alert and a group alike.
    val single = PushContent.from(data(state = "completed") + ("body" to "h4rn · Finished"))
    assertEquals("Fix the thing", single.displayTitle(SpanishWords))
    assertEquals("h4rn · Finished", single.displayBody(SpanishWords))

    val group = PushContent.from(
      data(thread = "", count = 2, state = "completed") +
        mapOf("title" to "2 agents finished", "body" to "one · two"),
    )
    assertEquals("2 agents finished", group.displayTitle(SpanishWords))
    assertEquals("one · two", group.displayBody(SpanishWords))
  }

  @Test
  fun anUnknownStateKeepsConnectsWords() {
    // A state a newer Connect adds has no local word yet.
    val content = PushContent.from(structured("paused") + ("body" to "Paused"))
    assertEquals("Paused", content.displayBody(SpanishWords))
    assertEquals("Paused", content.liveBody(SpanishWords))
    val group = PushContent.from(
      structured("paused", threadTitle = "") + mapOf("thread_id" to "", "count" to "3", "title" to "3 paused"),
    )
    assertEquals("3 paused", group.displayTitle(SpanishWords))
  }

  @Test
  fun aGroupIsCountedAndListedInThePhonesLanguage() {
    fun group(state: String, titles: String?): PushContent {
      val fields = data(thread = "", count = 3, state = state) +
        mapOf("thread_title" to "", "title" to "3 agents …", "body" to "one · A thread · two")
      return PushContent.from(if (titles == null) fields else fields + ("thread_titles" to titles))
    }
    for (state in listOf("approval", "input", "limit")) {
      assertEquals(state, "3 agentes necesitan atención", group(state, "one\n\ntwo").displayTitle(SpanishWords))
    }
    for (state in listOf("completed", "failed")) {
      assertEquals(state, "3 agentes terminaron", group(state, "one\n\ntwo").displayTitle(SpanishWords))
    }
    // An untitled thread in the list is named locally, in Connect's order.
    assertEquals("one · Un hilo · two", group("completed", "one\n\ntwo").displayBody(SpanishWords))
    // A group too large to repeat its titles falls back to Connect's list.
    assertEquals("one · A thread · two", group("completed", null).displayBody(SpanishWords))
  }

  @Test
  fun aFinishIsTheNormalAlertNotALiveUpdate() {
    for (state in listOf("completed", "failed", "limit")) {
      val content = PushContent.from(data(state = state))
      assertFalse("$state was shown as a running turn", content.isLiveUpdate())
    }
  }

  @Test
  fun aTurnStartIsNeverANormalAlert() {
    // With Live Updates off, "working" must not appear as if it were a finish.
    assertTrue(PushContent.from(data(state = "working")).isLiveOnly())
    assertFalse(PushContent.from(data(state = "approval")).isLiveOnly())
    assertFalse(PushContent.from(data(state = "completed")).isLiveOnly())
  }

  @Test
  fun aGroupedUpdateIsNeverALiveUpdate() {
    // A grouped alert names no thread, so it cannot be one ongoing update.
    val grouped = PushContent.from(data(thread = "", count = 2, state = "working"))
    assertEquals(null, grouped.threadId)
    assertFalse(grouped.isLiveUpdate())
  }

  @Test
  fun theFinalAlertReusesTheLiveUpdateId() {
    val live = PushContent.from(data(state = "working"))
    val finished = PushContent.from(data(state = "completed"))
    val liveId = PushContent.notificationId(live.threadId, 0)
    assertEquals(liveId, PushContent.notificationId(finished.threadId, 0))
    // And that id is the thread's own, not a group id.
    assertEquals("th_one".hashCode(), liveId)
  }

  @Test
  fun aTurnStartWithoutALiveUpdateClearsOnlyATrackedEntry() {
    // th_live had a Live Update before the switch went off: the new turn
    // start clears it. th_done's id holds its unread Finished alert, and a
    // queued message's turn start must not erase it.
    val tracked = setOf("th_live")
    assertTrue(PushContent.turnStartClearsEntry("th_live", tracked))
    assertFalse(PushContent.turnStartClearsEntry("th_done", tracked))
    assertFalse(PushContent.turnStartClearsEntry("th_done", emptySet()))
    assertFalse(PushContent.turnStartClearsEntry(null, tracked))
  }

  @Test
  fun onlyATrackedLiveUpdateTheMachineListsAsStoppedIsStale() {
    // A cold start: th_stopped was stopped while the app was closed and
    // th_grouped finished in a grouped alert. th_elsewhere runs on another
    // machine, which this list knows nothing about, so it stays.
    val tracked = setOf("th_stopped", "th_grouped", "th_elsewhere")
    assertEquals(
      setOf("th_stopped", "th_grouped"),
      PushContent.staleLiveThreads(tracked, setOf("th_stopped", "th_grouped", "th_untracked")),
    )
    // A list that names none of them clears nothing; nothing tracked, nothing.
    assertTrue(PushContent.staleLiveThreads(tracked, emptySet()).isEmpty())
    assertTrue(PushContent.staleLiveThreads(emptySet(), setOf("th_stopped")).isEmpty())
  }
}
