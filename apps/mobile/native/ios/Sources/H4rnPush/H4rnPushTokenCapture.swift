import ObjectiveC
import UIKit

/// Captures the APNs device token without owning the app delegate (ADR-052).
///
/// Tauri's iOS runtime (TAO) declares the `AppDelegate` class itself, so there
/// is no project file to add a method to. Adding the two remote-notification
/// delegate methods to that class at runtime is the same hook Firebase and
/// other push SDKs install, and it chains to whatever implementation already
/// exists instead of replacing it.

private var h4rnOriginalDidRegister: IMP?
private var h4rnOriginalDidFail: IMP?

private let h4rnDidRegisterToken:
  @convention(c) (AnyObject, Selector, UIApplication, Data) -> Void = {
    target, selector, application, token in
    H4rnPushCenter.shared.store(deviceToken: token)
    if let original = h4rnOriginalDidRegister {
      typealias Register = @convention(c) (AnyObject, Selector, UIApplication, Data) -> Void
      unsafeBitCast(original, to: Register.self)(target, selector, application, token)
    }
  }

private let h4rnDidFailToRegister:
  @convention(c) (AnyObject, Selector, UIApplication, Error) -> Void = {
    target, selector, application, error in
    H4rnPushCenter.shared.registrationFailed(error)
    if let original = h4rnOriginalDidFail {
      typealias FailToRegister = @convention(c) (AnyObject, Selector, UIApplication, Error) -> Void
      unsafeBitCast(original, to: FailToRegister.self)(target, selector, application, error)
    }
  }

func h4rnInstallRemoteNotificationCapture() {
  guard let delegate = UIApplication.shared.delegate else {
    // The runtime declares the delegate class during launch; `load` runs while
    // the first webview is created, which is inside that launch.
    return
  }
  let appDelegate: AnyClass = type(of: delegate)

  let registerSelector = #selector(
    UIApplicationDelegate.application(_:didRegisterForRemoteNotificationsWithDeviceToken:))
  if let method = class_getInstanceMethod(appDelegate, registerSelector) {
    h4rnOriginalDidRegister = method_getImplementation(method)
    method_setImplementation(method, unsafeBitCast(h4rnDidRegisterToken, to: IMP.self))
  } else {
    class_addMethod(
      appDelegate,
      registerSelector,
      unsafeBitCast(h4rnDidRegisterToken, to: IMP.self),
      "v@:@@"
    )
  }

  let failSelector = #selector(
    UIApplicationDelegate.application(_:didFailToRegisterForRemoteNotificationsWithError:))
  if let method = class_getInstanceMethod(appDelegate, failSelector) {
    let original = method_getImplementation(method)
    h4rnOriginalDidFail = original
    method_setImplementation(
      method,
      unsafeBitCast(h4rnDidFailToRegister, to: IMP.self)
    )
  } else {
    class_addMethod(
      appDelegate,
      failSelector,
      unsafeBitCast(h4rnDidFailToRegister, to: IMP.self),
      "v@:@@"
    )
  }
}
