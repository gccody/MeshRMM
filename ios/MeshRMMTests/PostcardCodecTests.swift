import XCTest
@testable import MeshRMM

final class PostcardCodecTests: XCTestCase {
    @MainActor
    func testRemoteSurfaceUsesReadablePortraitZoomAndLocalCursor() {
        let surface = RemoteSurfaceUIView(controller: RemoteSessionController())
        surface.frame = CGRect(x: 0, y: 0, width: 390, height: 844)
        surface.configure(remoteSize: CGSize(width: 1_920, height: 1_080), displayID: 7)
        surface.layoutIfNeeded()

        XCTAssertEqual(surface.sampleLayer.frame.width, 936, accuracy: 0.01)
        XCTAssertEqual(surface.sampleLayer.frame.height, 526.5, accuracy: 0.01)
        XCTAssertEqual(surface.sampleLayer.frame.minX, -273, accuracy: 0.01)
        XCTAssertEqual(surface.sampleLayer.frame.minY, 158.75, accuracy: 0.01)

        let cursor = surface.layer.sublayers?.compactMap { $0 as? CAShapeLayer }.first
        XCTAssertNotNil(cursor?.path)
        XCTAssertEqual(cursor?.isHidden, false)
        XCTAssertEqual(cursor?.position.x ?? 0, 195, accuracy: 0.01)
        XCTAssertEqual(cursor?.position.y ?? 0, 422, accuracy: 0.01)
    }

    func testTrackpadPointerMovesRelativelyAndClampsToDisplay() {
        var pointer = TrackpadPointer()
        let moved = pointer.move(
            by: CGPoint(x: 100, y: -50),
            in: CGSize(width: 1_000, height: 500),
            sensitivity: 1
        )
        XCTAssertEqual(moved.0, UInt16((0.6 * Double(UInt16.max)).rounded()))
        XCTAssertEqual(moved.1, UInt16((0.4 * Double(UInt16.max)).rounded()))

        let clamped = pointer.move(
            by: CGPoint(x: 10_000, y: -10_000),
            in: CGSize(width: 1_000, height: 500),
            sensitivity: 1
        )
        XCTAssertEqual(clamped.0, UInt16.max)
        XCTAssertEqual(clamped.1, 0)
    }

    func testViewerCapabilitiesMatchesRustPostcardFixture() {
        XCTAssertEqual(PostcardWriter.viewerCapabilities(), Data([13, 1, 0, 0, 1, 0]))
        XCTAssertEqual(PostcardWriter.requestKeyframe(streamID: 300), Data([4, 0xac, 0x02]))
    }

    func testDecodesRustDisplayConfigurationFixture() throws {
        let fixture = Data([
            8, 1, 2, 4, 76, 101, 102, 116, 0xff, 0x27, 0xe7, 0x02, 0x80, 0x14,
            0xa0, 0x0b, 0, 2, 9, 0x80, 0x14, 0xa0, 0x0b, 60, 0, 0, 0x80, 0xb6, 0xdc, 0x05,
        ])
        var reader = PostcardReader(data: fixture)
        let expected = DisplayConfiguration(
            displays: [RemoteDisplay(id: 2, name: "Left", x: -2560, y: -180, width: 2560, height: 1440, primary: false)],
            activeDisplayID: 2,
            streamID: 9,
            format: RemoteVideoFormat(width: 2560, height: 1440, framesPerSecond: 60, codec: 0, pixelFormat: 0, bitrateBitsPerSecond: 12_000_000)
        )
        XCTAssertEqual(try reader.decodeControlMessage(), .displayConfiguration(expected))
    }

    func testPositionedInputEncoding() {
        XCTAssertEqual(
            PostcardWriter.pointerButton(displayID: 7, x: 65535, y: 0, button: .left, pressed: true),
            Data([10, 2, 7, 0xff, 0xff, 0x03, 0, 0, 1])
        )
        XCTAssertEqual(
            PostcardWriter.wheel(displayID: 1, x: 2, y: 3, horizontal: -1, vertical: 120),
            Data([10, 4, 1, 2, 3, 1, 0xf0, 0x01])
        )
    }
}
