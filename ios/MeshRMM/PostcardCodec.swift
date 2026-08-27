import Foundation

struct RemoteDisplay: Equatable, Identifiable {
    let id: UInt32
    let name: String
    let x: Int32
    let y: Int32
    let width: UInt32
    let height: UInt32
    let primary: Bool
}

struct RemoteVideoFormat: Equatable {
    let width: UInt32
    let height: UInt32
    let framesPerSecond: UInt16
    let codec: UInt32
    let pixelFormat: UInt32
    let bitrateBitsPerSecond: UInt32
}

struct DisplayConfiguration: Equatable {
    let displays: [RemoteDisplay]
    let activeDisplayID: UInt32
    let streamID: UInt32
    let format: RemoteVideoFormat
}

enum ControlMessage: Equatable {
    case displayConfiguration(DisplayConfiguration)
    case cursorShape(UInt32)
    case clipboard(String)
    case stop(String)
    case ignored
}

enum PointerButton: UInt32 { case left = 0, right = 1, middle = 2 }

enum PostcardError: Error, Equatable {
    case truncated
    case overflow
    case invalidUTF8
    case unsupportedVariant(UInt64)
}

struct PostcardReader {
    let data: Data
    private(set) var offset = 0

    mutating func unsigned() throws -> UInt64 {
        var result: UInt64 = 0
        var shift: UInt64 = 0
        while true {
            guard offset < data.count else { throw PostcardError.truncated }
            let byte = data[offset]
            offset += 1
            guard shift < 64 else { throw PostcardError.overflow }
            result |= UInt64(byte & 0x7f) << shift
            if byte & 0x80 == 0 { return result }
            shift += 7
        }
    }

    mutating func signed32() throws -> Int32 {
        let encoded = try unsigned()
        guard encoded <= UInt64(UInt32.max) else { throw PostcardError.overflow }
        let value = UInt32(encoded)
        return Int32(bitPattern: (value >> 1) ^ (0 &- (value & 1)))
    }

    mutating func bool() throws -> Bool {
        guard offset < data.count else { throw PostcardError.truncated }
        defer { offset += 1 }
        return data[offset] != 0
    }

    mutating func string() throws -> String {
        let length = try Int(exactly: unsigned()).unwrap(or: PostcardError.overflow)
        guard data.count - offset >= length else { throw PostcardError.truncated }
        defer { offset += length }
        guard let value = String(data: data[offset..<(offset + length)], encoding: .utf8) else { throw PostcardError.invalidUTF8 }
        return value
    }

    mutating func decodeControlMessage() throws -> ControlMessage {
        let variant = try unsigned()
        switch variant {
        case 7:
            return .stop(try string())
        case 8:
            let displayCount = try Int(exactly: unsigned()).unwrap(or: PostcardError.overflow)
            var displays: [RemoteDisplay] = []
            displays.reserveCapacity(displayCount)
            for _ in 0..<displayCount {
                let id = try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow)
                let name = try string()
                let x = try signed32()
                let y = try signed32()
                let width = try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow)
                let height = try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow)
                displays.append(RemoteDisplay(id: id, name: name, x: x, y: y, width: width, height: height, primary: try bool()))
            }
            let active = try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow)
            let stream = try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow)
            let format = RemoteVideoFormat(
                width: try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow),
                height: try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow),
                framesPerSecond: try UInt16(exactly: unsigned()).unwrap(or: PostcardError.overflow),
                codec: try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow),
                pixelFormat: try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow),
                bitrateBitsPerSecond: try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow)
            )
            return .displayConfiguration(DisplayConfiguration(displays: displays, activeDisplayID: active, streamID: stream, format: format))
        case 11:
            return .cursorShape(try UInt32(exactly: unsigned()).unwrap(or: PostcardError.overflow))
        case 12:
            return .clipboard(try string())
        default:
            return .ignored
        }
    }
}

struct PostcardWriter {
    private(set) var data = Data()

    mutating func unsigned<T: BinaryInteger>(_ value: T) {
        var remaining = UInt64(value)
        repeat {
            var byte = UInt8(remaining & 0x7f)
            remaining >>= 7
            if remaining != 0 { byte |= 0x80 }
            data.append(byte)
        } while remaining != 0
    }

    mutating func signed<T: FixedWidthInteger & SignedInteger>(_ value: T) {
        let encoded = (UInt64(truncatingIfNeeded: value) << 1) ^ UInt64(bitPattern: Int64(value >> (T.bitWidth - 1)))
        unsigned(encoded)
    }

    mutating func bool(_ value: Bool) { data.append(value ? 1 : 0) }
    mutating func string(_ value: String) { let bytes = Data(value.utf8); unsigned(bytes.count); data.append(bytes) }

    static func viewerCapabilities() -> Data {
        var writer = Self()
        writer.unsigned(13) // SessionMessage::ViewerCapabilities
        writer.unsigned(1)  // one supported profile
        writer.unsigned(0)  // Codec::H264
        writer.unsigned(0)  // ChromaMode::Yuv420
        writer.unsigned(1)  // QualityPreset::Balanced
        writer.unsigned(0)  // ChromaMode::Yuv420
        return writer.data
    }

    static func requestKeyframe(streamID: UInt32) -> Data {
        var writer = Self(); writer.unsigned(4); writer.unsigned(streamID); return writer.data
    }

    static func selectDisplay(_ displayID: UInt32) -> Data {
        var writer = Self(); writer.unsigned(9); writer.unsigned(displayID); return writer.data
    }

    static func pointerMove(displayID: UInt32, x: UInt16, y: UInt16) -> Data {
        var writer = Self(); writer.unsigned(10); writer.unsigned(0); writer.unsigned(displayID); writer.unsigned(x); writer.unsigned(y); return writer.data
    }

    static func pointerButton(displayID: UInt32, x: UInt16, y: UInt16, button: PointerButton, pressed: Bool) -> Data {
        var writer = Self(); writer.unsigned(10); writer.unsigned(2); writer.unsigned(displayID); writer.unsigned(x); writer.unsigned(y); writer.unsigned(button.rawValue); writer.bool(pressed); return writer.data
    }

    static func wheel(displayID: UInt32, x: UInt16, y: UInt16, horizontal: Int16, vertical: Int16) -> Data {
        var writer = Self(); writer.unsigned(10); writer.unsigned(4); writer.unsigned(displayID); writer.unsigned(x); writer.unsigned(y); writer.signed(horizontal); writer.signed(vertical); return writer.data
    }

    static func key(displayID: UInt32, scanCode: UInt16, extended: Bool, pressed: Bool) -> Data {
        var writer = Self(); writer.unsigned(10); writer.unsigned(5); writer.unsigned(displayID); writer.unsigned(scanCode); writer.bool(extended); writer.bool(pressed); return writer.data
    }
}

private extension Optional {
    func unwrap(or error: Error) throws -> Wrapped {
        guard let self else { throw error }
        return self
    }
}
