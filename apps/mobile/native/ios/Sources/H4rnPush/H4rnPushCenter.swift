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

  /// The exact thread on screen, as reported by the shared UI.
  public var openThread: String?

  private let defaults = UserDefaults.standard
  private weak var webView: WKWebView?
  private var pendingLinks: [String] = []
  private var tokenWaiters: [(Result<String, Error>) -> Void] = []
  private var installed = false

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

  /// Installs the delegate and the APNs registration capture once.
  public func install() {
    DispatchQueue.main.async {
      guard !self.installed else { return }
      self.installed = true
      UNUserNotificationCenter.current().delegate = self
      h4rnInstallRemoteNotificationCapture()
    }
  }

  /// Keeps the WebView that receives a tap's deep link.
  public func attach(webView: WKWebView) {
    DispatchQueue.main.async {
      self.webView = webView
      let links = self.pendingLinks
      self.pendingLinks.removeAll()
      for link in links { self.evaluate(link, in: webView) }
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

  /// Returns the APNs device token, waiting for registration when needed.
  public func token(completion: @escaping (Result<String, Error>) -> Void) {
    DispatchQueue.main.async {
      if let token = self.deviceToken, !token.isEmpty {
        completion(.success(token))
        return
      }
      self.tokenWaiters.append(completion)
      UIApplication.shared.registerForRemoteNotifications()
      // Registration answers quickly or not at all; a command must not hang.
      DispatchQueue.main.asyncAfter(deadline: .now() + 10) {
        self.failWaiters(H4rnPushError.noToken)
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
      for waiter in waiters { waiter(.success(hex)) }
    }
  }

  /// Reports a registration failure instead of leaving a command hanging.
  public func registrationFailed(_ error: Error) {
    DispatchQueue.main.async {
      self.failWaiters(error)
    }
  }

  private func failWaiters(_ error: Error) {
    let waiters = tokenWaiters
    tokenWaiters.removeAll()
    for waiter in waiters { waiter(.failure(error)) }
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
      guard let webView = self.webView else {
        // A cold start: the webview does not exist yet. Keep the link until
        // the shell attaches it.
        self.pendingLinks.append(deepLink)
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
