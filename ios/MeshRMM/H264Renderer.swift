import AVFoundation
import CoreMedia
import Foundation

enum H264RenderResult {
    case enqueued
    case needsKeyframe(String)
}

final class H264Renderer {
    weak var layer: AVSampleBufferDisplayLayer?

    private var formatDescription: CMVideoFormatDescription?
    private var waitingForKeyframe = true

    var isReadyForMoreMediaData: Bool {
        layer?.isReadyForMoreMediaData == true
    }

    func attach(_ layer: AVSampleBufferDisplayLayer) {
        self.layer = layer
        layer.videoGravity = .resizeAspect
        layer.backgroundColor = CGColor(red: 0.02, green: 0.02, blue: 0.025, alpha: 1)
    }

    func reset() {
        formatDescription = nil
        waitingForKeyframe = true
        layer?.flushAndRemoveImage()
    }

    func recoverAtLiveEdge() {
        formatDescription = nil
        waitingForKeyframe = true
        // Preserve the last good desktop while discarding decoder work that
        // can no longer lead to a current image.
        layer?.flush()
    }

    func enqueue(_ frame: EncodedVideoFrame) -> H264RenderResult {
        if waitingForKeyframe && !frame.keyframe {
            return .needsKeyframe("The iOS decoder is waiting for a keyframe.")
        }
        do {
            guard let layer else {
                return .needsKeyframe("The iOS video surface is unavailable.")
            }
            if layer.requiresFlushToResumeDecoding || layer.status == .failed {
                let detail = layer.error?.localizedDescription ?? "The iOS H.264 decoder stopped."
                layer.flush()
                formatDescription = nil
                waitingForKeyframe = true
                return .needsKeyframe(detail)
            }
            let converted = try AnnexBAccessUnit(frame.data)
            if frame.keyframe, let sps = converted.sps, let pps = converted.pps {
                formatDescription = try Self.makeFormatDescription(sps: sps, pps: pps)
                waitingForKeyframe = false
            }
            guard let formatDescription else {
                waitingForKeyframe = true
                return .needsKeyframe("The H.264 keyframe did not include decoder parameters.")
            }
            let sample = try Self.makeSample(data: converted.lengthPrefixed, format: formatDescription)
            layer.enqueue(sample)
            return .enqueued
        } catch {
            waitingForKeyframe = true
            return .needsKeyframe(error.localizedDescription)
        }
    }

    private static func makeFormatDescription(sps: Data, pps: Data) throws -> CMVideoFormatDescription {
        var description: CMFormatDescription?
        let status: OSStatus = sps.withUnsafeBytes { spsBytes in
            pps.withUnsafeBytes { ppsBytes in
                guard let spsAddress = spsBytes.bindMemory(to: UInt8.self).baseAddress,
                      let ppsAddress = ppsBytes.bindMemory(to: UInt8.self).baseAddress else { return -1 }
                var pointers = [spsAddress, ppsAddress]
                var sizes = [sps.count, pps.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: 2,
                    parameterSetPointers: &pointers,
                    parameterSetSizes: &sizes,
                    nalUnitHeaderLength: 4,
                    formatDescriptionOut: &description
                )
            }
        }
        guard status == noErr, let description else {
            throw MeshError.remote("iOS rejected the remote H.264 parameter sets (\(status)).")
        }
        return description
    }

    private static func makeSample(data: Data, format: CMVideoFormatDescription) throws -> CMSampleBuffer {
        var blockBuffer: CMBlockBuffer?
        var status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: nil,
            blockLength: data.count,
            blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: data.count,
            flags: 0,
            blockBufferOut: &blockBuffer
        )
        guard status == kCMBlockBufferNoErr, let blockBuffer else { throw MeshError.remote("Could not allocate an iOS video buffer.") }
        status = data.withUnsafeBytes { bytes in
            guard let address = bytes.baseAddress else { return -1 }
            return CMBlockBufferReplaceDataBytes(with: address, blockBuffer: blockBuffer, offsetIntoDestination: 0, dataLength: data.count)
        }
        guard status == noErr else { throw MeshError.remote("Could not copy the remote video frame.") }
        var sampleBuffer: CMSampleBuffer?
        var sampleSize = data.count
        status = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: blockBuffer,
            formatDescription: format,
            sampleCount: 1,
            sampleTimingEntryCount: 0,
            sampleTimingArray: nil,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &sampleSize,
            sampleBufferOut: &sampleBuffer
        )
        guard status == noErr, let sampleBuffer else { throw MeshError.remote("Could not create an iOS video sample.") }
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: true) as? [NSMutableDictionary], let first = attachments.first {
            first[kCMSampleAttachmentKey_DisplayImmediately] = true
        }
        return sampleBuffer
    }
}

struct AnnexBAccessUnit {
    let lengthPrefixed: Data
    let sps: Data?
    let pps: Data?

    init(_ data: Data) throws {
        let units = Self.units(in: data)
        guard !units.isEmpty else { throw MeshError.remote("The remote H.264 frame contains no Annex-B NAL units.") }
        var output = Data(capacity: data.count)
        var foundSPS: Data?
        var foundPPS: Data?
        for unit in units {
            var length = UInt32(unit.count).bigEndian
            withUnsafeBytes(of: &length) { output.append(contentsOf: $0) }
            output.append(unit)
            guard let first = unit.first else { continue }
            if first & 0x1f == 7 { foundSPS = unit }
            if first & 0x1f == 8 { foundPPS = unit }
        }
        lengthPrefixed = output
        sps = foundSPS
        pps = foundPPS
    }

    private static func units(in data: Data) -> [Data] {
        let bytes = [UInt8](data)
        var starts: [(Int, Int)] = []
        var index = 0
        while index + 3 <= bytes.count {
            let prefix: Int
            if index + 4 <= bytes.count && Array(bytes[index..<(index + 4)]) == [0, 0, 0, 1] { prefix = 4 }
            else if Array(bytes[index..<(index + 3)]) == [0, 0, 1] { prefix = 3 }
            else { index += 1; continue }
            starts.append((index, prefix))
            index += prefix
        }
        return starts.enumerated().compactMap { position, start in
            let unitStart = start.0 + start.1
            var unitEnd = position + 1 < starts.count ? starts[position + 1].0 : bytes.count
            while unitEnd > unitStart && bytes[unitEnd - 1] == 0 { unitEnd -= 1 }
            return unitStart < unitEnd ? Data(bytes[unitStart..<unitEnd]) : nil
        }
    }
}
