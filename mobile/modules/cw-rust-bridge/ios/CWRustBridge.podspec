# Local Expo module: the Swift bridge to the Rust core in mobile/rust.
#
# CWMobile.xcframework is built by `scripts/build-rust.sh ios` and committed,
# because EAS builders have no Rust toolchain. Rebuild and commit it whenever
# mobile/rust changes.
Pod::Spec.new do |s|
  s.name           = 'CWRustBridge'
  s.version        = '1.1.0'
  s.summary        = 'Swift bridge to the Consistent CEO mobile Rust core'
  s.description    = 'Exposes the Rust natural-language parser (cw_parse_reminder, cw_parse_note) to JS as the CWRustBridge native module.'
  s.license        = 'MIT'
  s.author         = 'Consistent CEO'
  s.homepage       = 'https://consistent.ceo'
  s.platforms      = {
    :ios => '16.4'
  }
  s.swift_version  = '5.9'
  s.source         = { git: 'https://github.com/Al-Baqshi/consistent.ceo.git' }
  s.static_framework = true

  s.dependency 'ExpoModulesCore'

  # Top level only: a recursive glob would also pick up the headers inside the
  # XCFramework.
  s.source_files = '*.swift'
  s.vendored_frameworks = 'CWMobile.xcframework'
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'SWIFT_COMPILATION_MODE' => 'wholemodule'
  }
end
