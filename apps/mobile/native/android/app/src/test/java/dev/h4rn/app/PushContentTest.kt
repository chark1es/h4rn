package dev.h4rn.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Pure alert content rules; no Firebase, credentials or emulator needed. */
class PushContentTest {
  private fun data(thread: String = "th_one", count: Int = 1): Map<String, String> = mapOf(
    "h4rn_alert" to "agent",
    "state" to "approval",
    "thread_id" to thread,
    "title" to "Fix the thing",
    "body" to "Needs approval",
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
}
