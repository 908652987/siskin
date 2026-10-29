import Foundation

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

let pageIterations = 5_000
let pageSize: UInt32 = 50
var rowsSeen = 0
var sample = ""

let pageStart = Date()
for i in 0..<pageIterations {
    let rows = page(offset: UInt32(i) * pageSize, limit: pageSize)
    rowsSeen += rows.count
    if i == 0, let first = rows.first {
        sample = "\(first.id)|\(first.from)|\(first.subject)"
    }
}
let pageMs = Date().timeIntervalSince(pageStart) * 1000

let echoIterations = 20_000
let echoStart = Date()
for _ in 0..<echoIterations {
    _ = echo(text: "hello")
}
let echoMs = Date().timeIntervalSince(echoStart) * 1000

let result: [String: Any] = [
    "page_calls": pageIterations,
    "rows_per_call": pageSize,
    "rows_seen": rowsSeen,
    "page_total_ms": pageMs,
    "page_call_us": pageMs * 1000.0 / Double(pageIterations),
    "echo_calls": echoIterations,
    "echo_call_us": echoMs * 1000.0 / Double(echoIterations),
    "rss_mb": residentMB(),
    "sample": sample,
]
let data = try! JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
print(String(data: data, encoding: .utf8)!)
