@preconcurrency import AVFoundation
import Foundation
import os

/// Converts a captured mic buffer to the exact format SpeechAnalyzer wants.
///
/// The input node runs at the device's native rate/format; SpeechAnalyzer
/// advertises its preferred format via `bestAvailableAudioFormat`. This bridges
/// the two with a reused `AVAudioConverter`. Lifted from Apple's WWDC25 sample /
/// FluidInference's swift-scribe reference.
final class BufferConverter {
    enum ConvertError: Swift.Error {
        case failedToCreateConverter
        case failedToCreateConversionBuffer
        case conversionFailed(NSError?)
    }

    private var converter: AVAudioConverter?

    func convertBuffer(_ buffer: AVAudioPCMBuffer, to format: AVAudioFormat) throws -> AVAudioPCMBuffer {
        let inputFormat = buffer.format
        guard inputFormat != format else {
            return buffer
        }

        if converter == nil || converter?.outputFormat != format {
            converter = AVAudioConverter(from: inputFormat, to: format)
            // Avoid timestamp drift from priming at the cost of the first samples.
            converter?.primeMethod = .none
        }

        guard let converter else {
            throw ConvertError.failedToCreateConverter
        }

        let sampleRateRatio = converter.outputFormat.sampleRate / converter.inputFormat.sampleRate
        let scaledInputFrameLength = Double(buffer.frameLength) * sampleRateRatio
        let frameCapacity = AVAudioFrameCount(scaledInputFrameLength.rounded(.up))
        guard
            let conversionBuffer = AVAudioPCMBuffer(
                pcmFormat: converter.outputFormat, frameCapacity: frameCapacity)
        else {
            throw ConvertError.failedToCreateConversionBuffer
        }

        var nsError: NSError?
        let bufferProcessedLock = OSAllocatedUnfairLock(initialState: false)

        let status = converter.convert(to: conversionBuffer, error: &nsError) {
            _, inputStatusPointer in
            let wasProcessed = bufferProcessedLock.withLock { bufferProcessed -> Bool in
                let wasProcessed = bufferProcessed
                bufferProcessed = true
                return wasProcessed
            }
            inputStatusPointer.pointee = wasProcessed ? .noDataNow : .haveData
            return wasProcessed ? nil : buffer
        }

        guard status != .error else {
            throw ConvertError.conversionFailed(nsError)
        }

        return conversionBuffer
    }
}
