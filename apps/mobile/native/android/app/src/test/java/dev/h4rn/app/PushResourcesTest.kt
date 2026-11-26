package dev.h4rn.app

import java.io.File
import javax.xml.parsers.DocumentBuilderFactory
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element

/**
 * Every language words every alert (ADR-054).
 *
 * Reads each `push_strings.xml` under `res/` from the module, so a string missing
 * in one language, a lost `%d`, or a plural without the quantities its
 * language needs fails here instead of showing English on that phone.
 */
class PushResourcesTest {
  private val languages = mapOf(
    "values" to setOf("one", "other"),
    "values-zh-rCN" to setOf("other"),
    "values-es" to setOf("one", "many", "other"),
    "values-ja" to setOf("other"),
  )

  private fun read(folder: String): Element {
    val file = File("src/main/res/$folder/push_strings.xml")
    assertTrue("missing ${file.path}", file.isFile)
    return DocumentBuilderFactory.newInstance().newDocumentBuilder().parse(file).documentElement
  }

  private fun Element.children(tag: String): List<Element> {
    val nodes = getElementsByTagName(tag)
    return (0 until nodes.length).map { nodes.item(it) as Element }
  }

  private fun strings(root: Element): Map<String, String> =
    root.children("string")
      .filter { it.getAttribute("translatable") != "false" }
      .associate { it.getAttribute("name") to it.textContent }

  private fun plurals(root: Element): Map<String, Map<String, String>> =
    root.children("plurals").associate { plural ->
      plural.getAttribute("name") to
        plural.children("item").associate { it.getAttribute("quantity") to it.textContent }
    }

  @Test
  fun everyLanguageHasEveryAlertWord() {
    val english = read("values")
    val englishStrings = strings(english)
    val englishPlurals = plurals(english)
    assertTrue(englishStrings.containsKey("push_channel_name"))
    for ((folder, quantities) in languages) {
      val root = read(folder)
      val translated = strings(root)
      assertEquals(folder, englishStrings.keys, translated.keys)
      for ((name, text) in translated) {
        assertTrue("$folder/$name is empty", text.isNotBlank())
      }
      val translatedPlurals = plurals(root)
      assertEquals(folder, englishPlurals.keys, translatedPlurals.keys)
      for ((name, items) in translatedPlurals) {
        assertEquals("$folder/$name quantities", quantities, items.keys)
        for ((quantity, text) in items) {
          assertTrue("$folder/$name/$quantity lost its count", text.contains("%d"))
        }
      }
    }
  }

  @Test
  fun theProjectSeparatorIsSharedByEveryLanguage() {
    // Only English defines it, so no language can drop the project.
    val english = read("values").children("string").first { it.getAttribute("name") == "push_with_project" }
    assertEquals("false", english.getAttribute("translatable"))
    assertTrue(english.textContent.contains("%1\$s") && english.textContent.contains("%2\$s"))
    for (folder in languages.keys - "values") {
      val names = read(folder).children("string").map { it.getAttribute("name") }
      assertTrue("$folder redefines push_with_project", "push_with_project" !in names)
    }
  }
}
