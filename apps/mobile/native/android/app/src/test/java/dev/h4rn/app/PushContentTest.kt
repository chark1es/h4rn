package dev.h4rn.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Pure alert content rules; no Firebase, credentials or emulator needed. */
class PushContentTest {
  private fun data(thread: String = "th_one"): Map<String, String> = mapOf(
    "h4rn_alert" to "agent",
    "state" to "approval",
    "thread_id" to thread,
    "title" to "Fix the thing",
    "body" to "Needs approval",
    "count" to "1",
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
    assertTrue(PushContent.isSuppressed(data("th_one"), foreground = true, openThread = "th_one"))
    assertFalse(PushContent.isSuppressed(data("th_one"), foreground = false, openThread = "th_one"))
    assertFalse(PushContent.isSuppressed(data("th_two"), foreground = true, openThread = "th_one"))
    assertFalse(PushContent.isSuppressed(data("th_one"), foreground = true, openThread = null))
  }

  @Test
  fun aTapOpensTheThreadDeepLink() {
    assertEquals("h4rn://thread/th_one", PushContent.deepLink("th_one"))
  }
}
