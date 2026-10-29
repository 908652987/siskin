import AppKit
import Foundation
import WebKit

func residentMB() -> Double {
    var info = mach_task_basic_info()
    var count = mach_msg_type_number_t(MemoryLayout<mach_task_basic_info>.size / MemoryLayout<integer_t>.size)
    let kr = withUnsafeMutablePointer(to: &info) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            task_info(mach_task_self_, task_flavor_t(MACH_TASK_BASIC_INFO), $0, &count)
        }
    }
    return kr == KERN_SUCCESS ? Double(info.resident_size) / 1_048_576.0 : 0
}

guard CommandLine.arguments.count > 1, let url = URL(string: CommandLine.arguments[1]) else {
    FileHandle.standardError.write("usage: webview-probe <url>\n".data(using: .utf8)!)
    exit(2)
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
app.finishLaunching()

let config = WKWebViewConfiguration()
config.defaultWebpagePreferences.allowsContentJavaScript = false
config.websiteDataStore = .nonPersistent()

let webView = WKWebView(frame: NSRect(x: 0, y: 0, width: 900, height: 700), configuration: config)
let window = NSWindow(
    contentRect: webView.frame,
    styleMask: [.titled],
    backing: .buffered,
    defer: false
)
window.contentView = webView
window.orderFront(nil)

let start = Date()
webView.load(URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 10))
RunLoop.main.run(until: Date().addingTimeInterval(4.0))

let result: [String: Any] = [
    "url": url.absoluteString,
    "title": webView.title ?? "",
    "rss_mb": residentMB(),
    "elapsed_ms": Date().timeIntervalSince(start) * 1000,
    "can_go_back": webView.canGoBack,
]
let data = try! JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
print(String(data: data, encoding: .utf8)!)
