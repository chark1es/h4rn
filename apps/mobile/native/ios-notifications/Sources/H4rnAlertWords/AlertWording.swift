import Foundation

/// One fixed word of a phone alert.
///
/// The raw value is the key in each language's `Localizable.strings`, and the
/// same name Android uses in `push_strings.xml`.
public enum AlertWord: String, CaseIterable, Sendable {
  /// A thread with no title.
  case untitledThread = "push_untitled_thread"
  case working = "push_working"
  case finished = "push_finished"
  case failed = "push_failed"
  case needsApproval = "push_needs_approval"
  case asksForInput = "push_asks_for_input"
  case stoppedAtLimit = "push_stopped_at_limit"
}

/// A grouped alert's title, which changes with the count.
///
/// The raw value is the key in each language's `Localizable.stringsdict`.
public enum AlertCountWord: String, CaseIterable, Sendable {
  /// Every agent in the group needs the person.
  case agentsNeedAttention = "push_agents_need_attention"
  /// Every agent in the group finished or failed.
  case agentsFinished = "push_agents_finished"
}

/// The words an alert is written in, in the phone's language.
///
/// The extension reads them from its bundle (`BundleAlertWords`); tests pass
/// their own, so the wording rules run without a device.
public protocol AlertWords {
  func text(_ word: AlertWord) -> String
  func text(_ word: AlertCountWord, count: Int) -> String
}

/// An alert's facts, and the rules that word it (ADR-054, ADR-055).
///
/// These are the same rules as Android's `PushContent`. Connect sends the
/// English `title` and `body` in the APNs alert, which iOS shows when this
/// code does not run, and the state, project, count and raw thread titles as
/// custom keys. Only the presence of `thread_title` switches to local wording,
/// so an alert from a relay that predates it keeps Connect's English, and so
/// does a state this app does not know.
public struct AlertWording: Equatable, Sendable {
  public static let stateWorking = "working"
  public static let stateApproval = "approval"
  public static let stateInput = "input"
  public static let stateLimit = "limit"
  public static let stateCompleted = "completed"
  public static let stateFailed = "failed"

  private static let knownStates: Set<String> = [
    stateWorking, stateApproval, stateInput, stateLimit, stateCompleted, stateFailed,
  ]

  /// Connect's English title, shown as sent when the alert cannot be worded.
  public let title: String
  /// Connect's English body, shown as sent when the alert cannot be worded.
  public let body: String
  public let project: String
  public let state: String
  /// How many threads the alert stands for; at least one.
  public let count: Int
  /// The thread's raw title, empty when it has none; nil from an older Connect.
  public let threadTitle: String?
  /// A group's raw titles in order, empty for an untitled thread; nil when
  /// Connect left them out to keep the payload small.
  public let threadTitles: [String]?

  public init(
    title: String,
    body: String,
    project: String = "",
    state: String = "",
    count: Int = 1,
    threadTitle: String? = nil,
    threadTitles: [String]? = nil
  ) {
    self.title = title
    self.body = body
    self.project = project
    self.state = state
    self.count = max(count, 1)
    self.threadTitle = threadTitle
    self.threadTitles = threadTitles
  }

  /// Reads an APNs payload's custom keys next to the alert iOS would show.
  ///
  /// Only these keys are read; anything else in the payload is ignored.
  public init(userInfo: [AnyHashable: Any], title: String, body: String) {
    func string(_ key: String) -> String? {
      (userInfo[key] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines)
    }
    self.init(
      title: title,
      body: body,
      project: string("project") ?? "",
      state: string("state") ?? "",
      count: Self.count(userInfo["count"]),
      // Only presence matters: an empty value is an untitled thread.
      threadTitle: string("thread_title"),
      // One title per line; an empty line is an untitled thread.
      threadTitles: (userInfo["thread_titles"] as? String)?
        .split(separator: "\n", omittingEmptySubsequences: false)
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
    )
  }

  /// Connect sends the count as a string; unknown is one thread.
  private static func count(_ value: Any?) -> Int {
    switch value {
    case let value as String:
      return Int(value.trimmingCharacters(in: .whitespacesAndNewlines)) ?? 1
    case let value as Int:
      return value
    case let value as NSNumber:
      return value.intValue
    default:
      return 1
    }
  }

  /// Whether Connect sent the fields, for a state this app knows.
  private var wordable: Bool {
    threadTitle != nil && Self.knownStates.contains(state)
  }

  private var grouped: Bool { count > 1 }

  /// The notification title, in the phone's language when the alert allows it.
  public func displayTitle(_ words: AlertWords) -> String {
    guard wordable, let threadTitle else { return title }
    if !grouped {
      return threadTitle.isEmpty ? words.text(.untitledThread) : threadTitle
    }
    switch state {
    case Self.stateApproval, Self.stateInput, Self.stateLimit:
      return words.text(.agentsNeedAttention, count: count)
    case Self.stateCompleted, Self.stateFailed:
      return words.text(.agentsFinished, count: count)
    default:
      return title
    }
  }

  /// The notification text, in the phone's language when the alert allows it.
  public func displayBody(_ words: AlertWords) -> String {
    guard wordable else { return body }
    if grouped {
      // Too large a group arrives without its titles; Connect's body then
      // lists them, with "A thread" in English for an untitled one.
      guard let threadTitles else { return body }
      let untitled = words.text(.untitledThread)
      return threadTitles.map { $0.isEmpty ? untitled : $0 }.joined(separator: " · ")
    }
    switch state {
    case Self.stateWorking:
      return withProject(words.text(.working))
    case Self.stateCompleted:
      return withProject(words.text(.finished))
    case Self.stateFailed:
      return withProject(words.text(.failed))
    case Self.stateApproval:
      return words.text(.needsApproval)
    case Self.stateInput:
      return words.text(.asksForInput)
    case Self.stateLimit:
      return words.text(.stoppedAtLimit)
    default:
      return body
    }
  }

  /// "h4rn · Finished": the same in every language, like Android's
  /// untranslatable `push_with_project`.
  private func withProject(_ word: String) -> String {
    project.isEmpty ? word : "\(project) · \(word)"
  }
}
