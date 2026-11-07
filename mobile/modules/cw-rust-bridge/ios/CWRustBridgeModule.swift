import CWMobile
import ExpoModulesCore

/// The `CWRustBridge` native module: the Rust core's C ABI
/// (`mobile/rust/include/cw_mobile.h`), as JS functions.
///
/// `src/lib/rustBridge.ts` loads it with `requireOptionalNativeModule`, and
/// falls back to its TS parser when the module is missing (Android, web, an
/// old build) or a call rejects. Every function resolves to the Rust core's
/// JSON string unchanged; the TS side parses it.
public final class CWRustBridgeModule: Module {
  public func definition() -> ModuleDefinition {
    Name("CWRustBridge")

    AsyncFunction("version") { () throws -> String in
      try takeRustString(cw_version(), from: "cw_version")
    }

    AsyncFunction("parseReminder") { (text: String) throws -> String in
      let request = try requestJSON(text)
      return try takeRustString(cw_parse_reminder(request), from: "cw_parse_reminder")
    }

    AsyncFunction("parseNote") { (text: String) throws -> String in
      let request = try requestJSON(text)
      return try takeRustString(cw_parse_note(request), from: "cw_parse_note")
    }
  }
}

/// The request the Rust core expects. The device's UTC offset goes with the
/// text because the core can't rely on reading the time zone itself inside
/// the iOS sandbox (see `mobile/rust/src/lib.rs`).
private func requestJSON(_ text: String) throws -> String {
  let request: [String: Any] = [
    "text": text,
    "utcOffsetMinutes": TimeZone.current.secondsFromGMT() / 60,
  ]
  let data = try JSONSerialization.data(withJSONObject: request)
  guard let json = String(data: data, encoding: .utf8) else {
    throw RustCallFailedException("JSON encoding")
  }
  return json
}

/// Copy a string the Rust core returned, then hand it back to Rust to free.
private func takeRustString(_ pointer: UnsafeMutablePointer<CChar>?, from function: String) throws -> String {
  guard let pointer else {
    throw RustCallFailedException(function)
  }
  defer { cw_free_string(pointer) }
  return String(cString: pointer)
}

final class RustCallFailedException: GenericException<String>, @unchecked Sendable {
  override var reason: String {
    "The Rust core returned no result from \(param)"
  }
}
