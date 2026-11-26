import Foundation
import Tauri
import UserNotifications
import WebKit

class PushForegroundArgs: Decodable {
  let threadId: String?
}

class PushEnabledArgs: Decodable {
  let enabled: Bool
}

/// iOS push bridge (ADR-052).
///
/// The command surface and response shapes match the Android `PushPlugin`:
/// `push_status` resolves the same fields, `push_token` resolves
/// `{ "token": string }`, and the other three commands resolve with no payload.
/// The shared UI therefore needs no iOS-specific branch beyond detecting the
/// platform.
class H4rnPushPlugin: Plugin {
  override public func load(webview: WKWebView) {
    H4rnPushCenter.shared.attach(webView: webview)
    H4rnPushCenter.shared.install()
  }

  @objc public func pushStatus(_ invoke: Invoke) {
    H4rnPushCenter.shared.status { payload in
      invoke.resolve(payload)
    }
  }

  @objc public func pushRequestPermission(_ invoke: Invoke) {
    H4rnPushCenter.shared.requestPermission { error in
      if let error {
        invoke.reject(error.localizedDescription)
      } else {
        invoke.resolve()
      }
    }
  }

  @objc public func pushToken(_ invoke: Invoke) {
    H4rnPushCenter.shared.token { result in
      switch result {
      case .success(let token):
        var payload: JsonObject = [:]
        payload["token"] = token
        invoke.resolve(payload)
      case .failure(let error):
        invoke.reject(error.localizedDescription)
      }
    }
  }

  @objc public func pushSetForegroundThread(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(PushForegroundArgs.self)
    let trimmed = args.threadId?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    DispatchQueue.main.async {
      H4rnPushCenter.shared.openThread = trimmed.isEmpty ? nil : trimmed
      invoke.resolve()
    }
  }

  /// Mirrors the shared UI's switch so the delegate can drop alerts while off.
  @objc public func pushSetEnabled(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(PushEnabledArgs.self)
    DispatchQueue.main.async {
      H4rnPushCenter.shared.enabled = args.enabled
      invoke.resolve()
    }
  }
}

@_cdecl("init_plugin_h4rn_push")
public func initPluginPush() -> Plugin {
  return H4rnPushPlugin()
}
