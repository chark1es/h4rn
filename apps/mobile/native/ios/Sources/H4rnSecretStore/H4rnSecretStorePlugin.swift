import Foundation
import Security
import Tauri

/// iOS counterpart of `SecretStorePlugin.kt` (ADR-016).
///
/// Values are generic-password items in the data-protection keychain, scoped to
/// the app's bundle identifier and to this device only. The item is readable
/// after the first unlock so a background reconnect can still read the daemon
/// token, but it never migrates to another device through iCloud Keychain.
///
/// The Rust command surface (`secret_get`, `secret_set`, `secret_delete`) and
/// the response shapes match the Android plugin: `secret_get` resolves
/// `{ "value": string | null }`, `secret_set` and `secret_delete` resolve with
/// no payload, and deleting a missing key succeeds.
class SecretGetArgs: Decodable {
  let key: String
}

class SecretSetArgs: Decodable {
  let key: String
  let value: String
}

class SecretDeleteArgs: Decodable {
  let key: String
}

class H4rnSecretStorePlugin: Plugin {
  /// Stable service namespace. The account is the caller's logical key, which
  /// keeps every secret addressable without a second index.
  private static let service = "dev.h4rn.app.secrets"

  private func baseQuery(account: String) -> [String: Any] {
    return [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: H4rnSecretStorePlugin.service,
      kSecAttrAccount as String: account,
      // Explicitly opt into the data-protection keychain on every OS version
      // that supports it, and never allow iCloud synchronisation.
      kSecUseDataProtectionKeychain as String: true,
      kSecAttrSynchronizable as String: false,
    ]
  }

  private func keychainError(_ status: OSStatus, operation: String) -> NSError {
    let message = SecCopyErrorMessageString(status, nil) as String? ?? "status \(status)"
    return NSError(
      domain: NSOSStatusErrorDomain,
      code: Int(status),
      userInfo: [NSLocalizedDescriptionKey: "could not \(operation) the secret: \(message)"]
    )
  }

  @objc public func secretGet(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(SecretGetArgs.self)

    var query = baseQuery(account: args.key)
    query[kSecReturnData as String] = true
    query[kSecMatchLimit as String] = kSecMatchLimitOne

    var item: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &item)

    if status == errSecItemNotFound {
      invoke.resolve(["value": nil])
      return
    }
    guard status == errSecSuccess else {
      throw keychainError(status, operation: "read")
    }
    guard let data = item as? Data,
      let secret = String(data: data, encoding: .utf8)
    else {
      // A value written by another build or with a different encoding is not
      // recoverable as text; drop it so the app can pair again instead of
      // failing forever (same recovery the Android plugin implements).
      SecItemDelete(baseQuery(account: args.key) as CFDictionary)
      throw NSError(
        domain: NSOSStatusErrorDomain,
        code: Int(errSecDecode),
        userInfo: [NSLocalizedDescriptionKey: "the stored secret is not valid UTF-8"]
      )
    }

    invoke.resolve(["value": secret])
  }

  @objc public func secretSet(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(SecretSetArgs.self)
    guard let data = args.value.data(using: .utf8) else {
      throw NSError(
        domain: NSOSStatusErrorDomain,
        code: Int(errSecParam),
        userInfo: [NSLocalizedDescriptionKey: "the secret is not valid UTF-8"]
      )
    }

    // Replace semantics: delete first so an accessible-attribute change cannot
    // leave the old item in place, then add with the required protection class.
    SecItemDelete(baseQuery(account: args.key) as CFDictionary)

    var query = baseQuery(account: args.key)
    query[kSecValueData as String] = data
    query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly

    let status = SecItemAdd(query as CFDictionary, nil)
    guard status == errSecSuccess else {
      throw keychainError(status, operation: "store")
    }

    invoke.resolve()
  }

  @objc public func secretDelete(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(SecretDeleteArgs.self)
    let status = SecItemDelete(baseQuery(account: args.key) as CFDictionary)
    guard status == errSecSuccess || status == errSecItemNotFound else {
      throw keychainError(status, operation: "delete")
    }
    invoke.resolve()
  }
}

@_cdecl("init_plugin_h4rn_secret_store")
public func initPlugin() -> Plugin {
  return H4rnSecretStorePlugin()
}
