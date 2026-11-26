import XCTest

#if canImport(FoundationXML)
  import FoundationXML
#endif

@testable import H4rnAlertWords

/// Each language has every word, the plural forms its language uses, and the
/// same words as the Android app (docs/i18n.md, Phone alerts).
final class AlertResourcesTests: XCTestCase {
  /// The iOS localization, the Android resource folder, and the plural
  /// categories the language uses (`Intl.PluralRules` / CLDR).
  private static let languages: [(ios: String, android: String, plurals: Set<String>)] = [
    ("en", "values", ["one", "other"]),
    ("zh-Hans", "values-zh-rCN", ["other"]),
    ("es", "values-es", ["one", "many", "other"]),
    ("ja", "values-ja", ["other"]),
  ]

  private static let packageRoot = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()  // H4rnAlertWordsTests
    .deletingLastPathComponent()  // Tests
    .deletingLastPathComponent()  // ios-notifications

  private static func resource(_ language: String, _ file: String) -> URL {
    packageRoot.appendingPathComponent("Sources/H4rnAlertWords/Resources")
      .appendingPathComponent("\(language).lproj")
      .appendingPathComponent(file)
  }

  private static func plist(_ url: URL) throws -> [String: Any] {
    let data = try Data(contentsOf: url)
    let value = try PropertyListSerialization.propertyList(from: data, format: nil)
    return try XCTUnwrap(value as? [String: Any], "\(url.lastPathComponent) is not a dictionary")
  }

  private static func strings(_ language: String) throws -> [String: String] {
    let raw = try plist(resource(language, "Localizable.strings"))
    return try raw.mapValues { try XCTUnwrap($0 as? String) }
  }

  /// Each count word's plural variants, by category.
  private static func plurals(_ language: String) throws -> [String: [String: String]] {
    let raw = try plist(resource(language, "Localizable.stringsdict"))
    var result: [String: [String: String]] = [:]
    for (key, value) in raw {
      let entry = try XCTUnwrap(value as? [String: Any], key)
      XCTAssertEqual(entry["NSStringLocalizedFormatKey"] as? String, "%#@agents@", key)
      let agents = try XCTUnwrap(entry["agents"] as? [String: Any], key)
      XCTAssertEqual(agents["NSStringFormatSpecTypeKey"] as? String, "NSStringPluralRuleType", key)
      XCTAssertEqual(agents["NSStringFormatValueTypeKey"] as? String, "ld", key)
      var forms: [String: String] = [:]
      for (category, text) in agents where !category.hasPrefix("NSString") {
        forms[category] = try XCTUnwrap(text as? String, "\(key).\(category)")
      }
      result[key] = forms
    }
    return result
  }

  func testEveryLanguageHasEveryWordAndItsPluralForms() throws {
    let words = Set(AlertWord.allCases.map(\.rawValue))
    let countWords = Set(AlertCountWord.allCases.map(\.rawValue))
    for language in Self.languages {
      let strings = try Self.strings(language.ios)
      XCTAssertEqual(Set(strings.keys), words, language.ios)
      for (key, text) in strings {
        XCTAssertFalse(text.trimmingCharacters(in: .whitespaces).isEmpty, "\(language.ios) \(key)")
      }
      let plurals = try Self.plurals(language.ios)
      XCTAssertEqual(Set(plurals.keys), countWords, language.ios)
      for (key, forms) in plurals {
        XCTAssertEqual(Set(forms.keys), language.plurals, "\(language.ios) \(key)")
        for (category, text) in forms {
          XCTAssertEqual(
            text.components(separatedBy: "%ld").count, 2,
            "\(language.ios) \(key).\(category) must show the count once")
        }
      }
    }
  }

  /// Android words iOS does not show: the Live Update lines, the channel's
  /// Settings name and description, and the fixed "project · word" join
  /// (iOS joins in Swift).
  private static let androidOnly: Set<String> = [
    "push_with_project", "push_channel_name", "push_channel_description",
    "push_waiting_for_approval", "push_asked_a_question",
  ]

  func testTheWordsAreAndroidsWords() throws {
    for language in Self.languages {
      let android = try AndroidStrings(Self.androidFile(language.android))
      // Both ways: a word Android starts showing in an alert must reach iOS too.
      XCTAssertEqual(
        Set(android.strings.keys).subtracting(Self.androidOnly),
        Set(AlertWord.allCases.map(\.rawValue)),
        "\(language.android) has alert words iOS lacks, or iOS has words Android lacks")
      XCTAssertEqual(
        Set(android.plurals.keys), Set(AlertCountWord.allCases.map(\.rawValue)),
        "\(language.android) plurals differ from iOS's")
      let strings = try Self.strings(language.ios)
      for word in AlertWord.allCases {
        XCTAssertEqual(
          strings[word.rawValue], android.strings[word.rawValue],
          "\(language.ios) \(word.rawValue) differs from Android")
      }
      let plurals = try Self.plurals(language.ios)
      for word in AlertCountWord.allCases {
        // Android formats an Int with %d; Foundation's is %ld.
        let androidForms = try XCTUnwrap(android.plurals[word.rawValue], word.rawValue)
          .mapValues { $0.replacingOccurrences(of: "%d", with: "%ld") }
        XCTAssertEqual(
          plurals[word.rawValue], androidForms,
          "\(language.ios) \(word.rawValue) differs from Android")
      }
    }
  }

  private static func androidFile(_ folder: String) -> URL {
    packageRoot
      .deletingLastPathComponent()  // native
      .appendingPathComponent("android/app/src/main/res")
      .appendingPathComponent(folder)
      .appendingPathComponent("push_strings.xml")
  }
}

/// The `<string>` and `<plurals>` entries of an Android resource file.
private final class AndroidStrings: NSObject, XMLParserDelegate {
  private(set) var strings: [String: String] = [:]
  private(set) var plurals: [String: [String: String]] = [:]
  private var name: String?
  private var plural: String?
  private var quantity: String?
  private var text = ""

  init(_ url: URL) throws {
    super.init()
    let parser = try XCTUnwrap(XMLParser(contentsOf: url), url.path)
    parser.delegate = self
    XCTAssertTrue(parser.parse(), "\(url.path) is not valid XML")
  }

  func parser(
    _ parser: XMLParser, didStartElement element: String, namespaceURI: String?,
    qualifiedName: String?, attributes: [String: String] = [:]
  ) {
    switch element {
    case "string":
      name = attributes["name"]
      text = ""
    case "plurals":
      plural = attributes["name"]
    case "item":
      quantity = attributes["quantity"]
      text = ""
    default:
      break
    }
  }

  func parser(_ parser: XMLParser, foundCharacters string: String) {
    text += string
  }

  func parser(
    _ parser: XMLParser, didEndElement element: String, namespaceURI: String?,
    qualifiedName: String?
  ) {
    switch element {
    case "string":
      if let name { strings[name] = text }
      name = nil
    case "item":
      if let plural, let quantity { plurals[plural, default: [:]][quantity] = text }
      quantity = nil
    case "plurals":
      plural = nil
    default:
      break
    }
  }
}
