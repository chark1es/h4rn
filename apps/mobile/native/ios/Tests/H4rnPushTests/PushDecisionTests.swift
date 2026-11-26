import XCTest

@testable import H4rnPush

final class PushDecisionTests: XCTestCase {
  func testTheExactOpenThreadStaysQuietOnlyInTheForeground() {
    let alert: [AnyHashable: Any] = ["thread_id": "th_one", "count": "1"]
    XCTAssertTrue(
      PushDecision.shouldSuppress(
        userInfo: alert, enabled: true, foreground: true, openThread: "th_one"))
    XCTAssertFalse(
      PushDecision.shouldSuppress(
        userInfo: alert, enabled: true, foreground: false, openThread: "th_one"))
    XCTAssertFalse(
      PushDecision.shouldSuppress(
        userInfo: alert, enabled: true, foreground: true, openThread: "th_two"))
    XCTAssertFalse(
      PushDecision.shouldSuppress(
        userInfo: alert, enabled: true, foreground: true, openThread: nil))
  }

  func testAGroupedAlertIsNeverQuiet() {
    let group: [AnyHashable: Any] = ["thread_id": "", "count": "2"]
    XCTAssertFalse(
      PushDecision.shouldSuppress(
        userInfo: group, enabled: true, foreground: true, openThread: "th_one"))
    // The same rule applies when a group somehow still names a thread.
    let named: [AnyHashable: Any] = ["thread_id": "th_one", "count": "3"]
    XCTAssertFalse(
      PushDecision.shouldSuppress(
        userInfo: named, enabled: true, foreground: true, openThread: "th_one"))
  }

  func testTheSwitchOffDropsEverything() {
    let alert: [AnyHashable: Any] = ["thread_id": "th_one", "count": "1"]
    XCTAssertTrue(
      PushDecision.shouldSuppress(
        userInfo: alert, enabled: false, foreground: false, openThread: nil))
    XCTAssertTrue(
      PushDecision.shouldSuppress(
        userInfo: alert, enabled: false, foreground: true, openThread: "th_two"))
  }

  func testAMissingCountIsOneThread() {
    XCTAssertEqual(PushDecision.count(from: [:]), 1)
    XCTAssertEqual(PushDecision.count(from: ["count": " 2 "]), 2)
    XCTAssertEqual(PushDecision.count(from: ["count": 4]), 4)
    XCTAssertEqual(PushDecision.count(from: ["count": "many"]), 1)
  }

  func testATapFollowsTheSameDeepLinkAsAndroid() {
    XCTAssertEqual(PushDecision.deepLink(threadId: "th_one"), "h4rn://thread/th_one")
    XCTAssertNil(PushDecision.threadId(from: [:]))
    XCTAssertNil(PushDecision.threadId(from: ["thread_id": "   "]))
    XCTAssertEqual(
      PushDecision.threadId(from: ["thread_id": " th_one "]), "th_one")
  }
}
