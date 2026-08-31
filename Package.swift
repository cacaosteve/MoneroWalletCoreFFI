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
            url: "https://github.com/cacaosteve/MoneroWalletCoreFFI/releases/download/walletcore-v0.1.6/MoneroWalletCore.xcframework.zip",
            checksum: "1c87888101f2e8403f727d24bc25eeca6d68800eb361a909814ca538c2937111"
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
        ),
        .testTarget(
            name: "MoneroWalletCoreFFITests",
            dependencies: ["MoneroWalletCoreFFI"],
            path: "Tests/MoneroWalletCoreFFITests"
        )
    ]
)
