// swift-tools-version: 5.9
import PackageDescription
import Foundation

let packageRoot = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path
let rustFlags: [LinkerSetting] = [
    .unsafeFlags(["-L\(packageRoot)/libs", "-lmail_ffi"])
]

let package = Package(
    name: "Siskin",
    defaultLocalization: "zh-Hans",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "Siskin", targets: ["SiskinApp"])
    ],
    targets: [
        .systemLibrary(
            name: "mail_ffiFFI",
            path: "Sources/mail_ffiFFI"
        ),
        .target(
            name: "MailFFI",
            dependencies: ["mail_ffiFFI"],
            path: "Generated"
        ),
        .executableTarget(
            name: "SiskinApp",
            dependencies: ["MailFFI"],
            path: "Sources/SiskinApp",
            resources: [.process("Resources")],
            linkerSettings: rustFlags
        ),
        .executableTarget(
            name: "SiskinSmoke",
            dependencies: ["MailFFI"],
            path: "Sources/SiskinSmoke",
            linkerSettings: rustFlags
        ),
    ]
)
