import Foundation
import UserNotifications

/// Words each h4rn alert in the phone's language before iOS shows it (ADR-055).
///
/// Connect's APNs alert carries English words in `aps.alert` and the facts as
/// custom keys, with `mutable-content` set so iOS runs this extension first.
/// The extension replaces only the title and body, using `AlertWording`, the
/// same rules as the Android app; everything else (the sound, the custom keys
/// a tap reads) is kept. An alert it cannot word, or one still unanswered
/// when iOS's time runs out, is shown in Connect's English as sent.
///
/// It does not decide whether an alert is shown: the phone's own switch is
/// still enforced by removing the registration from Connect and, while the
/// app runs, by its notification delegate (ADR-052).
final class NotificationService: UNNotificationServiceExtension {
  private let lock = NSLock()
  private var contentHandler: ((UNNotificationContent) -> Void)?
  private var english: UNNotificationContent?

  override func didReceive(
    _ request: UNNotificationRequest,
    withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
  ) {
    lock.lock()
    self.contentHandler = contentHandler
    english = request.content
    lock.unlock()

    guard let content = request.content.mutableCopy() as? UNMutableNotificationContent else {
      deliver(request.content)
      return
    }
    let alert = AlertWording(
      userInfo: content.userInfo, title: content.title, body: content.body)
    let words = BundleAlertWords()
    content.title = alert.displayTitle(words)
    content.body = alert.displayBody(words)
    deliver(content)
  }

  /// iOS is about to show the alert as it arrived; hand it the English.
  override func serviceExtensionTimeWillExpire() {
    lock.lock()
    let english = self.english
    lock.unlock()
    if let english {
      deliver(english)
    }
  }

  /// Calls the handler once, whichever path gets there first.
  private func deliver(_ content: UNNotificationContent) {
    lock.lock()
    let handler = contentHandler
    contentHandler = nil
    lock.unlock()
    handler?(content)
  }
}
