import AVFoundation
import Foundation
import UIKit
import WebRTC

struct SignalMessage: Codable {
    let type: String
    var sdp: String?
    var candidate: String?
    var sdpMid: String?
    var sdpMLineIndex: Int32?
    var usernameFragment: String?
    var message: String?

    enum CodingKeys: String, CodingKey {
        case type, sdp, candidate, message
        case sdpMid = "sdp_mid"
        case sdpMLineIndex = "sdp_mline_index"
        case usernameFragment = "username_fragment"
    }
}

final class RemoteSessionController: NSObject, ObservableObject, @unchecked Sendable {
    @Published private(set) var status = "Authorizing remote session…"
    @Published private(set) var configuration: DisplayConfiguration?
    @Published private(set) var isConnected = false
    @Published var errorMessage: String?

    let renderer = H264Renderer()
    weak var inputView: RemoteSurfaceUIView?

    private static let factory: RTCPeerConnectionFactory = {
        RTCInitializeSSL()
        return RTCPeerConnectionFactory(encoderFactory: RTCDefaultVideoEncoderFactory(), decoderFactory: RTCDefaultVideoDecoderFactory())
    }()

    private var serverURL: URL?
    private var bootstrap: SessionBootstrap?
    private var sessionProvider: (@MainActor () async throws -> (URL, SessionBootstrap))?
    private var resumeProvider: (@MainActor (URL, SessionBootstrap) async throws -> (URL, SessionBootstrap))?
    private var reconnectInProgress = false
    private var reconnectRequested = false
    private var peer: RTCPeerConnection?
    private var signalingSession: URLSession?
    private var socket: URLSessionWebSocketTask?
    private var socketOpened = false
    private var stopping = false
    private var controlChannel: RTCDataChannel?
    private var videoChannel: RTCDataChannel?
    private var receiveTask: Task<Void, Never>?
    private var heartbeatTask: Task<Void, Never>?
    private var negotiationTask: Task<Void, Never>?
    private var videoWatchdogTask: Task<Void, Never>?
    private let videoQueue = DispatchQueue(label: "com.meshrmm.ios.video-receive", qos: .userInteractive)
    private var reassembler = VideoReassembler()
    private var capabilitiesSent = false
    private var lastFrameID: UInt64?
    private var waitingForKeyframe = true
    private var lastKeyframeRequestNanoseconds: UInt64 = 0
    private var activeDisplayID: UInt32?
    private var activeDisplayName = "display"
    private var streamID: UInt32?
    private var videoStreamID: UInt32?
    private var connectionStartedNanoseconds: UInt64 = 0
    private var lastVideoPacketNanoseconds: UInt64 = 0
    private var videoStallRecoveryAttempts = 0

    private let presentationLock = NSLock()
    private var presentationFrames: [EncodedVideoFrame] = []
    private var presentationScheduled = false
    private var presentationRecovering = true
    // Match the desktop viewer's 250 ms burst allowance at 60 FPS. This is
    // bounded tightly enough to prevent long-term drift without treating the
    // decoder's normal startup warm-up as congestion.
    private static let maximumPresentationFrames = 15
    // An unreliable video channel can abandon one fragment without closing.
    // Request recovery immediately, but avoid turning sustained loss into an
    // IDR storm that congests SCTP and eventually starves the stream entirely.
    private static let keyframeRetryNanoseconds: UInt64 = 2_000_000_000
    private static let videoStallNanoseconds: UInt64 = 2_000_000_000
    private static let connectionStallNanoseconds: UInt64 = 10_000_000_000

    @MainActor
    func configureSessionProvider(
        _ provider: @escaping @MainActor () async throws -> (URL, SessionBootstrap),
        resume: @escaping @MainActor (URL, SessionBootstrap) async throws -> (URL, SessionBootstrap)
    ) {
        sessionProvider = provider
        resumeProvider = resume
    }

    @MainActor
    func connect() async {
        await connect(resuming: false)
    }

    @MainActor
    private func connect(resuming: Bool) async {
        guard peer == nil, !reconnectInProgress, let sessionProvider else { return }
        reconnectInProgress = true
        defer { reconnectInProgress = false }
        var lastError: Error?
        let attemptCount = resuming ? 6 : 5
        for attempt in 0..<attemptCount {
            guard self.sessionProvider != nil else { return }
            do {
                let result: (URL, SessionBootstrap)
                if resuming,
                   attempt < 3,
                   let serverURL,
                   let bootstrap,
                   let resumeProvider {
                    result = try await resumeProvider(serverURL, bootstrap)
                } else {
                    result = try await sessionProvider()
                }
                guard self.sessionProvider != nil else { return }
                await start(apiURL: result.0, bootstrap: result.1)
                return
            } catch {
                lastError = error
                guard attempt + 1 < attemptCount else { break }
                status = "Retrying remote session…"
                let delaySeconds = min(1 << attempt, 4)
                try? await Task.sleep(nanoseconds: UInt64(delaySeconds) * 1_000_000_000)
            }
        }
        errorMessage = lastError?.localizedDescription ?? "The remote session could not be created."
    }

    @MainActor
    func disconnect() {
        sessionProvider = nil
        resumeProvider = nil
        reconnectRequested = false
        stop()
    }

    @MainActor
    func start(apiURL: URL, bootstrap: SessionBootstrap) async {
        guard peer == nil else { return }
        serverURL = apiURL
        self.bootstrap = bootstrap
        stopping = false
        socketOpened = false
        reconnectRequested = false
        status = "Opening encrypted connection…"

        let rtcConfiguration = RTCConfiguration()
        rtcConfiguration.iceServers = bootstrap.iceServers.map {
            RTCIceServer(urlStrings: $0.urls, username: $0.username, credential: $0.credential)
        }
        rtcConfiguration.sdpSemantics = .unifiedPlan
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: ["DtlsSrtpKeyAgreement": "true"])
        guard let peer = Self.factory.peerConnection(with: rtcConfiguration, constraints: constraints, delegate: self) else {
            fail("iOS could not create a WebRTC peer connection.")
            return
        }
        self.peer = peer

        var components = URLComponents(url: apiURL, resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        components.path = "/v1/remote/sessions/\(bootstrap.sessionID)/signal"
        components.queryItems = [URLQueryItem(name: "role", value: "client")]
        guard let signalURL = components.url else { fail("The signaling URL is invalid."); return }
        var request = URLRequest(url: signalURL)
        request.setValue("Bearer \(bootstrap.signalingToken)", forHTTPHeaderField: "Authorization")
        let signalingSession = URLSession(configuration: .default, delegate: self, delegateQueue: nil)
        let socket = signalingSession.webSocketTask(with: request)
        self.signalingSession = signalingSession
        self.socket = socket
        socket.resume()
        receiveTask = Task { [weak self] in await self?.receiveSignals() }
        videoWatchdogTask = Task { [weak self] in await self?.monitorVideoLiveness() }
        videoQueue.async { [weak self] in
            self?.connectionStartedNanoseconds = DispatchTime.now().uptimeNanoseconds
        }
    }

    func stop() {
        stopTransport(endSession: true)
    }

    private func stopForReconnect() {
        stopTransport(endSession: false)
    }

    private func stopTransport(endSession: Bool) {
        guard !stopping else { return }
        stopping = true
        receiveTask?.cancel()
        heartbeatTask?.cancel()
        negotiationTask?.cancel()
        videoWatchdogTask?.cancel()
        controlChannel?.close()
        videoChannel?.close()
        peer?.close()
        finishSignalingSession(endSession: endSession)
        socket = nil; signalingSession = nil; peer = nil; controlChannel = nil; videoChannel = nil
        videoQueue.async { [weak self] in
            self?.videoStreamID = nil
            self?.connectionStartedNanoseconds = 0
            self?.lastFrameID = nil
            self?.waitingForKeyframe = true
            self?.lastVideoPacketNanoseconds = 0
            self?.videoStallRecoveryAttempts = 0
            self?.reassembler.reset()
        }
        resetPresentation()
        DispatchQueue.main.async { [weak self] in
            self?.renderer.reset()
            self?.isConnected = false
        }
    }

    func selectDisplay(_ id: UInt32) {
        sendControl(PostcardWriter.selectDisplay(id))
        DispatchQueue.main.async { [weak self] in self?.status = "Switching display…" }
    }

    func showKeyboard() {
        inputView?.becomeFirstResponder()
    }

    func sendPointerMove(x: UInt16, y: UInt16) {
        guard let activeDisplayID else { return }
        sendControl(PostcardWriter.pointerMove(displayID: activeDisplayID, x: x, y: y))
    }

    func sendButton(x: UInt16, y: UInt16, pressed: Bool, button: PointerButton = .left) {
        guard let activeDisplayID else { return }
        sendControl(PostcardWriter.pointerButton(displayID: activeDisplayID, x: x, y: y, button: button, pressed: pressed))
    }

    func sendWheel(x: UInt16, y: UInt16, horizontal: Int16, vertical: Int16) {
        guard let activeDisplayID else { return }
        sendControl(PostcardWriter.wheel(displayID: activeDisplayID, x: x, y: y, horizontal: horizontal, vertical: vertical))
    }

    func sendText(_ text: String) {
        guard let activeDisplayID else { return }
        for character in text {
            guard let key = WindowsScanCodes.key(for: character) else { continue }
            if key.shift { sendKey(displayID: activeDisplayID, scanCode: 0x2a, extended: false, pressed: true) }
            sendKey(displayID: activeDisplayID, scanCode: key.code, extended: key.extended, pressed: true)
            sendKey(displayID: activeDisplayID, scanCode: key.code, extended: key.extended, pressed: false)
            if key.shift { sendKey(displayID: activeDisplayID, scanCode: 0x2a, extended: false, pressed: false) }
        }
    }

    func sendBackspace() {
        guard let activeDisplayID else { return }
        sendKey(displayID: activeDisplayID, scanCode: 0x0e, extended: false, pressed: true)
        sendKey(displayID: activeDisplayID, scanCode: 0x0e, extended: false, pressed: false)
    }

    private func sendKey(displayID: UInt32, scanCode: UInt16, extended: Bool, pressed: Bool) {
        sendControl(PostcardWriter.key(displayID: displayID, scanCode: scanCode, extended: extended, pressed: pressed))
    }

    private func receiveSignals() async {
        guard let socket else { return }
        do {
            while !Task.isCancelled {
                let received = try await socket.receive()
                guard case .string(let text) = received, let data = text.data(using: .utf8) else { continue }
                let signal = try JSONDecoder().decode(SignalMessage.self, from: data)
                try await handle(signal)
            }
        } catch {
            if !Task.isCancelled { fail("The signaling connection ended: \(error.localizedDescription)") }
        }
    }

    private func handle(_ signal: SignalMessage) async throws {
        guard let peer else { throw MeshError.remote("The WebRTC peer is unavailable.") }
        switch signal.type {
        case "ready":
            // The relay does not retain readiness messages. If our initial
            // message raced ahead of the Agent socket, its readiness message
            // proves that retrying now will reach it.
            sendSignal(SignalMessage(type: "ready"))
        case "offer":
            guard let sdp = signal.sdp else { throw MeshError.invalidResponse }
            negotiationTask?.cancel()
            try await peer.setRemote(RTCSessionDescription(type: .offer, sdp: sdp))
            let answer = try await peer.answer(for: RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil))
            try await peer.setLocal(answer)
            sendSignal(SignalMessage(type: "answer", sdp: answer.sdp))
        case "ice_candidate":
            guard let candidate = signal.candidate else { return }
            try await peer.add(RTCIceCandidate(sdp: candidate, sdpMLineIndex: signal.sdpMLineIndex ?? 0, sdpMid: signal.sdpMid))
        case "peer_left": fail("The remote agent ended the session.")
        case "error":
            let message = signal.message ?? "The remote session failed."
            // Agents released before reconnect-aware teardown report the old
            // control channel closing after a successful resume. The video
            // watchdog owns recovery, so this legacy message is not terminal.
            if message != "remote input/control channel closed unexpectedly" {
                fail(message)
            }
        default: break
        }
    }

    private func retryNegotiation() async {
        while !Task.isCancelled {
            try? await Task.sleep(nanoseconds: 2_000_000_000)
            guard !Task.isCancelled else { return }
            sendSignal(SignalMessage(type: "ready"))
        }
    }

    private func heartbeat() async {
        while !Task.isCancelled {
            try? await Task.sleep(nanoseconds: 20_000_000_000)
            guard !Task.isCancelled else { return }
            socket?.sendPing { [weak self] error in if let error { self?.fail("Signaling heartbeat failed: \(error.localizedDescription)") } }
            sendSignal(SignalMessage(type: "activity"))
        }
    }

    private func sendSignal(_ signal: SignalMessage) {
        guard let socket, let data = try? JSONEncoder().encode(signal), let text = String(data: data, encoding: .utf8) else { return }
        socket.send(.string(text)) { [weak self] error in
            if let error, self?.stopping == false {
                self?.fail("Could not send a signaling message: \(error.localizedDescription)")
            }
        }
    }

    private func finishSignalingSession(endSession: Bool) {
        guard let socket, let signalingSession else { return }
        guard endSession,
              socketOpened,
              let data = try? JSONEncoder().encode(SignalMessage(type: "end_session")),
              let text = String(data: data, encoding: .utf8) else {
            socket.cancel(with: .normalClosure, reason: nil)
            signalingSession.invalidateAndCancel()
            return
        }
        socket.send(.string(text)) { _ in
            socket.cancel(with: .normalClosure, reason: nil)
            signalingSession.invalidateAndCancel()
        }
    }

    private func sendControl(_ data: Data) {
        guard controlChannel?.readyState == .open else { return }
        controlChannel?.sendData(RTCDataBuffer(data: data, isBinary: true))
    }

    private func handleControl(_ data: Data) {
        do {
            var reader = PostcardReader(data: data)
            switch try reader.decodeControlMessage() {
            case .displayConfiguration(let next):
                if !capabilitiesSent {
                    capabilitiesSent = true
                    sendControl(PostcardWriter.viewerCapabilities())
                    return
                }
                configuration = next
                activeDisplayID = next.activeDisplayID
                activeDisplayName = next.displays.first(where: { $0.id == next.activeDisplayID })?.name ?? "display"
                streamID = next.streamID
                renderer.reset()
                resetPresentation()
                videoQueue.async { [weak self] in
                    guard let self else { return }
                    self.videoStreamID = next.streamID
                    self.connectionStartedNanoseconds = 0
                    self.lastFrameID = nil
                    self.waitingForKeyframe = true
                    self.lastKeyframeRequestNanoseconds = 0
                    self.lastVideoPacketNanoseconds = DispatchTime.now().uptimeNanoseconds
                    self.videoStallRecoveryAttempts = 0
                    self.reassembler.reset()
                }
                status = "Starting video from \(activeDisplayName)…"
                isConnected = false
                sendControl(PostcardWriter.requestKeyframe(streamID: next.streamID))
            case .clipboard(let text): UIPasteboard.general.string = text
            case .stop(let reason): fail(reason)
            case .cursorShape, .ignored: break
            }
        } catch {
            fail("The agent sent an invalid control message.")
        }
    }

    private func handleVideo(_ data: Data) {
        lastVideoPacketNanoseconds = DispatchTime.now().uptimeNanoseconds
        videoStallRecoveryAttempts = 0
        guard let packet = try? VideoPacket(data: data), packet.streamID == videoStreamID else { return }
        let droppedBefore = reassembler.droppedFrames
        guard let frame = reassembler.push(packet) else {
            if reassembler.droppedFrames > droppedBefore {
                requestVideoKeyframe(streamID: packet.streamID)
            }
            return
        }
        if let lastFrameID, frame.frameID != lastFrameID &+ 1, !frame.keyframe { waitingForKeyframe = true }
        if waitingForKeyframe && !frame.keyframe {
            requestVideoKeyframe(streamID: packet.streamID)
            return
        }
        if frame.keyframe {
            waitingForKeyframe = false
            lastKeyframeRequestNanoseconds = 0
        }
        lastFrameID = frame.frameID
        publishForPresentation(frame)
    }

    private func monitorVideoLiveness() async {
        while !Task.isCancelled {
            try? await Task.sleep(nanoseconds: 1_000_000_000)
            guard !Task.isCancelled else { return }
            videoQueue.async { [weak self] in self?.recoverStalledVideoIfNeeded() }
        }
    }

    private func recoverStalledVideoIfNeeded() {
        guard !stopping else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        guard let videoStreamID else {
            if connectionStartedNanoseconds != 0,
               now &- connectionStartedNanoseconds >= Self.connectionStallNanoseconds {
                requestSessionReconnect()
            }
            return
        }
        guard lastVideoPacketNanoseconds != 0 else { return }
        let stalledFor = now &- lastVideoPacketNanoseconds
        guard stalledFor >= Self.videoStallNanoseconds else { return }
        if videoStallRecoveryAttempts >= 2,
           stalledFor >= Self.videoStallNanoseconds * 3 {
            requestSessionReconnect()
            return
        }
        if requestVideoKeyframe(streamID: videoStreamID) {
            videoStallRecoveryAttempts += 1
        }
    }

    @discardableResult
    private func requestVideoKeyframe(streamID: UInt32) -> Bool {
        let wasWaitingForKeyframe = waitingForKeyframe
        waitingForKeyframe = true
        let now = DispatchTime.now().uptimeNanoseconds
        guard lastKeyframeRequestNanoseconds == 0
                || now &- lastKeyframeRequestNanoseconds >= Self.keyframeRetryNanoseconds else { return false }
        lastKeyframeRequestNanoseconds = now
        DispatchQueue.main.async { [weak self] in
            if !wasWaitingForKeyframe {
                self?.beginPresentationRecovery()
                self?.renderer.recoverAtLiveEdge()
            }
            self?.sendControl(PostcardWriter.requestKeyframe(streamID: streamID))
        }
        return true
    }

    private func requestSessionReconnect() {
        guard !reconnectRequested else { return }
        reconnectRequested = true
        DispatchQueue.main.async { [weak self] in
            guard let self, self.sessionProvider != nil else { return }
            self.status = "Reconnecting live video…"
            self.isConnected = false
            self.stopForReconnect()
            Task { @MainActor [weak self] in
                try? await Task.sleep(nanoseconds: 500_000_000)
                await self?.connect(resuming: true)
            }
        }
    }

    private func publishForPresentation(_ frame: EncodedVideoFrame) {
        var shouldSchedule = false
        var shouldRequestKeyframe = false

        presentationLock.lock()
        if presentationRecovering && !frame.keyframe {
            presentationLock.unlock()
            return
        }
        if presentationFrames.count >= Self.maximumPresentationFrames {
            presentationFrames.removeAll(keepingCapacity: true)
            shouldRequestKeyframe = !presentationRecovering
            presentationRecovering = true
            if !frame.keyframe {
                presentationLock.unlock()
                if shouldRequestKeyframe { requestVideoKeyframe(streamID: frame.streamID) }
                return
            }
        }
        if frame.keyframe {
            presentationFrames.removeAll(keepingCapacity: true)
            presentationRecovering = false
        }
        presentationFrames.append(frame)
        if !presentationScheduled {
            presentationScheduled = true
            shouldSchedule = true
        }
        presentationLock.unlock()

        if shouldRequestKeyframe { requestVideoKeyframe(streamID: frame.streamID) }
        if shouldSchedule {
            DispatchQueue.main.async { [weak self] in self?.drainPresentationFrame() }
        }
    }

    private func drainPresentationFrame() {
        presentationLock.lock()
        guard !presentationFrames.isEmpty else {
            presentationScheduled = false
            presentationLock.unlock()
            return
        }
        presentationLock.unlock()

        // AVFoundation can report temporary backpressure while its hardware
        // decoder starts or drains. Wait briefly without feeding it more data;
        // the bounded mailbox will trigger live-edge recovery if this persists.
        guard renderer.isReadyForMoreMediaData else {
            DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(4)) { [weak self] in
                self?.drainPresentationFrame()
            }
            return
        }

        presentationLock.lock()
        let frame = presentationFrames.isEmpty ? nil : presentationFrames.removeFirst()
        presentationLock.unlock()

        var recovery: (UInt32, String)?
        if let frame {
            switch renderer.enqueue(frame) {
            case .enqueued:
                let connectedStatus = "Connected to \(activeDisplayName)"
                if status != connectedStatus { status = connectedStatus }
                if !isConnected { isConnected = true }
            case .needsKeyframe(let reason):
                recovery = (frame.streamID, reason)
            }
        }

        presentationLock.lock()
        if recovery != nil {
            presentationFrames.removeAll(keepingCapacity: true)
            presentationRecovering = true
        }
        presentationScheduled = false
        let shouldContinue = !presentationFrames.isEmpty
        if shouldContinue { presentationScheduled = true }
        presentationLock.unlock()

        if let (streamID, _) = recovery {
            videoQueue.async { [weak self] in self?.requestVideoKeyframe(streamID: streamID) }
        }
        if shouldContinue {
            DispatchQueue.main.async { [weak self] in self?.drainPresentationFrame() }
        }
    }

    private func resetPresentation() {
        presentationLock.lock()
        presentationFrames.removeAll(keepingCapacity: true)
        presentationRecovering = true
        presentationLock.unlock()
    }

    private func beginPresentationRecovery() {
        presentationLock.lock()
        presentationFrames.removeAll(keepingCapacity: true)
        presentationRecovering = true
        presentationLock.unlock()
        if isConnected { status = "Recovering live video…" }
    }

    private func fail(_ message: String) {
        DispatchQueue.main.async { [weak self] in
            self?.errorMessage = message
            self?.status = "Session ended"
            self?.isConnected = false
        }
    }
}

extension RemoteSessionController: URLSessionWebSocketDelegate {
    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didOpenWithProtocol protocol: String?
    ) {
        guard webSocketTask === socket, !stopping else { return }
        socketOpened = true
        DispatchQueue.main.async { [weak self] in self?.status = "Waiting for remote agent…" }
        sendSignal(SignalMessage(type: "ready"))
        sendSignal(SignalMessage(type: "activity"))
        negotiationTask = Task { [weak self] in await self?.retryNegotiation() }
        heartbeatTask = Task { [weak self] in await self?.heartbeat() }
    }

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didCloseWith closeCode: URLSessionWebSocketTask.CloseCode,
        reason: Data?
    ) {
        socketOpened = false
        guard !stopping else { return }
        let detail = reason.flatMap { String(data: $0, encoding: .utf8) }
        fail(detail.map { "The signaling connection closed: \($0)" } ?? "The signaling connection closed unexpectedly.")
    }
}

extension RemoteSessionController: RTCPeerConnectionDelegate {
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCPeerConnectionState) {
        if newState == .connected { negotiationTask?.cancel() }
        if newState == .failed { fail("The encrypted WebRTC connection failed.") }
    }
    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {
        sendSignal(SignalMessage(type: "ice_candidate", candidate: candidate.sdp, sdpMid: candidate.sdpMid, sdpMLineIndex: candidate.sdpMLineIndex))
    }
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {
        dataChannel.delegate = self
        if dataChannel.label == "meshrmm-control-v4" { controlChannel = dataChannel }
        if dataChannel.label == "meshrmm-video-v1" { videoChannel = dataChannel }
    }
}

extension RemoteSessionController: RTCDataChannelDelegate {
    func dataChannelDidChangeState(_ dataChannel: RTCDataChannel) {
        if dataChannel.label == "meshrmm-control-v4", dataChannel.readyState == .open {
            DispatchQueue.main.async { [weak self] in self?.status = "Negotiating video…" }
        }
    }

    func dataChannel(_ dataChannel: RTCDataChannel, didReceiveMessageWith buffer: RTCDataBuffer) {
        if dataChannel.label == "meshrmm-control-v4" {
            DispatchQueue.main.async { [weak self] in self?.handleControl(buffer.data) }
        } else if dataChannel.label == "meshrmm-video-v1" {
            videoQueue.async { [weak self] in self?.handleVideo(buffer.data) }
        }
    }
}

private extension RTCPeerConnection {
    func setRemote(_ description: RTCSessionDescription) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            setRemoteDescription(description) { error in
                if let error { continuation.resume(throwing: error) } else { continuation.resume() }
            }
        }
    }

    func setLocal(_ description: RTCSessionDescription) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            setLocalDescription(description) { error in
                if let error { continuation.resume(throwing: error) } else { continuation.resume() }
            }
        }
    }

    func answer(for constraints: RTCMediaConstraints) async throws -> RTCSessionDescription {
        try await withCheckedThrowingContinuation { continuation in
            answer(for: constraints) { description, error in
                if let error { continuation.resume(throwing: error) }
                else if let description { continuation.resume(returning: description) }
                else { continuation.resume(throwing: MeshError.remote("WebRTC did not create an answer.")) }
            }
        }
    }
}

private enum WindowsScanCodes {
    struct Key { let code: UInt16; let shift: Bool; var extended = false }
    private static let base: [Character: UInt16] = [
        "1": 0x02, "2": 0x03, "3": 0x04, "4": 0x05, "5": 0x06, "6": 0x07, "7": 0x08, "8": 0x09, "9": 0x0a, "0": 0x0b,
        "q": 0x10, "w": 0x11, "e": 0x12, "r": 0x13, "t": 0x14, "y": 0x15, "u": 0x16, "i": 0x17, "o": 0x18, "p": 0x19,
        "a": 0x1e, "s": 0x1f, "d": 0x20, "f": 0x21, "g": 0x22, "h": 0x23, "j": 0x24, "k": 0x25, "l": 0x26,
        "z": 0x2c, "x": 0x2d, "c": 0x2e, "v": 0x2f, "b": 0x30, "n": 0x31, "m": 0x32,
        "-": 0x0c, "=": 0x0d, "[": 0x1a, "]": 0x1b, ";": 0x27, "'": 0x28, "`": 0x29, "\\": 0x2b, ",": 0x33, ".": 0x34, "/": 0x35, " ": 0x39, "\n": 0x1c, "\t": 0x0f,
    ]
    private static let shifted: [Character: Character] = [
        "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7", "*": "8", "(": "9", ")": "0",
        "_": "-", "+": "=", "{": "[", "}": "]", ":": ";", "\"": "'", "~": "`", "|": "\\", "<": ",", ">": ".", "?": "/",
    ]

    static func key(for character: Character) -> Key? {
        if let unshifted = shifted[character], let code = base[unshifted] { return Key(code: code, shift: true) }
        let lower = Character(String(character).lowercased())
        guard let code = base[lower] else { return nil }
        return Key(code: code, shift: character.isUppercase)
    }
}

private extension Character {
    var isUppercase: Bool { String(self) != String(self).lowercased() }
}
