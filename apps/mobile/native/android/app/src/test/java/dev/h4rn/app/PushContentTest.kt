package dev.h4rn.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

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
    assertEquals("h4rn · Working", working.liveBody())

    val approval = PushContent.from(data(state = "approval"))
    assertTrue(approval.isLiveUpdate())
    assertEquals("h4rn · Waiting for approval", approval.liveBody())

    val input = PushContent.from(data(state = "input"))
    assertTrue(input.isLiveUpdate())
    assertEquals("h4rn · Asked a question", input.liveBody())
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
}
