import Foundation

struct VideoPacket: Equatable {
    static let headerLength = 56
    let streamID: UInt32
    let frameID: UInt64
    let keyframe: Bool
    let packetIndex: UInt16
    let packetCount: UInt16
    let payload: Data

    init(data: Data) throws {
        guard data.count >= Self.headerLength else { throw PostcardError.truncated }
        guard data[0..<4] == Data("PRVF".utf8), data[4] == 1, data.u16(at: 6) == Self.headerLength else { throw MeshError.remote("Invalid MeshRMM video packet.") }
        let index = data.u16(at: 48)
        let count = data.u16(at: 50)
        let payloadLength = Int(data.u16(at: 52))
        guard count > 0, index < count, data.count == Self.headerLength + payloadLength else { throw MeshError.remote("Invalid MeshRMM video fragment.") }
        streamID = data.u32(at: 8)
        frameID = data.u64(at: 16)
        keyframe = data[5] & 1 != 0
        packetIndex = index
        packetCount = count
        payload = data.suffix(payloadLength)
    }
}

struct EncodedVideoFrame: Equatable {
    let streamID: UInt32
    let frameID: UInt64
    let keyframe: Bool
    let data: Data
}

final class VideoReassembler {
    private var streamID: UInt32?
    private var frameID: UInt64?
    private var newestFrameID: UInt64?
    private var keyframe = false
    private var fragments: [Data?] = []
    private var totalBytes = 0
    private(set) var droppedFrames = 0

    func push(_ packet: VideoPacket) -> EncodedVideoFrame? {
        guard packet.packetCount <= 1024 else { return nil }
        if let streamID, streamID != packet.streamID {
            reset()
        }
        // The video data channel is intentionally unordered. Do not abandon an
        // incomplete recovery keyframe for a newer delta frame, and never let a
        // late fragment roll presentation back to an older frame.
        if let frameID,
           keyframe,
           !packet.keyframe,
           packet.frameID > frameID {
            return nil
        }
        if let newestFrameID,
           packet.frameID < newestFrameID || (frameID == nil && packet.frameID == newestFrameID) {
            return nil
        }
        if frameID != packet.frameID {
            if !fragments.isEmpty && fragments.contains(where: { $0 == nil }) { droppedFrames += 1 }
            streamID = packet.streamID
            frameID = packet.frameID
            newestFrameID = packet.frameID
            keyframe = packet.keyframe
            fragments = Array(repeating: nil, count: Int(packet.packetCount))
            totalBytes = 0
        }
        guard fragments.count == Int(packet.packetCount), fragments[Int(packet.packetIndex)] == nil else { return nil }
        fragments[Int(packet.packetIndex)] = packet.payload
        totalBytes += packet.payload.count
        guard totalBytes <= 8 * 1024 * 1024 else { reset(); return nil }
        guard fragments.allSatisfy({ $0 != nil }), let streamID, let frameID else { return nil }
        var bytes = Data(capacity: totalBytes)
        for fragment in fragments { bytes.append(fragment!) }
        let frame = EncodedVideoFrame(streamID: streamID, frameID: frameID, keyframe: keyframe, data: bytes)
        clearPending()
        return frame
    }

    func reset() {
        streamID = nil
        newestFrameID = nil
        clearPending()
    }

    private func clearPending() {
        frameID = nil; fragments = []; totalBytes = 0
    }
}

private extension Data {
    func u16(at offset: Int) -> UInt16 { self[offset..<(offset + 2)].reduce(0) { ($0 << 8) | UInt16($1) } }
    func u32(at offset: Int) -> UInt32 { self[offset..<(offset + 4)].reduce(0) { ($0 << 8) | UInt32($1) } }
    func u64(at offset: Int) -> UInt64 { self[offset..<(offset + 8)].reduce(0) { ($0 << 8) | UInt64($1) } }
}
