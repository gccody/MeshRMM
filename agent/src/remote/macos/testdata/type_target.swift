// A text window for input tests: reports its frame in global display
// coordinates and keeps writing its text to the given file until killed.
import AppKit
let output = CommandLine.arguments[1]
let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(contentRect: NSRect(x: 300, y: 300, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
let text = NSTextView(frame: window.contentView!.bounds)
window.contentView!.addSubview(text)
window.makeKeyAndOrderFront(nil)
window.makeFirstResponder(text)
app.activate(ignoringOtherApps: true)
Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in
    let frame = window.frame
    let top = NSScreen.screens[0].frame.height - frame.maxY
    let report = "\(frame.minX) \(top) \(frame.width) \(frame.height) \(NSApp.isActive)\n" + text.string
    try? report.write(toFile: output, atomically: true, encoding: .utf8)
}
app.run()
