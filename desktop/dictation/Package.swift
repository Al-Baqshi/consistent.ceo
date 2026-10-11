// swift-tools-version: 6.0
import PackageDescription

// On-device dictation sidecar for the Consistent desktop shell.
//
// Wraps Apple's SpeechAnalyzer / SpeechTranscriber (macOS 26+, WWDC25) as a
// small CLI: reads control commands (start/stop/quit) as line-delimited JSON on
// stdin, and streams transcript events as NDJSON on stdout. The Tauri shell
// spawns this the same way it spawns the gateway sidecar.
//
// Speech.framework's new APIs require macOS 26, so the deployment target is
// pinned there. The desktop shell must feature-detect at runtime and fall back
// (Gemini Live) on older systems rather than requiring 26 globally.
let package = Package(
    name: "consistent-dictate",
    platforms: [.macOS("26.0")],
    targets: [
        .executableTarget(
            name: "consistent-dictate",
            path: "Sources/consistent-dictate",
            // Not a compiled source — embedded into the binary via the linker
            // flags below, so keep SwiftPM from treating it as a resource.
            exclude: ["Info.plist"],
            // Language mode v5: the realtime audio-tap callback touches captured
            // reference types off the main actor, which strict v6 concurrency
            // rejects. The SpeechAnalyzer APIs are available regardless of mode.
            swiftSettings: [.swiftLanguageMode(.v5)],
            // Embed Info.plist as a __TEXT,__info_plist section. TCC reads the
            // usage-description strings from there; without it the OS SIGABRTs
            // the process the instant it touches the mic or speech APIs.
            linkerSettings: [
                .unsafeFlags([
                    "-Xlinker", "-sectcreate",
                    "-Xlinker", "__TEXT",
                    "-Xlinker", "__info_plist",
                    "-Xlinker", "Sources/consistent-dictate/Info.plist",
                ])
            ]
        )
    ]
)
