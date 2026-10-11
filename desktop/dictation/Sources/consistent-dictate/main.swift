//! On-device dictation sidecar.
//!
//! Protocol (all NDJSON, one JSON object per line):
//!   stdin  (client → sidecar): {"cmd":"start"} | {"cmd":"stop"} | {"cmd":"quit"}
//!   stdout (sidecar → client): {"type":"ready"}
//!                              {"type":"state","value":"listening"|"stopped"}
//!                              {"type":"model","status":"downloading"|"ready"}
//!                              {"type":"volatile","text":"…"}   (in-progress guess)
//!                              {"type":"final","text":"…"}      (committed segment)
//!                              {"type":"error","message":"…"}
//!   stderr: freeform debug logs (never parsed by the client)
//!
//! `volatile` events are the live, rewrite-in-place hypothesis; `final` events
//! are committed text the client should append + inject at the cursor.

import AVFoundation
import Foundation
import Speech

// MARK: - IO helpers

/// Serialize one JSON object as a single stdout line and flush immediately, so
/// the parent process sees events with no buffering lag.
func emit(_ obj: [String: Any]) {
    guard
        let data = try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys]),
        var line = String(data: data, encoding: .utf8)
    else { return }
    line.append("\n")
    FileHandle.standardOutput.write(Data(line.utf8))
}

func log(_ message: String) {
    FileHandle.standardError.write(Data("[dictate] \(message)\n".utf8))
}

// MARK: - Dictation engine

/// Owns the mic capture graph and the SpeechAnalyzer session. Main-actor
/// isolated: `start`/`stop` mutate the graph and must not race. The realtime
/// audio tap deliberately captures plain locals (see `start`) instead of
/// `self`, so it never hops the actor on the audio thread.
@MainActor
final class DictationEngine {
    private let audioEngine = AVAudioEngine()
    private let converter = BufferConverter()

    private var transcriber: SpeechTranscriber?
    private var analyzer: SpeechAnalyzer?
    private var inputBuilder: AsyncStream<AnalyzerInput>.Continuation?
    private var recognizerTask: Task<Void, Never>?
    private var analyzerFormat: AVAudioFormat?
    private var running = false
    /// True only once `analyzer.start(inputSequence:)` has returned. Finalizing
    /// an analyzer that never started hangs, so the teardown path must gate on it.
    private var analyzerStarted = false

    /// Preferred dictation locale; falls back to whatever the system has
    /// installed if this exact one isn't available.
    private let preferredLocale = Locale(identifier: "en-US")

    func start() async {
        guard !running else { return }

        guard await ensurePermissions() else {
            emit(["type": "error", "message": "microphone or speech permission denied"])
            return
        }

        do {
            try await setUpTranscriber()
            try startAudioCapture()
            running = true
            emit(["type": "state", "value": "listening"])
        } catch {
            emit(["type": "error", "message": "start failed: \(error.localizedDescription)"])
            await stop()
        }
    }

    func stop() async {
        guard running || analyzer != nil else { return }

        if audioEngine.isRunning {
            audioEngine.inputNode.removeTap(onBus: 0)
            audioEngine.stop()
        }
        inputBuilder?.finish()
        inputBuilder = nil

        // Flush any trailing audio into a final result before tearing down — but
        // only if the analyzer actually started; finalizing an unstarted one hangs.
        if analyzerStarted, let analyzer {
            try? await analyzer.finalizeAndFinishThroughEndOfInput()
        }
        recognizerTask?.cancel()
        recognizerTask = nil
        analyzer = nil
        transcriber = nil
        analyzerStarted = false
        running = false
        emit(["type": "state", "value": "stopped"])
    }

    // MARK: setup

    private func setUpTranscriber() async throws {
        let transcriber = SpeechTranscriber(
            locale: preferredLocale,
            transcriptionOptions: [],
            reportingOptions: [.volatileResults],
            attributeOptions: [.audioTimeRange])
        self.transcriber = transcriber

        let analyzer = SpeechAnalyzer(modules: [transcriber])
        self.analyzer = analyzer

        try await ensureModel(for: transcriber)

        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber])
        else {
            throw NSError(
                domain: "dictate", code: 1,
                userInfo: [NSLocalizedDescriptionKey: "no compatible audio format"])
        }
        analyzerFormat = format

        // Drain results on a task; volatile vs final drive the UI. Captures the
        // local `transcriber`, not self, so it outlives a teardown race cleanly.
        recognizerTask = Task {
            do {
                for try await result in transcriber.results {
                    let text = String(result.text.characters)
                    if text.isEmpty { continue }
                    if result.isFinal {
                        emit(["type": "final", "text": text])
                    } else {
                        emit(["type": "volatile", "text": text])
                    }
                }
            } catch {
                emit(["type": "error", "message": "recognition stream error: \(error.localizedDescription)"])
            }
        }

        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
        inputBuilder = continuation
        try await analyzer.start(inputSequence: stream)
        analyzerStarted = true
    }

    /// Download + reserve the on-device model for our locale if the system
    /// doesn't already have it. First run on a fresh machine pays a one-time
    /// download; subsequent runs are instant.
    private func ensureModel(for transcriber: SpeechTranscriber) async throws {
        if let request = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
            emit(["type": "model", "status": "downloading"])
            try await request.downloadAndInstall()
        }

        let reserved = await AssetInventory.reservedLocales
        let alreadyReserved = reserved.contains {
            $0.identifier(.bcp47) == preferredLocale.identifier(.bcp47)
        }
        if !alreadyReserved {
            try await AssetInventory.reserve(locale: preferredLocale)
        }
        emit(["type": "model", "status": "ready"])
    }

    private func startAudioCapture() throws {
        let inputNode = audioEngine.inputNode
        let inputFormat = inputNode.outputFormat(forBus: 0)

        // Capture plain locals — never `self` — so the realtime callback stays
        // off the main actor and off any reference cycle.
        guard let analyzerFormat, let continuation = inputBuilder else {
            throw NSError(
                domain: "dictate", code: 2,
                userInfo: [NSLocalizedDescriptionKey: "capture started before analyzer was ready"])
        }
        let converter = self.converter

        inputNode.installTap(onBus: 0, bufferSize: 4096, format: inputFormat) { buffer, _ in
            do {
                let converted = try converter.convertBuffer(buffer, to: analyzerFormat)
                continuation.yield(AnalyzerInput(buffer: converted))
            } catch {
                log("buffer convert failed: \(error.localizedDescription)")
            }
        }

        audioEngine.prepare()
        try audioEngine.start()
    }

    // MARK: permissions

    private func ensurePermissions() async -> Bool {
        // The modern SpeechAnalyzer path gates the model through AssetInventory,
        // not SFSpeechRecognizer authorization — so the only runtime permission
        // we request is the microphone. (Requesting speech-recognition auth from
        // a flat CLI additionally trips a TCC crash; it's unnecessary here.)
        let micGranted = await AVCaptureDevice.requestAccess(for: .audio)
        if !micGranted {
            log("microphone access not granted")
        }
        return micGranted
    }
}

// MARK: - Command loop (top-level)

let engine = DictationEngine()
emit(["type": "ready"])

// `--once` starts listening immediately and runs until stdin closes / Ctrl-C —
// handy for a manual smoke test straight from a terminal.
if CommandLine.arguments.contains("--once") {
    await engine.start()
}

do {
    for try await line in FileHandle.standardInput.bytes.lines {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { continue }

        guard
            let data = trimmed.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let cmd = obj["cmd"] as? String
        else {
            log("ignoring unparseable stdin line: \(trimmed)")
            continue
        }

        switch cmd {
        case "start": await engine.start()
        case "stop": await engine.stop()
        case "quit":
            await engine.stop()
            exit(0)
        default:
            log("unknown cmd: \(cmd)")
        }
    }
} catch {
    log("stdin loop error: \(error.localizedDescription)")
}

// stdin closed: shut down cleanly so the mic is released.
await engine.stop()
