import Foundation

/// Alert words from a bundle's `Localizable.strings` and `.stringsdict`.
///
/// The extension passes its own bundle, and iOS has already picked the
/// bundle's language from the phone's preferred languages: English,
/// Simplified Chinese, Spanish or Japanese, and English for any other.
public struct BundleAlertWords: AlertWords {
  private let bundle: Bundle
  private let locale: Locale

  /// Words from `bundle`, with plurals chosen by `locale`'s rules.
  public init(bundle: Bundle, locale: Locale) {
    self.bundle = bundle
    self.locale = locale
  }

  /// Words from `bundle` in the language iOS picked for it.
  public init(bundle: Bundle = .main) {
    let language = bundle.preferredLocalizations.first ?? "en"
    self.init(bundle: bundle, locale: Locale(identifier: language))
  }

  public func text(_ word: AlertWord) -> String {
    bundle.localizedString(forKey: word.rawValue, value: nil, table: nil)
  }

  public func text(_ word: AlertCountWord, count: Int) -> String {
    // The stringsdict entry picks the plural form from the count.
    let format = bundle.localizedString(forKey: word.rawValue, value: nil, table: nil)
    return String(format: format, locale: locale, count)
  }
}
