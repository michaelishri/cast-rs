// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "CastDesktop",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "CastDesktop", targets: ["CastDesktop"]),
    ],
    targets: [
        .executableTarget(
            name: "CastDesktop",
            path: "Sources/CastDesktop"
        ),
        .testTarget(
            name: "CastDesktopTests",
            dependencies: ["CastDesktop"],
            path: "Tests/CastDesktopTests"
        ),
    ]
)
