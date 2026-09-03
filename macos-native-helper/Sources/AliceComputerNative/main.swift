import ApplicationServices
import CoreGraphics
import CoreMedia
import CoreVideo
import Darwin
import Foundation
import ImageIO
import ScreenCaptureKit
import UniformTypeIdentifiers

private enum HelperError: Error, CustomStringConvertible {
    case usage(String)
    case permission(String)
    case capture(String)
    case image(String)

    var description: String {
        switch self {
        case let .usage(message), let .permission(message), let .capture(message), let .image(message):
            return message
        }
    }
}

private final class OneShotGate: @unchecked Sendable {
    private let lock = NSLock()
    private var claimed = false

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !claimed else { return false }
        claimed = true
        return true
    }
}

@main
struct AliceComputerNative {
    private static let screenCaptureSettingsGate = OneShotGate()

    static func main() async {
        do {
            try await run(arguments: Array(CommandLine.arguments.dropFirst()))
        } catch {
            emit(["ok": false, "error": String(describing: error)])
            Darwin.exit(EXIT_FAILURE)
        }
    }

    private static func run(arguments: [String]) async throws {
        guard let command = arguments.first else {
            throw HelperError.usage("usage: alice-computer-native diagnose | capture --output <path> [--display <id>] | --stdio")
        }
        switch command {
        case "diagnose":
            diagnose()
        case "capture":
            let request = try parseCapture(arguments: Array(arguments.dropFirst()))
            try await capture(request: request)
        case "--stdio":
            try await serveStdio()
        case "--socket":
            guard arguments.count >= 2 else {
                throw HelperError.usage("--socket requires a Unix socket path")
            }
            try await serveSocket(path: arguments[1])
        case "--help", "-h":
            print("usage: alice-computer-native diagnose | capture --output <path> [--display <id>] | --stdio")
        default:
            throw HelperError.usage("unknown command: \(command)")
        }
    }

    private struct CaptureRequest {
        let output: URL
        let displayID: CGDirectDisplayID?
    }

    private static func parseCapture(arguments: [String]) throws -> CaptureRequest {
        var output: URL?
        var displayID: CGDirectDisplayID?
        var index = 0
        while index < arguments.count {
            switch arguments[index] {
            case "--output", "-o":
                index += 1
                guard index < arguments.count else {
                    throw HelperError.usage("capture --output requires a path")
                }
                output = URL(fileURLWithPath: arguments[index])
            case "--display", "-d":
                index += 1
                guard index < arguments.count, let value = UInt32(arguments[index]) else {
                    throw HelperError.usage("capture --display requires a numeric display id")
                }
                displayID = value
            case "--help", "-h":
                print("usage: alice-computer-native capture --output <path> [--display <id>]")
                Darwin.exit(EXIT_SUCCESS)
            default:
                throw HelperError.usage("unknown capture argument: \(arguments[index])")
            }
            index += 1
        }
        guard let output else {
            throw HelperError.usage("capture requires --output <path>")
        }
        return CaptureRequest(output: output, displayID: displayID)
    }

    private static func diagnose() {
        emit([
            "ok": true,
            "pid": ProcessInfo.processInfo.processIdentifier,
            "bundle_id": Bundle.main.bundleIdentifier ?? "unknown",
            "bundle_path": Bundle.main.bundlePath,
            "accessibility": AXIsProcessTrusted(),
            "screen_recording": CGPreflightScreenCaptureAccess(),
            "displays": activeDisplayIDs().count,
            "backend": "swift_screen_capture_kit",
        ])
    }

    private static func serveStdio() async throws {
        while let line = readLine() {
            guard let data = line.data(using: .utf8),
                  let request = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let method = request["method"] as? String else {
                writeLine(["ok": false, "error": "invalid helper request"])
                continue
            }

            switch method {
            case "diagnose":
                writeLine([
                    "ok": true,
                    "pid": ProcessInfo.processInfo.processIdentifier,
                    "bundle_id": Bundle.main.bundleIdentifier ?? "unknown",
                    "screen_recording": CGPreflightScreenCaptureAccess(),
                    "accessibility": AXIsProcessTrusted(),
                    "backend": "swift_screen_capture_kit",
                ])
            case "capture_display":
                guard let number = request["display_id"] as? NSNumber else {
                    writeLine(["ok": false, "error": "capture_display requires display_id"])
                    continue
                }
                do {
                    let captured = try await captureRaw(displayID: CGDirectDisplayID(number.uint32Value))
                    writeLine([
                        "ok": true,
                        "width": captured.width,
                        "height": captured.height,
                        "stride": captured.stride,
                        "data_bytes": captured.bytes.count,
                        "backend": "swift_screen_capture_kit",
                    ])
                    FileHandle.standardOutput.write(Data(captured.bytes))
                } catch {
                    writeLine(["ok": false, "error": String(describing: error)])
                }
            default:
                writeLine(["ok": false, "error": "unknown helper method: \(method)"])
            }
        }
    }

    private static func serveSocket(path: String) async throws {
        let socketPath = Array(path.utf8)
        guard socketPath.count < MemoryLayout<sockaddr_un>.size - MemoryLayout<sa_family_t>.size else {
            throw HelperError.usage("Unix socket path is too long")
        }

        _ = unlink(path)
        let server = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard server >= 0 else {
            throw HelperError.capture("could not create the helper Unix socket")
        }
        defer {
            Darwin.close(server)
            _ = unlink(path)
        }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &address.sun_path) { buffer in
            for (index, byte) in socketPath.enumerated() {
                buffer[index] = byte
            }
        }
        let addressLength = socklen_t(MemoryLayout<sa_family_t>.size + socketPath.count + 1)
        let bindResult = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { rebound in
                Darwin.bind(server, rebound, addressLength)
            }
        }
        guard bindResult == 0, Darwin.listen(server, 4) == 0 else {
            Darwin.close(server)
            throw HelperError.capture("could not publish the helper Unix socket at \(path)")
        }

        while true {
            let client = Darwin.accept(server, nil, nil)
            guard client >= 0 else { continue }
            defer { Darwin.close(client) }
            // Rust uses a connect-and-close probe to wait for the listener.
            // EOF is therefore a normal empty connection; writing an error
            // back to it would raise SIGPIPE and kill the helper process.
            guard let data = readRequestLine(fd: client) else {
                continue
            }
            guard let request = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let method = request["method"] as? String else {
                writeLine(fd: client, ["ok": false, "error": "invalid helper request"])
                continue
            }

            if method == "shutdown" {
                writeLine(fd: client, ["ok": true])
                return
            }
            guard method == "capture_display",
                  let number = request["display_id"] as? NSNumber else {
                writeLine(fd: client, ["ok": false, "error": "unknown helper method or missing display_id"])
                continue
            }
            do {
                let captured = try await captureRaw(displayID: CGDirectDisplayID(number.uint32Value))
                writeLine(fd: client, [
                    "ok": true,
                    "width": captured.width,
                    "height": captured.height,
                    "stride": captured.stride,
                    "data_bytes": captured.bytes.count,
                    "backend": "swift_screen_capture_kit",
                ])
                writeData(fd: client, data: Data(captured.bytes))
            } catch {
                writeLine(fd: client, ["ok": false, "error": String(describing: error)])
            }
        }
    }

    private static func readRequestLine(fd: Int32) -> Data? {
        var bytes = [UInt8]()
        var byte: UInt8 = 0
        while bytes.count <= 1_048_576 {
            let count = Darwin.read(fd, &byte, 1)
            guard count == 1 else { return nil }
            if byte == 10 { return Data(bytes) }
            bytes.append(byte)
        }
        return nil
    }

    private static func writeData(fd: Int32, data: Data) {
        data.withUnsafeBytes { buffer in
            guard let base = buffer.baseAddress else { return }
            var offset = 0
            while offset < buffer.count {
                let written = Darwin.write(fd, base.advanced(by: offset), buffer.count - offset)
                guard written > 0 else { return }
                offset += written
            }
        }
    }

    private static func activeDisplayIDs() -> [CGDirectDisplayID] {
        var count: UInt32 = 0
        guard CGGetActiveDisplayList(0, nil, &count) == .success, count > 0 else {
            return []
        }
        var displays = Array(repeating: CGDirectDisplayID(0), count: Int(count))
        let result = displays.withUnsafeMutableBufferPointer { buffer in
            CGGetActiveDisplayList(count, buffer.baseAddress, &count)
        }
        guard result == .success else {
            return []
        }
        return Array(displays.prefix(Int(count)))
    }

    @available(macOS 14.0, *)
    private static func capture(request: CaptureRequest) async throws {
        let started = DispatchTime.now().uptimeNanoseconds
        let selectedID = request.displayID ?? activeDisplayIDs().first
        guard let selectedID else {
            throw HelperError.capture("no active display is available")
        }
        let image = try await captureImage(displayID: selectedID)
        try writePNG(image: image, to: request.output)
        let elapsedMicros = (DispatchTime.now().uptimeNanoseconds - started) / 1_000

        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        let display = content.displays.first(where: { $0.displayID == selectedID })
        emit([
            "ok": true,
            "pid": ProcessInfo.processInfo.processIdentifier,
            "bundle_id": Bundle.main.bundleIdentifier ?? "unknown",
            "display_id": selectedID,
            "logical_width": display?.width ?? 0,
            "logical_height": display?.height ?? 0,
            "pixel_width": image.width,
            "pixel_height": image.height,
            "elapsed_micros": elapsedMicros,
            "output": request.output.path,
            "backend": "swift_screen_capture_kit",
        ])
    }

    private struct RawCapture {
        let bytes: [UInt8]
        let width: Int
        let height: Int
        let stride: Int
    }

    @available(macOS 14.0, *)
    private static func captureRaw(displayID: CGDirectDisplayID) async throws -> RawCapture {
        let image = try await captureImage(displayID: displayID)
        guard let provider = image.dataProvider,
              let data = provider.data else {
            throw HelperError.image("ScreenCaptureKit returned no pixel data")
        }

        let width = image.width
        let height = image.height
        let sourceStride = image.bytesPerRow
        let length = CFDataGetLength(data)
        guard let pointer = CFDataGetBytePtr(data), length >= 0 else {
            throw HelperError.image("ScreenCaptureKit returned an unreadable pixel buffer")
        }
        let sourceBytes = Array(UnsafeBufferPointer(start: pointer, count: length))
        guard width > 0, height > 0, sourceStride >= width * 4,
              sourceBytes.count >= sourceStride * height else {
            throw HelperError.image("ScreenCaptureKit returned invalid BGRA dimensions")
        }

        var bytes = Array(repeating: UInt8(0), count: width * height * 4)
        for row in 0..<height {
            let sourceStart = row * sourceStride
            let destinationStart = row * width * 4
            bytes[destinationStart..<(destinationStart + width * 4)] =
                sourceBytes[sourceStart..<(sourceStart + width * 4)]
        }
        return RawCapture(bytes: bytes, width: width, height: height, stride: width * 4)
    }

    @available(macOS 14.0, *)
    private static func captureImage(displayID: CGDirectDisplayID) async throws -> CGImage {
        guard CGPreflightScreenCaptureAccess() else {
            _ = CGRequestScreenCaptureAccess()
            if !CGPreflightScreenCaptureAccess(), screenCaptureSettingsGate.claim() {
                _ = try? Process.run(URL(fileURLWithPath: "/usr/bin/open"), arguments: [
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
                ])
            }
            throw HelperError.permission("Screen Recording permission is not granted to \(Bundle.main.bundleIdentifier ?? "this helper")")
        }
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        guard let display = content.displays.first(where: { $0.displayID == displayID }) else {
            throw HelperError.capture("no shareable display matched display \(displayID)")
        }
        let filter = SCContentFilter(display: display, excludingWindows: [])
        let configuration = SCStreamConfiguration()
        configuration.width = CGDisplayPixelsWide(display.displayID)
        configuration.height = CGDisplayPixelsHigh(display.displayID)
        configuration.pixelFormat = kCVPixelFormatType_32BGRA
        // The Rust runtime draws its session-scoped virtual cursor into the
        // returned frame. Keeping ScreenCaptureKit's real cursor hidden avoids
        // leaking or moving the user's pointer on the background route.
        configuration.showsCursor = false
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 1)
        return try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: configuration)
    }

    private static func writePNG(image: CGImage, to output: URL) throws {
        guard let destination = CGImageDestinationCreateWithURL(
            output as CFURL,
            UTType.png.identifier as CFString,
            1,
            nil
        ) else {
            throw HelperError.image("could not create PNG destination at \(output.path)")
        }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination) else {
            throw HelperError.image("could not finalize PNG at \(output.path)")
        }
    }

    private static func emit(_ value: [String: Any]) {
        writeLine(value)
    }

    private static func writeLine(_ value: [String: Any]) {
        writeLine(fd: STDOUT_FILENO, value)
    }

    private static func writeLine(fd: Int32, _ value: [String: Any]) {
        guard JSONSerialization.isValidJSONObject(value),
              let data = try? JSONSerialization.data(withJSONObject: value),
              let text = String(data: data, encoding: .utf8) else {
            writeData(fd: fd, data: Data("{\"ok\":false,\"error\":\"failed to serialize helper result\"}\n".utf8))
            return
        }
        writeData(fd: fd, data: Data((text + "\n").utf8))
    }
}
