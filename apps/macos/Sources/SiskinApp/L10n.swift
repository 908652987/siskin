import Foundation

/// SwiftPM resource bundles do not always report the system language as a
/// preferred localization (the bundle can advertise only `en`), so the table
/// is loaded from the matching `.lproj` directly.
enum L10n {
    private static let strings: [String: String] = load()

    static func t(_ key: String) -> String {
        strings[key] ?? key
    }

    static func t(_ key: String, _ arguments: CVarArg...) -> String {
        String(format: t(key), arguments: arguments)
    }

    private static func load() -> [String: String] {
        let environment = ProcessInfo.processInfo.environment
        let preferred = environment["SISKIN_LANG"]
            ?? Locale.preferredLanguages.first
            ?? "en"
        let language = preferred.hasPrefix("zh") ? "zh-Hans" : "en"
        for code in [language, "en"] {
            guard let base = Bundle.module.resourceURL else { continue }
            let path = base.appendingPathComponent("\(code).lproj/Localizable.strings")
            if let table = NSDictionary(contentsOf: path) as? [String: String] {
                return table
            }
        }
        return [:]
    }
}
