import AVFoundation
import SwiftUI
import UIKit

struct RemoteDesktopView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let device: ManagedDevice
    @StateObject private var controller = RemoteSessionController()

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            RemoteVideoSurface(controller: controller)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .ignoresSafeArea()
            if !controller.isConnected {
                VStack(spacing: 14) {
                    ProgressView().controlSize(.large).tint(.white)
                    Text(controller.status).foregroundStyle(.white).font(.headline)
                    Text(device.name).foregroundStyle(.white.opacity(0.62)).font(.subheadline)
                }
            }
            VStack {
                topBar
                Spacer()
                bottomBar
            }
            .padding(.horizontal, 12).padding(.vertical, 8)
        }
        .persistentSystemOverlays(.hidden)
        .statusBarHidden()
        .task {
            controller.configureSessionProvider({ [weak model] in
                guard let model, let config = model.config else {
                    throw MeshError.remote("The company workspace is unavailable.")
                }
                return try await model.api.createSession(deviceID: device.id, config: config)
            }, resume: { [weak model] apiURL, bootstrap in
                guard let model else {
                    throw MeshError.remote("The company workspace is unavailable.")
                }
                return try await model.api.resumeSession(apiURL: apiURL, bootstrap: bootstrap)
            })
            await controller.connect()
        }
        .onDisappear { controller.disconnect() }
        .alert("Remote session ended", isPresented: Binding(
            get: { controller.errorMessage != nil },
            set: { if !$0 { controller.errorMessage = nil } }
        )) {
            Button("Close") { dismiss() }
        } message: { Text(controller.errorMessage ?? "") }
    }

    private var topBar: some View {
        HStack(spacing: 10) {
            Button { controller.disconnect(); dismiss() } label: { Image(systemName: "xmark") }
            VStack(alignment: .leading, spacing: 1) {
                Text(device.name).font(.subheadline.bold())
                Text(controller.status).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if let config = controller.configuration, config.displays.count > 1 {
                Menu {
                    ForEach(config.displays) { display in
                        Button {
                            controller.selectDisplay(display.id)
                        } label: {
                            if display.id == config.activeDisplayID { Label(display.name, systemImage: "checkmark") }
                            else { Text(display.name) }
                        }
                    }
                } label: { Image(systemName: "rectangle.on.rectangle") }
            }
            Button { controller.showKeyboard() } label: { Image(systemName: "keyboard") }
        }
        .buttonStyle(RemoteToolbarButtonStyle())
        .padding(8)
        .background(.ultraThinMaterial, in: Capsule())
        .environment(\.colorScheme, .dark)
    }

    private var bottomBar: some View {
        HStack(spacing: 9) {
            Label("Drag to move", systemImage: "hand.draw")
            Divider().frame(height: 16)
            Label("Tap to click", systemImage: "hand.tap")
            Divider().frame(height: 16)
            Label("Hold to drag", systemImage: "hand.point.up.left")
            Divider().frame(height: 16)
            Label("2-finger scroll", systemImage: "hand.point.up.braille")
        }
        .font(.caption2.weight(.medium)).foregroundStyle(.white.opacity(0.78))
        .minimumScaleFactor(0.72)
        .lineLimit(1)
        .padding(.horizontal, 12).padding(.vertical, 9)
        .background(.ultraThinMaterial, in: Capsule())
        .environment(\.colorScheme, .dark)
    }

}

private struct RemoteToolbarButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(.white)
            .frame(minWidth: 34, minHeight: 34)
            .contentShape(Rectangle())
            .opacity(configuration.isPressed ? 0.55 : 1)
    }
}

struct RemoteVideoSurface: UIViewRepresentable {
    @ObservedObject var controller: RemoteSessionController

    func makeUIView(context: Context) -> RemoteSurfaceUIView {
        let view = RemoteSurfaceUIView(controller: controller)
        controller.inputView = view
        controller.renderer.attach(view.sampleLayer)
        configure(view)
        return view
    }

    func updateUIView(_ view: RemoteSurfaceUIView, context: Context) {
        configure(view)
    }

    private func configure(_ view: RemoteSurfaceUIView) {
        view.configure(
            remoteSize: controller.configuration.map {
                CGSize(width: Int($0.format.width), height: Int($0.format.height))
            },
            displayID: controller.configuration?.activeDisplayID
        )
    }
}

final class RemoteSurfaceUIView: UIView, UIKeyInput, UIGestureRecognizerDelegate {
    let sampleLayer = AVSampleBufferDisplayLayer()
    private let cursorLayer = CAShapeLayer()

    private var remoteSize: CGSize?
    private var displayID: UInt32?
    private weak var controller: RemoteSessionController?
    private var pointer = TrackpadPointer()
    private var viewportCenter = CGPoint(x: 0.5, y: 0.5)
    private var pointerPan: UIPanGestureRecognizer!
    private var dragPress: UILongPressGestureRecognizer!
    private var dragButtonPressed = false

    init(controller: RemoteSessionController) {
        self.controller = controller
        super.init(frame: .zero)
        backgroundColor = .black
        clipsToBounds = true
        isMultipleTouchEnabled = true

        sampleLayer.videoGravity = .resizeAspect
        sampleLayer.backgroundColor = UIColor.black.cgColor
        layer.addSublayer(sampleLayer)
        configureCursorLayer()
        layer.addSublayer(cursorLayer)

        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        tap.numberOfTouchesRequired = 1

        let rightTap = UITapGestureRecognizer(target: self, action: #selector(rightTapped(_:)))
        rightTap.numberOfTouchesRequired = 2

        pointerPan = UIPanGestureRecognizer(target: self, action: #selector(pointerMoved(_:)))
        pointerPan.maximumNumberOfTouches = 1

        dragPress = UILongPressGestureRecognizer(target: self, action: #selector(dragPressed(_:)))
        dragPress.minimumPressDuration = 0.32
        dragPress.allowableMovement = 18

        let scroll = UIPanGestureRecognizer(target: self, action: #selector(scrolled(_:)))
        scroll.minimumNumberOfTouches = 2
        scroll.maximumNumberOfTouches = 2

        tap.delegate = self
        rightTap.delegate = self
        pointerPan.delegate = self
        dragPress.delegate = self
        scroll.delegate = self
        addGestureRecognizer(tap)
        addGestureRecognizer(rightTap)
        addGestureRecognizer(pointerPan)
        addGestureRecognizer(dragPress)
        addGestureRecognizer(scroll)

        tap.require(toFail: pointerPan)
        rightTap.require(toFail: scroll)
    }

    required init?(coder: NSCoder) { nil }
    override var canBecomeFirstResponder: Bool { true }
    var hasText: Bool { true }
    var keyboardType: UIKeyboardType = .asciiCapable

    func insertText(_ text: String) { controller?.sendText(text) }
    func deleteBackward() { controller?.sendBackspace() }

    func configure(remoteSize: CGSize?, displayID: UInt32?) {
        let displayChanged = self.displayID != displayID
        let sizeChanged = self.remoteSize != remoteSize
        self.remoteSize = remoteSize
        self.displayID = displayID
        cursorLayer.isHidden = displayID == nil
        if displayChanged || sizeChanged {
            pointer = TrackpadPointer()
            viewportCenter = CGPoint(x: 0.5, y: 0.5)
            setNeedsLayout()
        }
    }

    private func configureCursorLayer() {
        let cursor = UIBezierPath()
        cursor.move(to: CGPoint(x: 1.5, y: 1.5))
        cursor.addLine(to: CGPoint(x: 1.5, y: 27))
        cursor.addLine(to: CGPoint(x: 7.6, y: 21.2))
        cursor.addLine(to: CGPoint(x: 12.6, y: 32.8))
        cursor.addLine(to: CGPoint(x: 18.2, y: 30.4))
        cursor.addLine(to: CGPoint(x: 13.2, y: 19.3))
        cursor.addLine(to: CGPoint(x: 22.2, y: 19.3))
        cursor.close()

        cursorLayer.bounds = CGRect(x: 0, y: 0, width: 24, height: 35)
        cursorLayer.anchorPoint = CGPoint(x: 0, y: 0)
        cursorLayer.path = cursor.cgPath
        cursorLayer.fillColor = UIColor(white: 0.05, alpha: 0.95).cgColor
        cursorLayer.strokeColor = UIColor.white.cgColor
        cursorLayer.lineWidth = 1.6
        cursorLayer.lineJoin = .round
        cursorLayer.shadowColor = UIColor.black.cgColor
        cursorLayer.shadowOpacity = 0.72
        cursorLayer.shadowRadius = 2
        cursorLayer.shadowOffset = CGSize(width: 1, height: 2)
        cursorLayer.isHidden = true
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        layoutVideo()
    }

    @objc private func tapped(_ recognizer: UITapGestureRecognizer) {
        guard recognizer.state == .ended else { return }
        let position = pointer.encodedPosition
        controller?.sendButton(x: position.0, y: position.1, pressed: true)
        controller?.sendButton(x: position.0, y: position.1, pressed: false)
    }

    @objc private func rightTapped(_ recognizer: UITapGestureRecognizer) {
        guard recognizer.state == .ended else { return }
        let position = pointer.encodedPosition
        controller?.sendButton(x: position.0, y: position.1, pressed: true, button: .right)
        controller?.sendButton(x: position.0, y: position.1, pressed: false, button: .right)
    }

    @objc private func pointerMoved(_ recognizer: UIPanGestureRecognizer) {
        guard recognizer.state == .began || recognizer.state == .changed else { return }
        let translation = recognizer.translation(in: self)
        let position = pointer.move(by: translation, in: renderedVideoSize)
        updateViewportForPointer()
        controller?.sendPointerMove(x: position.0, y: position.1)
        recognizer.setTranslation(.zero, in: self)
    }

    @objc private func dragPressed(_ recognizer: UILongPressGestureRecognizer) {
        let position = pointer.encodedPosition
        switch recognizer.state {
        case .began:
            dragButtonPressed = true
            controller?.sendButton(x: position.0, y: position.1, pressed: true)
        case .ended, .cancelled, .failed:
            guard dragButtonPressed else { return }
            dragButtonPressed = false
            controller?.sendButton(x: position.0, y: position.1, pressed: false)
        default:
            break
        }
    }

    @objc private func scrolled(_ recognizer: UIPanGestureRecognizer) {
        guard recognizer.state == .changed else { return }
        let point = pointer.encodedPosition
        let translation = recognizer.translation(in: self)
        let horizontal = Int16(clamping: Int(-translation.x * 3))
        let vertical = Int16(clamping: Int(translation.y * 3))
        controller?.sendWheel(x: point.0, y: point.1, horizontal: horizontal, vertical: vertical)
        recognizer.setTranslation(.zero, in: self)
    }

    private var renderedVideoSize: CGSize {
        guard let remoteSize,
              remoteSize.width > 0,
              remoteSize.height > 0,
              bounds.width > 0,
              bounds.height > 0 else {
            return bounds.size
        }
        let fitScale = min(bounds.width / remoteSize.width, bounds.height / remoteSize.height)
        let isPortraitViewer = bounds.height > bounds.width
        let isLandscapeDesktop = remoteSize.width > remoteSize.height
        let defaultMagnification: CGFloat = isPortraitViewer && isLandscapeDesktop ? 2.4 : 1.35
        let displayScale = fitScale * defaultMagnification
        return CGSize(width: remoteSize.width * displayScale, height: remoteSize.height * displayScale)
    }

    private func updateViewportForPointer() {
        let videoSize = renderedVideoSize
        guard videoSize.width > 0, videoSize.height > 0 else { return }

        let visibleWidth = min(1, bounds.width / videoSize.width)
        let visibleHeight = min(1, bounds.height / videoSize.height)
        let horizontalDeadZone = visibleWidth * 0.3
        let verticalDeadZone = visibleHeight * 0.3

        if pointer.position.x < viewportCenter.x - horizontalDeadZone {
            viewportCenter.x = pointer.position.x + horizontalDeadZone
        } else if pointer.position.x > viewportCenter.x + horizontalDeadZone {
            viewportCenter.x = pointer.position.x - horizontalDeadZone
        }
        if pointer.position.y < viewportCenter.y - verticalDeadZone {
            viewportCenter.y = pointer.position.y + verticalDeadZone
        } else if pointer.position.y > viewportCenter.y + verticalDeadZone {
            viewportCenter.y = pointer.position.y - verticalDeadZone
        }

        viewportCenter.x = min(1 - visibleWidth / 2, max(visibleWidth / 2, viewportCenter.x))
        viewportCenter.y = min(1 - visibleHeight / 2, max(visibleHeight / 2, viewportCenter.y))
        layoutVideo()
    }

    private func layoutVideo() {
        let videoSize = renderedVideoSize
        guard videoSize.width > 0, videoSize.height > 0 else {
            sampleLayer.frame = bounds
            cursorLayer.position = CGPoint(x: bounds.midX, y: bounds.midY)
            return
        }

        let origin = CGPoint(
            x: videoOrigin(
                viewportLength: bounds.width,
                videoLength: videoSize.width,
                viewportCenter: viewportCenter.x
            ),
            y: videoOrigin(
                viewportLength: bounds.height,
                videoLength: videoSize.height,
                viewportCenter: viewportCenter.y
            )
        )
        let cursorPosition = CGPoint(
            x: origin.x + pointer.position.x * videoSize.width,
            y: origin.y + pointer.position.y * videoSize.height
        )

        CATransaction.begin()
        CATransaction.setDisableActions(true)
        sampleLayer.frame = CGRect(origin: origin, size: videoSize)
        cursorLayer.position = cursorPosition
        CATransaction.commit()
    }

    private func videoOrigin(
        viewportLength: CGFloat,
        videoLength: CGFloat,
        viewportCenter: CGFloat
    ) -> CGFloat {
        guard videoLength > viewportLength else { return (viewportLength - videoLength) / 2 }
        let proposed = viewportLength / 2 - viewportCenter * videoLength
        return min(0, max(viewportLength - videoLength, proposed))
    }

    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith otherGestureRecognizer: UIGestureRecognizer) -> Bool {
        (gestureRecognizer === pointerPan && otherGestureRecognizer === dragPress)
            || (gestureRecognizer === dragPress && otherGestureRecognizer === pointerPan)
    }
}

struct TrackpadPointer {
    private(set) var position = CGPoint(x: 0.5, y: 0.5)

    var encodedPosition: (UInt16, UInt16) {
        (
            UInt16((position.x * CGFloat(UInt16.max)).rounded()),
            UInt16((position.y * CGFloat(UInt16.max)).rounded())
        )
    }

    @discardableResult
    mutating func move(
        by translation: CGPoint,
        in renderedSize: CGSize,
        sensitivity: CGFloat = 1.15
    ) -> (UInt16, UInt16) {
        guard renderedSize.width > 0, renderedSize.height > 0 else { return encodedPosition }
        position.x = min(1, max(0, position.x + translation.x / renderedSize.width * sensitivity))
        position.y = min(1, max(0, position.y + translation.y / renderedSize.height * sensitivity))
        return encodedPosition
    }
}
