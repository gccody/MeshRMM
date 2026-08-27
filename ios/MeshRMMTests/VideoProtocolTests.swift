import XCTest
@testable import MeshRMM

final class VideoProtocolTests: XCTestCase {
    func testParsesAndReassemblesVideoPackets() throws {
        let first = try VideoPacket(data: packet(frame: 42, index: 0, count: 2, keyframe: true, payload: [1, 2]))
        let second = try VideoPacket(data: packet(frame: 42, index: 1, count: 2, keyframe: true, payload: [3]))
        let reassembler = VideoReassembler()
        XCTAssertNil(reassembler.push(first))
        XCTAssertEqual(reassembler.push(second), EncodedVideoFrame(streamID: 7, frameID: 42, keyframe: true, data: Data([1, 2, 3])))
    }

    func testIncompleteKeyframeSurvivesNewerDeltaPacket() throws {
        let keyframeFirst = try VideoPacket(data: packet(frame: 42, index: 0, count: 2, keyframe: true, payload: [1]))
        let keyframeSecond = try VideoPacket(data: packet(frame: 42, index: 1, count: 2, keyframe: true, payload: [2]))
        let newerDelta = try VideoPacket(data: packet(frame: 43, index: 0, count: 1, keyframe: false, payload: [9]))
        let reassembler = VideoReassembler()

        XCTAssertNil(reassembler.push(keyframeFirst))
        XCTAssertNil(reassembler.push(newerDelta))
        XCTAssertEqual(
            reassembler.push(keyframeSecond),
            EncodedVideoFrame(streamID: 7, frameID: 42, keyframe: true, data: Data([1, 2]))
        )
    }

    func testLateFragmentCannotReplaceANewerCompletedFrame() throws {
        let oldFirst = try VideoPacket(data: packet(frame: 42, index: 0, count: 2, keyframe: false, payload: [1]))
        let oldSecond = try VideoPacket(data: packet(frame: 42, index: 1, count: 2, keyframe: false, payload: [2]))
        let newer = try VideoPacket(data: packet(frame: 43, index: 0, count: 1, keyframe: true, payload: [9]))
        let reassembler = VideoReassembler()

        XCTAssertNil(reassembler.push(oldFirst))
        XCTAssertEqual(
            reassembler.push(newer),
            EncodedVideoFrame(streamID: 7, frameID: 43, keyframe: true, data: Data([9]))
        )
        XCTAssertNil(reassembler.push(oldSecond))
        XCTAssertEqual(reassembler.droppedFrames, 1)
    }

    func testAnnexBConversionExtractsParameterSets() throws {
        let unit = try AnnexBAccessUnit(Data([0, 0, 0, 1, 0x67, 0x64, 0, 0x1f, 0, 0, 1, 0x68, 0xee, 0, 0, 1, 0x65, 0xaa]))
        XCTAssertEqual(unit.sps, Data([0x67, 0x64, 0, 0x1f]))
        XCTAssertEqual(unit.pps, Data([0x68, 0xee]))
        XCTAssertEqual(unit.lengthPrefixed, Data([0, 0, 0, 4, 0x67, 0x64, 0, 0x1f, 0, 0, 0, 2, 0x68, 0xee, 0, 0, 0, 2, 0x65, 0xaa]))
    }

    private func packet(frame: UInt64, index: UInt16, count: UInt16, keyframe: Bool, payload: [UInt8]) -> Data {
        var data = Data("PRVF".utf8)
        data.append(1); data.append(keyframe ? 1 : 0)
        append(UInt16(56), to: &data); append(UInt32(7), to: &data); append(UInt32(0), to: &data)
        append(frame, to: &data); append(UInt64(1), to: &data); append(UInt64(2), to: &data); append(UInt64(3), to: &data)
        append(index, to: &data); append(count, to: &data); append(UInt16(payload.count), to: &data); append(UInt16(0), to: &data)
        data.append(contentsOf: payload)
        return data
    }

    private func append<T: FixedWidthInteger>(_ value: T, to data: inout Data) {
        var value = value.bigEndian
        withUnsafeBytes(of: &value) { data.append(contentsOf: $0) }
    }
}
