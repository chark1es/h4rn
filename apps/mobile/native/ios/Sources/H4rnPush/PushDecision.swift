import Foundation

/// What to do with one alert, without touching UIKit (ADR-051, ADR-052).
///
/// The rules are the ones the Android notification service follows: the local
/// switch off drops everything, an alert is quiet only when the app is in the
/// foreground and that exact thread is on screen, and a grouped alert is never
/// quiet because it names no single thread.
public enum PushDecision {
  /// The thread a tap opens, or nil for a grouped alert.
  public static func threadId(from userInfo: [AnyHashable: Any]) -> String? {
    guard let raw = userInfo["thread_id"] as? String else { return nil }
    let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
    return trimmed.isEmpty ? nil : trimmed
  }

  /// How many threads one alert represents; unknown is one.
  public static func count(from userInfo: [AnyHashable: Any]) -> Int {
    switch userInfo["count"] {
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

  /// Whether the alert must not be shown.
  ///
  /// A switched-off phone is silent even when a message still arrives: the
  /// relay row may outlive a removal that could not reach Connect.
  public static func shouldSuppress(
    userInfo: [AnyHashable: Any],
    enabled: Bool,
    foreground: Bool,
    openThread: String?
  ) -> Bool {
    if !enabled { return true }
    if !foreground { return false }
    if count(from: userInfo) > 1 { return false }
    guard let thread = threadId(from: userInfo) else { return false }
    let open = openThread?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    return thread == open
  }

  /// The deep link a tap follows, the same route the Android app uses.
  public static func deepLink(threadId: String) -> String {
    return "h4rn://thread/\(threadId)"
  }
}
