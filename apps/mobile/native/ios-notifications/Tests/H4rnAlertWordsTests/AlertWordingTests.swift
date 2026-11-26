import XCTest

@testable import H4rnAlertWords

/// Words that name themselves, so a rule's choice is visible in the result.
private struct TagWords: AlertWords {
  func text(_ word: AlertWord) -> String { "<\(word.rawValue)>" }
  func text(_ word: AlertCountWord, count: Int) -> String { "<\(word.rawValue):\(count)>" }
}

/// The extension's real words in one language, from the package's bundle.
func bundleWords(_ localization: String) throws -> BundleAlertWords {
  // SwiftPM writes `zh-Hans.lproj` as `zh-hans.lproj`; the extension's
  // bundle keeps the source name.
  let url = try XCTUnwrap(
    Bundle.module.url(forResource: localization, withExtension: "lproj")
      ?? Bundle.module.url(forResource: localization.lowercased(), withExtension: "lproj"),
    "no \(localization).lproj in the bundle")
  let bundle = try XCTUnwrap(Bundle(url: url))
  return BundleAlertWords(bundle: bundle, locale: Locale(identifier: localization))
}

private func alert(_ userInfo: [AnyHashable: Any]) -> AlertWording {
  AlertWording(userInfo: userInfo, title: "English title", body: "English body")
}

final class AlertWordingTests: XCTestCase {
  // MARK: - Rules

  func testASingleAlertNamesItsThreadAndWordsItsState() {
    let words = TagWords()
    let finished = alert([
      "state": "completed", "project": "h4rn", "count": "1", "thread_title": "Fix the bug",
    ])
    XCTAssertEqual(finished.displayTitle(words), "Fix the bug")
    XCTAssertEqual(finished.displayBody(words), "h4rn · <push_finished>")

    let failed = alert(["state": "failed", "project": "", "thread_title": "x"])
    XCTAssertEqual(failed.displayBody(words), "<push_failed>")

    let working = alert(["state": "working", "project": "p", "thread_title": "x"])
    XCTAssertEqual(working.displayBody(words), "p · <push_working>")

    // The asks name no project, like Android and Connect's English body.
    let cases: [(String, String)] = [
      ("approval", "<push_needs_approval>"),
      ("input", "<push_asks_for_input>"),
      ("limit", "<push_stopped_at_limit>"),
    ]
    for (state, body) in cases {
      let ask = alert(["state": state, "project": "h4rn", "thread_title": "x"])
      XCTAssertEqual(ask.displayBody(words), body, state)
      XCTAssertEqual(ask.displayTitle(words), "x", state)
    }
  }

  func testAnUntitledThreadIsSaidInThePhonesLanguage() {
    let untitled = alert(["state": "approval", "thread_title": "  "])
    XCTAssertEqual(untitled.threadTitle, "")
    XCTAssertEqual(untitled.displayTitle(TagWords()), "<push_untitled_thread>")
  }

  func testAGroupCountsItsAgentsAndListsItsTitles() {
    let words = TagWords()
    for state in ["approval", "input", "limit"] {
      let group = alert([
        "state": state, "count": "3", "thread_title": "", "thread_titles": "one\n\n two ",
      ])
      XCTAssertEqual(group.displayTitle(words), "<push_agents_need_attention:3>", state)
      XCTAssertEqual(group.displayBody(words), "one · <push_untitled_thread> · two", state)
    }
    for state in ["completed", "failed"] {
      let group = alert(["state": state, "count": 2, "thread_title": "", "thread_titles": "a\nb"])
      XCTAssertEqual(group.displayTitle(words), "<push_agents_finished:2>", state)
      XCTAssertEqual(group.displayBody(words), "a · b", state)
    }
  }

  func testAGroupWithoutItsTitlesKeepsTheEnglishList() {
    // Connect leaves the list out when it would not fit in the payload.
    let group = alert(["state": "completed", "count": "4", "thread_title": ""])
    XCTAssertEqual(group.displayTitle(TagWords()), "<push_agents_finished:4>")
    XCTAssertEqual(group.displayBody(TagWords()), "English body")
  }

  func testAnAlertFromAnOlderRelayIsShownAsSent() {
    // No `thread_title`: Connect from before ADR-054 sends only the English.
    let old = alert(["state": "completed", "project": "h4rn", "count": "1"])
    XCTAssertNil(old.threadTitle)
    XCTAssertEqual(old.displayTitle(TagWords()), "English title")
    XCTAssertEqual(old.displayBody(TagWords()), "English body")

    let oldGroup = alert(["state": "approval", "count": "2", "thread_titles": "a\nb"])
    XCTAssertEqual(oldGroup.displayTitle(TagWords()), "English title")
    XCTAssertEqual(oldGroup.displayBody(TagWords()), "English body")
  }

  func testAStateThisAppDoesNotKnowKeepsConnectsWords() {
    for state in ["", "paused", "Completed"] {
      let unknown = alert(["state": state, "thread_title": "x", "project": "p"])
      XCTAssertEqual(unknown.displayTitle(TagWords()), "English title", state)
      XCTAssertEqual(unknown.displayBody(TagWords()), "English body", state)
    }
    let group = alert(["state": "paused", "count": "2", "thread_title": "", "thread_titles": "a"])
    XCTAssertEqual(group.displayTitle(TagWords()), "English title")
    XCTAssertEqual(group.displayBody(TagWords()), "English body")
  }

  func testAMissingOrBadCountIsOneThread() {
    XCTAssertEqual(alert([:]).count, 1)
    XCTAssertEqual(alert(["count": "many"]).count, 1)
    XCTAssertEqual(alert(["count": "0"]).count, 1)
    XCTAssertEqual(alert(["count": "-3"]).count, 1)
    XCTAssertEqual(alert(["count": " 2 "]).count, 2)
    XCTAssertEqual(alert(["count": 5]).count, 5)
    XCTAssertEqual(alert(["count": NSNumber(value: 6)]).count, 6)
    // A non-string title is not a title.
    XCTAssertNil(alert(["thread_title": 3]).threadTitle)
  }

  func testOnlyTheDocumentedKeysAreRead() {
    let parsed = alert([
      "state": " completed ", "project": " h4rn ", "count": "1", "thread_title": " t ",
      "prompt": "secret", "aps": ["alert": ["title": "x"]],
    ])
    XCTAssertEqual(
      parsed,
      AlertWording(
        title: "English title", body: "English body", project: "h4rn", state: "completed",
        count: 1, threadTitle: "t", threadTitles: nil))
  }

  // MARK: - Each language

  func testEnglish() throws {
    let words = try bundleWords("en")
    XCTAssertEqual(
      alert(["state": "completed", "project": "h4rn", "thread_title": ""]).displayTitle(words),
      "A thread")
    XCTAssertEqual(
      alert(["state": "completed", "project": "h4rn", "thread_title": ""]).displayBody(words),
      "h4rn · Finished")
    XCTAssertEqual(alert(["state": "failed", "thread_title": "x"]).displayBody(words), "Failed")
    XCTAssertEqual(
      alert(["state": "approval", "thread_title": "x"]).displayBody(words), "Needs approval")
    XCTAssertEqual(
      alert(["state": "input", "thread_title": "x"]).displayBody(words), "Asks for input")
    XCTAssertEqual(
      alert(["state": "limit", "thread_title": "x"]).displayBody(words), "Stopped at a limit")
    let group = alert(["state": "input", "count": "2", "thread_title": "", "thread_titles": "\nb"])
    XCTAssertEqual(group.displayBody(words), "A thread · b")
  }

  #if canImport(Darwin)
    // Linux Foundation does not read .stringsdict plural rules.
    func testEnglishPlurals() throws {
      let words = try bundleWords("en")
      XCTAssertEqual(words.text(.agentsNeedAttention, count: 1), "1 agent needs attention")
      XCTAssertEqual(words.text(.agentsNeedAttention, count: 3), "3 agents need attention")
      XCTAssertEqual(words.text(.agentsFinished, count: 1), "1 agent finished")
      XCTAssertEqual(words.text(.agentsFinished, count: 2), "2 agents finished")
      let group = alert(["state": "input", "count": "2", "thread_title": "", "thread_titles": "\nb"])
      XCTAssertEqual(group.displayTitle(words), "2 agents need attention")
    }
  #endif

  func testSimplifiedChinese() throws {
    let words = try bundleWords("zh-Hans")
    XCTAssertEqual(
      alert(["state": "completed", "project": "h4rn", "thread_title": ""]).displayTitle(words),
      "对话")
    XCTAssertEqual(
      alert(["state": "completed", "project": "h4rn", "thread_title": ""]).displayBody(words),
      "h4rn · 已完成")
    XCTAssertEqual(
      alert(["state": "failed", "project": "p", "thread_title": "x"]).displayBody(words), "p · 失败")
    XCTAssertEqual(alert(["state": "approval", "thread_title": "x"]).displayBody(words), "需要审批")
    XCTAssertEqual(alert(["state": "input", "thread_title": "x"]).displayBody(words), "需要你回答")
    XCTAssertEqual(alert(["state": "limit", "thread_title": "x"]).displayBody(words), "已达到上限并停止")
    let group = alert(["state": "failed", "count": "2", "thread_title": "", "thread_titles": "a\n"])
    XCTAssertEqual(group.displayBody(words), "a · 对话")
  }

  #if canImport(Darwin)
    // Linux Foundation does not read .stringsdict plural rules.
    func testSimplifiedChinesePlurals() throws {
      let words = try bundleWords("zh-Hans")
      XCTAssertEqual(words.text(.agentsNeedAttention, count: 1), "1 个智能体需要你处理")
      XCTAssertEqual(words.text(.agentsNeedAttention, count: 3), "3 个智能体需要你处理")
      XCTAssertEqual(words.text(.agentsFinished, count: 3), "3 个智能体已完成")
      let group = alert(["state": "failed", "count": "2", "thread_title": "", "thread_titles": "a\n"])
      XCTAssertEqual(group.displayTitle(words), "2 个智能体已完成")
    }
  #endif

  func testSpanish() throws {
    let words = try bundleWords("es")
    XCTAssertEqual(
      alert(["state": "completed", "project": "h4rn", "thread_title": ""]).displayTitle(words),
      "Un hilo")
    XCTAssertEqual(
      alert(["state": "completed", "project": "h4rn", "thread_title": ""]).displayBody(words),
      "h4rn · Terminado")
    XCTAssertEqual(alert(["state": "failed", "thread_title": "x"]).displayBody(words), "Fallido")
    XCTAssertEqual(
      alert(["state": "approval", "thread_title": "x"]).displayBody(words), "Requiere aprobación")
    XCTAssertEqual(
      alert(["state": "input", "thread_title": "x"]).displayBody(words), "Pide una respuesta")
    XCTAssertEqual(
      alert(["state": "limit", "thread_title": "x"]).displayBody(words),
      "Se detuvo al llegar a un límite")
    let group = alert(["state": "approval", "count": "3", "thread_title": "", "thread_titles": "a\nb\n"])
    XCTAssertEqual(group.displayBody(words), "a · b · Un hilo")
  }

  #if canImport(Darwin)
    // Linux Foundation does not read .stringsdict plural rules.
    func testSpanishPlurals() throws {
      let words = try bundleWords("es")
      XCTAssertEqual(words.text(.agentsNeedAttention, count: 1), "1 agente necesita atención")
      XCTAssertEqual(words.text(.agentsNeedAttention, count: 3), "3 agentes necesitan atención")
      XCTAssertEqual(words.text(.agentsFinished, count: 1), "1 agente terminó")
      XCTAssertEqual(words.text(.agentsFinished, count: 2), "2 agentes terminaron")
      // CLDR's `many` (a million) reads like `other`. Foundation groups the
      // digits the Spanish way; a real group is far too small to show it.
      XCTAssertEqual(
        words.text(.agentsFinished, count: 1_000_000), "1.000.000 agentes terminaron")
      let group = alert(["state": "approval", "count": "3", "thread_title": "", "thread_titles": "a\nb\n"])
      XCTAssertEqual(group.displayTitle(words), "3 agentes necesitan atención")
    }
  #endif

  func testJapanese() throws {
    let words = try bundleWords("ja")
    XCTAssertEqual(
      alert(["state": "completed", "project": "h4rn", "thread_title": ""]).displayTitle(words),
      "スレッド")
    XCTAssertEqual(
      alert(["state": "completed", "project": "h4rn", "thread_title": ""]).displayBody(words),
      "h4rn · 完了")
    XCTAssertEqual(alert(["state": "failed", "thread_title": "x"]).displayBody(words), "失敗")
    XCTAssertEqual(alert(["state": "approval", "thread_title": "x"]).displayBody(words), "承認が必要")
    XCTAssertEqual(alert(["state": "input", "thread_title": "x"]).displayBody(words), "回答を求めています")
    XCTAssertEqual(alert(["state": "limit", "thread_title": "x"]).displayBody(words), "上限で停止しました")
    let group = alert(["state": "completed", "count": "2", "thread_title": "", "thread_titles": "\n"])
    XCTAssertEqual(group.displayBody(words), "スレッド · スレッド")
  }

  #if canImport(Darwin)
    // Linux Foundation does not read .stringsdict plural rules.
    func testJapanesePlurals() throws {
      let words = try bundleWords("ja")
      XCTAssertEqual(words.text(.agentsNeedAttention, count: 1), "エージェント 1 件が対応待ち")
      XCTAssertEqual(words.text(.agentsNeedAttention, count: 3), "エージェント 3 件が対応待ち")
      XCTAssertEqual(words.text(.agentsFinished, count: 3), "エージェント 3 件が完了")
      let group = alert(["state": "completed", "count": "2", "thread_title": "", "thread_titles": "\n"])
      XCTAssertEqual(group.displayTitle(words), "エージェント 2 件が完了")
    }
  #endif
}
