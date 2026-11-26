import Foundation
import UIKit
import UserNotifications
import WebKit

/// The process-wide push state and the notification centre delegate (ADR-052).
///
/// iOS shows an alert from its own notification block, so unlike Android the
/// app never renders the body. The delegate here only decides whether to keep
/// the alert quiet (foreground, exact thread, or the switch off) and where a
/// tap goes.
public final class H4rnPushCenter: NSObject, UNUserNotificationCenterDelegate {
  public static let shared = H4rnPushCenter()

  private enum Key {
    static let token = "h4rn.push.apnsToken"
    static let enabled = "h4rn.push.alertsEnabled"
  }

  /// How long `token()` waits for APNs before falling back to the cache.
  ///
  /// A fresh answer is what rotates a token that Apple replaced, so the cache
  /// is a fallback for an offline launch, not the normal answer.
  private static let tokenTimeout: TimeInterval = 5

  /// The exact thread on screen, as reported by the shared UI.
  public var openThread: String?

  private struct TokenWaiter {
    let id: UUID
    let completion: (Result<String, Error>) -> Void
  }

  private let defaults = UserDefaults.standard
  private weak var webView: WKWebView?
  private var tokenWaiters: [TokenWaiter] = []
  private var installed = false
  /// Whether the page has installed its deep-link listener and run boot.
  ///
  /// A tap that arrives before this point has nowhere to land, so it is held
  /// for {@link takePendingLink} instead of being evaluated into the page.
  private var pageReady = false
  private var pendingLink: String?

  /// The person's local switch.
  ///
  /// A process woken for an alert has no UI to ask, so only a stored Off
  /// silences it; an unset value is On, matching Android.
  public var enabled: Bool {
    get {
      if defaults.object(forKey: Key.enabled) == nil { return true }
      return defaults.bool(forKey: Key.enabled)
    }
    set {
      defaults.set(newValue, forKey: Key.enabled)
      if !newValue {
        // A second line of defence for an alert Connect still delivered: the
        // tray is cleared the moment the switch goes off.
        UNUserNotificationCenter.current().removeAllDeliveredNotifications()
      }
    }
  }

  /// The APNs device token, when registration has answered.
  public var deviceToken: String? {
    get { defaults.string(forKey: Key.token) }
    set {
      if let newValue, !newValue.isEmpty {
        defaults.set(newValue, forKey: Key.token)
      } else {
        defaults.removeObject(forKey: Key.token)
      }
    }
  }

  /// Becomes the notification delegate, synchronously.
  ///
  /// Apple requires the delegate before the app finishes launching: a tap
  /// that cold-starts the app is delivered to whatever delegate is set then,
  /// and a delegate assigned later (after an async hop, or when the webview
  /// loads) never receives it. The plugin's constructor calls this during
  /// Tauri's setup, inside launch.
  public func installDelegate() {
    UNUserNotificationCenter.current().delegate = self
  }

  /// Installs the delegate, the APNs capture and this launch's registration.
  public func install() {
    installDelegate()
    DispatchQueue.main.async {
      guard !self.installed else { return }
      self.installed = true
      h4rnInstallRemoteNotificationCapture()
      self.rearmRegistration()
    }
  }

  /// Asks APNs for the current token on every launch.
  ///
  /// A phone restored from a backup can carry a token Apple already retired in
  /// the cache. Registering again is what asks for the live one; the cached
  /// value is only used when APNs does not answer.
  private func rearmRegistration() {
    UNUserNotificationCenter.current().getNotificationSettings { settings in
      guard Self.permissionGranted(settings.authorizationStatus) else { return }
      DispatchQueue.main.async {
        UIApplication.shared.registerForRemoteNotifications()
      }
    }
  }

  private static func permissionGranted(_ status: UNAuthorizationStatus) -> Bool {
    switch status {
    case .authorized, .provisional, .ephemeral:
      return true
    default:
      return false
    }
  }

  /// Keeps the WebView that receives a tap's deep link.
  ///
  /// A link buffered before the page existed is not evaluated here: the page
  /// has not installed its listener yet. The UI reads it with
  /// {@link takePendingLink} once it has.
  public func attach(webView: WKWebView) {
    DispatchQueue.main.async {
      self.webView = webView
      self.pageReady = false
    }
  }

  /// Returns the link a tap buffered before the page was ready, once.
  ///
  /// The UI calls this during boot, after installing its deep-link listener.
  /// Calling it also marks the page ready, so later taps dispatch normally.
  public func takePendingLink(completion: @escaping (String?) -> Void) {
    DispatchQueue.main.async {
      self.pageReady = true
      let link = self.pendingLink
      self.pendingLink = nil
      completion(link)
    }
  }

  /// Reads this phone's real notification state for Settings.
  public func status(completion: @escaping ([String: Any?]) -> Void) {
    UNUserNotificationCenter.current().getNotificationSettings { settings in
      let permission: String
      switch settings.authorizationStatus {
      case .authorized, .provisional, .ephemeral:
        permission = "granted"
      case .denied:
        permission = "denied"
      case .notDetermined:
        permission = "default"
      @unknown default:
        permission = "unknown"
      }
      DispatchQueue.main.async {
        completion([
          "available": true,
          "permission": permission,
          "token": self.deviceToken.map { $0 as Any } ?? NSNull(),
          // iOS has no notification channels; the field keeps the shared
          // Settings code identical to Android.
          "channel": true,
        ])
      }
    }
  }

  /// Asks for notification permission, and registers for APNs when granted.
  public func requestPermission(completion: @escaping (Error?) -> Void) {
    DispatchQueue.main.async {
      UNUserNotificationCenter.current().requestAuthorization(
        options: [.alert, .sound, .badge]
      ) { _, error in
        DispatchQueue.main.async {
          if let error {
            completion(error)
            return
          }
          UIApplication.shared.registerForRemoteNotifications()
          completion(nil)
        }
      }
    }
  }

  /// Returns the APNs device token, waiting for this launch's registration.
  ///
  /// The cache is only a fallback after {@link tokenTimeout}: a token that a
  /// backup restored can name a device Apple has already retired, and handing
  /// it back forever means Connect deletes the row and the phone registers the
  /// same dead token again. Each call owns its timeout, so one slow waiter
  /// cannot fail the others.
  public func token(completion: @escaping (Result<String, Error>) -> Void) {
    DispatchQueue.main.async {
      let id = UUID()
      self.tokenWaiters.append(TokenWaiter(id: id, completion: completion))
      UIApplication.shared.registerForRemoteNotifications()
      // Registration answers quickly or not at all; a command must not hang.
      DispatchQueue.main.asyncAfter(deadline: .now() + Self.tokenTimeout) {
        guard let index = self.tokenWaiters.firstIndex(where: { $0.id == id }) else {
          return
        }
        let waiter = self.tokenWaiters.remove(at: index)
        if let cached = self.deviceToken, !cached.isEmpty {
          waiter.completion(.success(cached))
        } else {
          waiter.completion(.failure(H4rnPushError.noToken))
        }
      }
    }
  }

  /// Records the token UIKit handed the app delegate.
  public func store(deviceToken: Data) {
    let hex = deviceToken.map { String(format: "%02x", $0) }.joined()
    DispatchQueue.main.async {
      self.deviceToken = hex
      let waiters = self.tokenWaiters
      self.tokenWaiters.removeAll()
      for waiter in waiters { waiter.completion(.success(hex)) }
    }
  }

  /// Reports a registration failure instead of leaving a command hanging.
  public func registrationFailed(_ error: Error) {
    DispatchQueue.main.async {
      let waiters = self.tokenWaiters
      self.tokenWaiters.removeAll()
      for waiter in waiters { waiter.completion(.failure(error)) }
    }
  }

  // MARK: - UNUserNotificationCenterDelegate

  /// A foreground alert is kept only when it is for a thread not on screen.
  public func userNotificationCenter(
    _ center: UNUserNotificationCenter,
    willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) {
    let suppressed = PushDecision.shouldSuppress(
      userInfo: notification.request.content.userInfo,
      enabled: enabled,
      // `willPresent` means the app is in the foreground by definition.
      foreground: true,
      openThread: openThread
    )
    completionHandler(suppressed ? [] : [.banner, .sound, .list])
  }

  /// A tap opens its thread, or the app for a grouped alert.
  public func userNotificationCenter(
    _ center: UNUserNotificationCenter,
    didReceive response: UNNotificationResponse,
    withCompletionHandler completionHandler: @escaping () -> Void
  ) {
    if let thread = PushDecision.threadId(
      from: response.notification.request.content.userInfo
    ) {
      open(deepLink: PushDecision.deepLink(threadId: thread))
    }
    completionHandler()
  }

  private func open(deepLink: String) {
    DispatchQueue.main.async {
      guard let webView = self.webView, self.pageReady else {
        // A cold start, or a page still loading: there is no listener yet.
        // Keep the link for the one-shot command the UI reads during boot.
        self.pendingLink = deepLink
        return
      }
      self.evaluate(deepLink, in: webView)
    }
  }

  /// Dispatches the same event Tauri's deep-link plugin emits, so the shared
  /// UI's existing route opens the thread.
  private func evaluate(_ link: String, in webView: WKWebView) {
    guard
      let data = try? JSONSerialization.data(withJSONObject: [link]),
      let json = String(data: data, encoding: .utf8)
    else { return }
    let script = "window.dispatchEvent(new CustomEvent('h4rn-deep-link', { detail: \(json) }));"
    webView.evaluateJavaScript(script)
  }
}

enum H4rnPushError: LocalizedError {
  case noToken

  var errorDescription: String? {
    switch self {
    case .noToken:
      return "This phone has not received an APNs token. Check the build's push capability and try again."
    }
  }
}
