// swift-tools-version: 5.9
import PackageDescription

#if os(Linux)
let clibTarget: Target = .systemLibrary(
    name: "CLibMoneroWalletCore",
    path: "CLibMoneroWalletCore",
    pkgConfig: "monerowalletcore"
)
#else
let clibTarget: Target = .systemLibrary(
    name: "CLibMoneroWalletCore",
    path: "CLibMoneroWalletCore"
)
#endif

let package = Package(
    name: "MoneroWalletCoreFFI",
    defaultLocalization: "en",
    platforms: [
        .iOS(.v16),
        .macOS(.v13),
        .macCatalyst(.v16)
    ],
    products: [
        .library(name: "MoneroWalletCoreFFI", targets: ["MoneroWalletCoreFFI"])
    ],
    targets: [
        .binaryTarget(
            name: "MoneroWalletCore",
            // The XCFramework is generated from source by CI and published as a
            // versioned release asset. Keeping it out of Git makes clones and
            // SwiftPM dependency resolution much smaller while preserving the
            // no-Rust-required Apple consumer experience.
            url: "https://github.com/cacaosteve/MoneroWalletCoreFFI/releases/download/walletcore-v0.1.0/MoneroWalletCore.xcframework.zip",
            checksum: "f3359d10e4e34f9114c6aebb3513d86dcf0cfcc572f107058fe0f51fd852a7f5"
        ),
        clibTarget,
        .target(
            name: "MoneroWalletCoreFFI",
            dependencies: [
                .target(name: "MoneroWalletCore", condition: .when(platforms: [.iOS, .macOS, .macCatalyst])),
                .target(name: "CLibMoneroWalletCore", condition: .when(platforms: [.linux]))
            ],
            path: "Sources/MoneroWalletCoreFFI",
            swiftSettings: [
                .define("WALLETCORE_APPLE", .when(platforms: [.iOS, .macOS, .macCatalyst])),
                .define("WALLETCORE_LINUX", .when(platforms: [.linux]))
            ],
            linkerSettings: [
                .linkedLibrary("c++", .when(platforms: [.iOS, .macOS, .macCatalyst]))
            ]
        ),
        .executableTarget(
            name: "MoneroWalletCoreFFI_Smoke",
            dependencies: ["MoneroWalletCoreFFI"],
            path: "Utilities/Smoke",
            sources: ["main.swift"]
        )
    ]
)
