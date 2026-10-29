import AppKit
import Foundation

struct Row {
    let subject: String
    let sender: String
    let date: String
    let snippet: String
}

final class Source: NSObject, NSTableViewDataSource, NSTableViewDelegate {
    let rows: [Row]
    init(_ rows: [Row]) { self.rows = rows }

    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let id = NSUserInterfaceItemIdentifier("cell")
        let cell: NSTableCellView
        if let reused = tableView.makeView(withIdentifier: id, owner: nil) as? NSTableCellView {
            cell = reused
        } else {
            cell = NSTableCellView()
            cell.identifier = id
            let field = NSTextField(labelWithString: "")
            field.translatesAutoresizingMaskIntoConstraints = false
            field.lineBreakMode = .byTruncatingTail
            field.font = NSFont.systemFont(ofSize: 12)
            cell.addSubview(field)
            cell.textField = field
            NSLayoutConstraint.activate([
                field.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 8),
                field.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -8),
                field.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
            ])
        }
        let r = rows[row]
        cell.textField?.stringValue = "\(r.sender)  ·  \(r.subject)  ·  \(r.snippet)"
        return cell
    }
}

func residentBytes() -> UInt64 {
    var info = mach_task_basic_info()
    var count = mach_msg_type_number_t(MemoryLayout<mach_task_basic_info>.size / MemoryLayout<integer_t>.size)
    let kr = withUnsafeMutablePointer(to: &info) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            task_info(mach_task_self_, task_flavor_t(MACH_TASK_BASIC_INFO), $0, &count)
        }
    }
    return kr == KERN_SUCCESS ? info.resident_size : 0
}

let count = Int(CommandLine.arguments.dropFirst().first.flatMap { Int($0) } ?? 100_000)
let scrollSteps = 300

let baseRSS = residentBytes()

let buildStart = Date()
var rows = [Row]()
rows.reserveCapacity(count)
for i in 0..<count {
    rows.append(Row(
        subject: "Message number \(i) about project \(i % 997)",
        sender: "sender\(i % 977)@mail.test",
        date: "2026-10-\((i % 28) + 1)",
        snippet: "Synthetic preview text for row \(i), long enough to need truncation in a narrow column."
    ))
}
let buildMs = Date().timeIntervalSince(buildStart) * 1000
let afterModelRSS = residentBytes()

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
app.finishLaunching()

let window = NSWindow(
    contentRect: NSRect(x: 0, y: 0, width: 900, height: 700),
    styleMask: [.titled, .closable, .resizable],
    backing: .buffered,
    defer: false
)
let scrollView = NSScrollView(frame: window.contentView!.bounds)
scrollView.hasVerticalScroller = true
scrollView.autoresizingMask = [.width, .height]

let tableView = NSTableView(frame: scrollView.bounds)
tableView.rowHeight = 28
tableView.usesAutomaticRowHeights = false
tableView.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("main"))
column.width = 880
tableView.addTableColumn(column)

let source = Source(rows)
tableView.dataSource = source
tableView.delegate = source

scrollView.documentView = tableView
window.contentView?.addSubview(scrollView)

let windowStart = Date()
window.orderFront(nil)
tableView.reloadData()
window.displayIfNeeded()
let windowMs = Date().timeIntervalSince(windowStart) * 1000
RunLoop.main.run(until: Date().addingTimeInterval(0.3))
let afterWindowRSS = residentBytes()

let clip = scrollView.contentView
var scrollTimes = [Double]()
scrollTimes.reserveCapacity(scrollSteps)
for _ in 0..<scrollSteps {
    let t = Date()
    var origin = clip.bounds.origin
    origin.y = min(origin.y + 24, tableView.bounds.height - clip.bounds.height)
    clip.scroll(to: origin)
    scrollView.reflectScrolledClipView(clip)
    window.displayIfNeeded()
    scrollTimes.append(Date().timeIntervalSince(t) * 1000)
}

let sorted = scrollTimes.sorted()
let total = scrollTimes.reduce(0, +)
let p50 = sorted[sorted.count / 2]
let p95 = sorted[Int(Double(sorted.count) * 0.95)]
let maxMs = sorted.last ?? 0

let randomStart = Date()
var acc = 0
for i in stride(from: 0, to: count, by: 97) {
    acc += tableView.rect(ofRow: i).size.height > 0 ? 1 : 0
}
let randomMs = Date().timeIntervalSince(randomStart) * 1000

let result: [String: Any] = [
    "rows": count,
    "build_ms": buildMs,
    "window_show_ms": windowMs,
    "rss_base_mb": Double(baseRSS) / 1_048_576.0,
    "rss_after_model_mb": Double(afterModelRSS) / 1_048_576.0,
    "rss_after_window_mb": Double(afterWindowRSS) / 1_048_576.0,
    "rss_after_scroll_mb": Double(residentBytes()) / 1_048_576.0,
    "scroll_step_p50_ms": p50,
    "scroll_step_p95_ms": p95,
    "scroll_step_max_ms": maxMs,
    "scroll_total_ms": total,
    "fps_equivalent_p95": 1000.0 / max(p95, 0.001),
    "rect_of_row_ms": randomMs,
    "rows_probed": acc,
]

let data = try! JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
print(String(data: data, encoding: .utf8)!)
