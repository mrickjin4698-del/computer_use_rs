// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "AliceComputerNative",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        .executable(name: "alice-computer-native", targets: ["AliceComputerNative"]),
    ],
    targets: [
        .executableTarget(
            name: "AliceComputerNative",
            path: "Sources/AliceComputerNative"
        ),
    ]
)
