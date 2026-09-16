// vt-probe: VideoToolbox capability and latency probe for the Jackstay bridge.
//
// Reports what the hardware on this machine can do for the slice-one codec plan
// (hardware HEVC/H.264 4:4:4 encode and decode, low-latency mode, LTR), and
// measures encode/decode latency and the RGB round-trip error through a chosen
// path. Nothing here depends on porthole or on the Jackstay library; it is the
// evidence tool behind docs/design and the model for the runtime capability check
// the bridge halves perform at session start.
//
// Build: ./build.sh    Run: ../../build/vt-probe [--json] [all|caps|roundtrip ...|ltr]

import Foundation
import CoreText
import CoreVideo
import CoreMedia
import VideoToolbox
import IOSurface

// MARK: - small helpers

func fourcc(_ v: OSType) -> String {
    let b = [UInt8((v >> 24) & 0xff), UInt8((v >> 16) & 0xff), UInt8((v >> 8) & 0xff), UInt8(v & 0xff)]
    return String(bytes: b, encoding: .ascii) ?? String(format: "0x%08x", v)
}

func nowNs() -> UInt64 { DispatchTime.now().uptimeNanoseconds }

func percentile(_ xs: [Double], _ p: Double) -> Double {
    if xs.isEmpty { return -1 } // JSON cannot carry NaN
    let s = xs.sorted()
    let i = min(s.count - 1, max(0, Int(Double(s.count - 1) * p)))
    return s[i]
}

final class Lock { private let m = NSLock(); func with<T>(_ f: () -> T) -> T { m.lock(); defer { m.unlock() }; return f() } }

/// Removes H.264/HEVC emulation prevention bytes.
func stripEmulationPrevention(_ nal: [UInt8]) -> [UInt8] {
    var out: [UInt8] = []; out.reserveCapacity(nal.count)
    var zeros = 0
    for b in nal {
        if zeros >= 2 && b == 3 { zeros = 0; continue }
        out.append(b)
        zeros = (b == 0) ? zeros + 1 : 0
    }
    return out
}

struct BitReader {
    let d: [UInt8]; var pos = 0
    init(_ d: [UInt8]) { self.d = d }
    mutating func bit() -> Int { let byte = pos >> 3; if byte >= d.count { return 0 }; let v = (Int(d[byte]) >> (7 - (pos & 7))) & 1; pos += 1; return v }
    mutating func bits(_ n: Int) -> Int { var v = 0; for _ in 0..<n { v = (v << 1) | bit() }; return v }
    mutating func ue() -> Int { var z = 0; while bit() == 0 && z < 32 { z += 1 }; return (1 << z) - 1 + bits(z) }
}

struct H264Sps { var profileIdc = 0; var chromaFormatIdc = 1; var bitDepthLuma = 8 }

func parseH264Sps(_ raw: [UInt8]) -> H264Sps {
    let d = stripEmulationPrevention(raw)
    var r = BitReader(d); var s = H264Sps()
    _ = r.bits(8) // nal header
    s.profileIdc = r.bits(8); _ = r.bits(8); _ = r.bits(8); _ = r.ue()
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(s.profileIdc) {
        s.chromaFormatIdc = r.ue()
        if s.chromaFormatIdc == 3 { _ = r.bit() }
        s.bitDepthLuma = 8 + r.ue()
    }
    return s
}

func hevcGeneralProfileIdc(_ raw: [UInt8]) -> Int { raw.count > 3 ? Int(raw[3] & 0x1f) : -1 }

// MARK: - JSON-ish reporting

var jsonMode = false
var report: [String: Any] = ["machine": [:], "results": [] as [Any]]
func emit(_ line: String) { if !jsonMode { print(line) } }
func record(_ obj: [String: Any]) { var rs = report["results"] as! [Any]; rs.append(obj); report["results"] = rs }

func machineInfo() -> [String: Any] {
    var size = 0; sysctlbyname("machdep.cpu.brand_string", nil, &size, nil, 0)
    var buf = [CChar](repeating: 0, count: size); sysctlbyname("machdep.cpu.brand_string", &buf, &size, nil, 0)
    let cpu = String(cString: buf)
    let os = ProcessInfo.processInfo.operatingSystemVersion
    return ["cpu": cpu, "macos": "\(os.majorVersion).\(os.minorVersion).\(os.patchVersion)", "host": ProcessInfo.processInfo.hostName]
}

// MARK: - source frames

/// A synthetic UI frame: white background, coloured panels, and rows of text in
/// several colours. Sharp chroma edges are what 4:2:0 destroys.
func makeSourceBGRA(width: Int, height: Int, frame: Int, tagColour: Bool) -> CVPixelBuffer {
    var pb: CVPixelBuffer?
    let attrs: [CFString: Any] = [kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
                                  kCVPixelBufferMetalCompatibilityKey: true]
    let status = CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA, attrs as CFDictionary, &pb)
    guard status == kCVReturnSuccess, let p = pb else {
        FileHandle.standardError.write("CVPixelBufferCreate failed: \(status)\n".data(using: .utf8)!)
        exit(3)
    }
    CVPixelBufferLockBaseAddress(p, [])
    let base = CVPixelBufferGetBaseAddress(p)!
    let bpr = CVPixelBufferGetBytesPerRow(p)
    let cs = CGColorSpace(name: CGColorSpace.sRGB)!
    let ctx = CGContext(data: base, width: width, height: height, bitsPerComponent: 8, bytesPerRow: bpr, space: cs,
                        bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue)!
    ctx.setFillColor(CGColor(srgbRed: 1, green: 1, blue: 1, alpha: 1)); ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
    let panels: [(CGFloat, CGFloat, CGFloat)] = [(0.95, 0.2, 0.2), (0.2, 0.6, 0.95), (0.15, 0.7, 0.3), (0.9, 0.6, 0.1), (0.5, 0.2, 0.8)]
    for (i, c) in panels.enumerated() {
        ctx.setFillColor(CGColor(srgbRed: c.0, green: c.1, blue: c.2, alpha: 1))
        let x = CGFloat(i) * CGFloat(width) / CGFloat(panels.count)
        ctx.fill(CGRect(x: x, y: CGFloat(height) * 0.8, width: CGFloat(width) / CGFloat(panels.count) - 8, height: CGFloat(height) * 0.18))
    }
    let font = CTFontCreateWithName("Menlo" as CFString, 14, nil)
    let colours: [CGColor] = [CGColor(srgbRed: 0, green: 0, blue: 0, alpha: 1), CGColor(srgbRed: 0.85, green: 0.1, blue: 0.1, alpha: 1),
                              CGColor(srgbRed: 0.1, green: 0.3, blue: 0.9, alpha: 1), CGColor(srgbRed: 0.05, green: 0.55, blue: 0.2, alpha: 1)]
    var y: CGFloat = 20
    var row = 0
    while y < CGFloat(height) * 0.78 {
        let text = String(format: "%04d  fn acquire(cursor: u64) -> Result<FrameLease, AcquireError> { // frame %d row %d }", row, frame, row)
        let attrs: [CFString: Any] = [kCTFontAttributeName: font, kCTForegroundColorAttributeName: colours[row % colours.count]]
        let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attrs as [NSAttributedString.Key: Any]))
        ctx.textPosition = CGPoint(x: 12 + CGFloat(frame % 3), y: y)
        CTLineDraw(line, ctx)
        y += 18; row += 1
    }
    CVPixelBufferUnlockBaseAddress(p, [])
    if tagColour {
        CVBufferSetAttachment(p, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
        CVBufferSetAttachment(p, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_sRGB, .shouldPropagate)
        CVBufferSetAttachment(p, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, .shouldPropagate)
    }
    return p
}

/// Converts a pixel buffer to another format with VTPixelTransferSession (used to
/// feed 444f/420f sources and to bring decoded YCbCr back to BGRA for comparison).
final class Transfer {
    var session: VTPixelTransferSession?
    init() { VTPixelTransferSessionCreate(allocator: nil, pixelTransferSessionOut: &session) }
    func convert(_ src: CVPixelBuffer, to fmt: OSType, fullRange: Bool = true) -> CVPixelBuffer? {
        var dst: CVPixelBuffer?
        let attrs: [CFString: Any] = [kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary]
        CVPixelBufferCreate(nil, CVPixelBufferGetWidth(src), CVPixelBufferGetHeight(src), fmt, attrs as CFDictionary, &dst)
        guard let d = dst, let s = session else { return nil }
        let st = VTPixelTransferSessionTransferImage(s, from: src, to: d)
        return st == noErr ? d : nil
    }
}

/// Mean and max absolute error per channel between two BGRA buffers.
func compareBGRA(_ a: CVPixelBuffer, _ b: CVPixelBuffer) -> [String: Any] {
    CVPixelBufferLockBaseAddress(a, .readOnly); CVPixelBufferLockBaseAddress(b, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(a, .readOnly); CVPixelBufferUnlockBaseAddress(b, .readOnly) }
    let w = CVPixelBufferGetWidth(a), h = CVPixelBufferGetHeight(a)
    let pa = CVPixelBufferGetBaseAddress(a)!.assumingMemoryBound(to: UInt8.self), ra = CVPixelBufferGetBytesPerRow(a)
    let pb = CVPixelBufferGetBaseAddress(b)!.assumingMemoryBound(to: UInt8.self), rb = CVPixelBufferGetBytesPerRow(b)
    var sum = [0, 0, 0], mx = [0, 0, 0], over8 = 0
    for y in 0..<h { for x in 0..<w {
        let oa = y * ra + x * 4, ob = y * rb + x * 4
        var bad = false
        for c in 0..<3 { let d = abs(Int(pa[oa + c]) - Int(pb[ob + c])); sum[c] += d; if d > mx[c] { mx[c] = d }; if d > 8 { bad = true } }
        if bad { over8 += 1 }
    } }
    let n = Double(w * h)
    return ["mean_abs_bgr": sum.map { Double($0) / n }, "max_abs_bgr": mx, "pixels_err_gt8_pct": 100.0 * Double(over8) / n]
}

// MARK: - encoder

struct EncodeConfig {
    var codec: CMVideoCodecType = kCMVideoCodecType_HEVC
    var profile: String? = "HEVC_Main444_AutoLevel"
    var lowLatency = false
    var srcFormat: OSType = kCVPixelFormatType_32BGRA
    var width = 1920, height = 1080
    var bitrate = 20_000_000
    var frames = 60
    var tagColour = true
    var enableLTR = false
    var name: String { "\(codec == kCMVideoCodecType_HEVC ? "hevc" : "h264")\(lowLatency ? "-lowlatency" : "")/\(profile ?? "default")/\(fourcc(srcFormat))/\(width)x\(height)" }
}

final class EncodeResult {
    var samples: [CMSampleBuffer] = []
    var latenciesMs: [Double] = []
    var keyframes = 0, dropped = 0, ltrTokens = 0
    var format: CMFormatDescription?
    var submitted: [Int64: UInt64] = [:]
    let lock = Lock()
}

struct EncodeOutcome { var ok: Bool; var info: [String: Any]; var result: EncodeResult; var frames: [CVPixelBuffer] }

func runEncode(_ cfg: EncodeConfig) -> EncodeOutcome {
    var info: [String: Any] = ["config": cfg.name]
    let res = EncodeResult()
    var spec: [CFString: Any] = [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true]
    if cfg.lowLatency { spec[kVTVideoEncoderSpecification_EnableLowLatencyRateControl] = true }
    let srcAttrs: [CFString: Any] = [kCVPixelBufferPixelFormatTypeKey: cfg.srcFormat, kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary]
    let cb: VTCompressionOutputCallback = { refcon, frameRefcon, status, flags, sbuf in
        let r = Unmanaged<EncodeResult>.fromOpaque(refcon!).takeUnretainedValue()
        let t1 = nowNs()
        if flags.contains(.frameDropped) { r.lock.with { r.dropped += 1 } }
        guard status == noErr, let s = sbuf else { return }
        let pts = CMSampleBufferGetPresentationTimeStamp(s).value
        var key = true
        if let arr = CMSampleBufferGetSampleAttachmentsArray(s, createIfNecessary: false) as? [[CFString: Any]], let a = arr.first {
            if let ns = a[kCMSampleAttachmentKey_NotSync] as? Bool, ns { key = false }
            if a[kVTSampleAttachmentKey_RequireLTRAcknowledgementToken] != nil { r.lock.with { r.ltrTokens += 1 } }
        }
        r.lock.with {
            r.samples.append(s); r.format = CMSampleBufferGetFormatDescription(s)
            if key { r.keyframes += 1 }
            if let t0 = r.submitted.removeValue(forKey: pts) { r.latenciesMs.append(Double(t1 - t0) / 1e6) }
        }
    }
    var session: VTCompressionSession?
    let st = VTCompressionSessionCreate(allocator: nil, width: Int32(cfg.width), height: Int32(cfg.height), codecType: cfg.codec,
                                        encoderSpecification: spec as CFDictionary, imageBufferAttributes: srcAttrs as CFDictionary,
                                        compressedDataAllocator: nil, outputCallback: cb,
                                        refcon: Unmanaged.passUnretained(res).toOpaque(), compressionSessionOut: &session)
    info["create_status"] = Int(st)
    guard st == noErr, let s = session else { return EncodeOutcome(ok: false, info: info, result: res, frames: []) }
    func setp(_ k: CFString, _ v: CFTypeRef) -> Int { Int(VTSessionSetProperty(s, key: k, value: v)) }
    if let p = cfg.profile { info["set_profile_status"] = setp(kVTCompressionPropertyKey_ProfileLevel, p as CFString) }
    _ = setp(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)
    _ = setp(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse)
    _ = setp(kVTCompressionPropertyKey_AverageBitRate, cfg.bitrate as CFNumber)
    _ = setp(kVTCompressionPropertyKey_ExpectedFrameRate, 60 as CFNumber)
    _ = setp(kVTCompressionPropertyKey_MaxKeyFrameInterval, 100_000 as CFNumber)
    _ = setp(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue)
    if cfg.tagColour {
        _ = setp(kVTCompressionPropertyKey_ColorPrimaries, kCMFormatDescriptionColorPrimaries_ITU_R_709_2)
        _ = setp(kVTCompressionPropertyKey_TransferFunction, kCMFormatDescriptionTransferFunction_sRGB)
        _ = setp(kVTCompressionPropertyKey_YCbCrMatrix, kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2)
    }
    if cfg.enableLTR { info["enable_ltr_status"] = setp(kVTCompressionPropertyKey_EnableLTR, kCFBooleanTrue) }
    var v: CFTypeRef?
    VTSessionCopyProperty(s, key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, allocator: nil, valueOut: &v)
    info["using_hw_encoder"] = (v as? Bool).map { $0 ? 1 : 0 } ?? -1
    v = nil
    VTSessionCopyProperty(s, key: kVTCompressionPropertyKey_EncoderID, allocator: nil, valueOut: &v)
    info["encoder_id"] = (v as? String) ?? "unknown"
    VTCompressionSessionPrepareToEncodeFrames(s)

    let xfer = Transfer()
    var frames: [CVPixelBuffer] = []
    let warmup = 3
    for i in 0..<(cfg.frames + warmup) {
        let bgra = makeSourceBGRA(width: cfg.width, height: cfg.height, frame: i, tagColour: cfg.tagColour)
        let src: CVPixelBuffer
        if cfg.srcFormat == kCVPixelFormatType_32BGRA { src = bgra } else {
            guard let c = xfer.convert(bgra, to: cfg.srcFormat) else { info["source_convert_failed"] = fourcc(cfg.srcFormat); break }
            src = c
        }
        if i >= warmup { frames.append(bgra) }
        let pts = CMTime(value: CMTimeValue(i), timescale: 60)
        res.lock.with { res.submitted[pts.value] = nowNs() }
        let r = VTCompressionSessionEncodeFrame(s, imageBuffer: src, presentationTimeStamp: pts, duration: .invalid,
                                                frameProperties: nil, sourceFrameRefcon: nil, infoFlagsOut: nil)
        if r != noErr { info["encode_frame_status"] = Int(r) }
        // pace at 60 fps so rate control behaves as it would live
        usleep(16_000)
    }
    VTCompressionSessionCompleteFrames(s, untilPresentationTimeStamp: .invalid)
    VTCompressionSessionInvalidate(s)
    info["frames_out"] = res.samples.count
    if res.samples.isEmpty {
        // A session that opens but never produces output (an unsupported
        // profile or source format on this chip) must not poison the JSON with
        // NaN statistics; report the failure and let the other probes run.
        info["encode_failed"] = "no output samples"
        return EncodeOutcome(ok: false, info: info, result: res, frames: frames)
    }
    // drop warm-up samples/latencies from the statistics but keep them in the stream
    let lat = Array(res.latenciesMs.dropFirst(warmup))
    info["keyframes"] = res.keyframes
    info["dropped"] = res.dropped
    info["ltr_tokens"] = res.ltrTokens
    info["warmup_first_frame_ms"] = res.latenciesMs.first.map { ($0 * 100).rounded() / 100 } ?? -1
    info["encode_ms_p50"] = (percentile(lat, 0.5) * 100).rounded() / 100
    info["encode_ms_p90"] = (percentile(lat, 0.9) * 100).rounded() / 100
    let bytes = res.samples.reduce(0) { $0 + CMSampleBufferGetTotalSampleSize($1) }
    info["kbit_per_s_at_60fps"] = Int(Double(bytes) * 8 * 60 / Double(max(1, res.samples.count)) / 1000)
    if let f = res.format {
        var ptr: UnsafePointer<UInt8>?; var size = 0; var count = 0; var nal: Int32 = 0
        if cfg.codec == kCMVideoCodecType_HEVC {
            if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(f, parameterSetIndex: 1, parameterSetPointerOut: &ptr, parameterSetSizeOut: &size, parameterSetCountOut: &count, nalUnitHeaderLengthOut: &nal) == noErr, let p = ptr {
                let raw = Array(UnsafeBufferPointer(start: p, count: size))
                info["sps_general_profile_idc"] = hevcGeneralProfileIdc(raw)
            }
        } else {
            if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(f, parameterSetIndex: 0, parameterSetPointerOut: &ptr, parameterSetSizeOut: &size, parameterSetCountOut: &count, nalUnitHeaderLengthOut: &nal) == noErr, let p = ptr {
                let sps = parseH264Sps(Array(UnsafeBufferPointer(start: p, count: size)))
                info["sps_profile_idc"] = sps.profileIdc; info["sps_chroma_format_idc"] = sps.chromaFormatIdc; info["sps_bit_depth"] = sps.bitDepthLuma
            }
        }
        if let ext = CMFormatDescriptionGetExtensions(f) as? [String: Any] {
            for k in ["CVImageBufferColorPrimaries", "CVImageBufferTransferFunction", "CVImageBufferYCbCrMatrix", "FullRangeVideo"] {
                if let x = ext[k] { info["fmt_\(k)"] = "\(x)" }
            }
        }
    }
    return EncodeOutcome(ok: res.samples.count > 0, info: info, result: res, frames: frames)
}

// MARK: - decoder

final class DecodeResult {
    var outputs: [CVPixelBuffer] = []
    var latenciesMs: [Double] = []
    var errors = 0
    var submitted: [Int64: UInt64] = [:]
    let lock = Lock()
}

func runDecode(_ enc: EncodeOutcome, destFormat: OSType, info: inout [String: Any]) -> DecodeResult {
    let dres = DecodeResult()
    guard let first = enc.result.samples.first, let fmt = CMSampleBufferGetFormatDescription(first) else { info["decode"] = "no samples"; return dres }
    info["hw_decode_supported"] = VTIsHardwareDecodeSupported(CMFormatDescriptionGetMediaSubType(fmt))
    let spec: [CFString: Any] = [kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true]
    let dest: [CFString: Any] = [kCVPixelBufferPixelFormatTypeKey: destFormat,
                                 kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
                                 kCVPixelBufferMetalCompatibilityKey: true]
    var rec = VTDecompressionOutputCallbackRecord()
    rec.decompressionOutputCallback = { refcon, _, status, _, img, pts, _ in
        let r = Unmanaged<DecodeResult>.fromOpaque(refcon!).takeUnretainedValue()
        let t1 = nowNs()
        r.lock.with {
            if status == noErr, let i = img { r.outputs.append(i) } else { r.errors += 1 }
            if let t0 = r.submitted.removeValue(forKey: pts.value) { r.latenciesMs.append(Double(t1 - t0) / 1e6) }
        }
    }
    rec.decompressionOutputRefCon = Unmanaged.passUnretained(dres).toOpaque()
    var session: VTDecompressionSession?
    let st = VTDecompressionSessionCreate(allocator: nil, formatDescription: fmt, decoderSpecification: spec as CFDictionary,
                                          imageBufferAttributes: dest as CFDictionary, outputCallback: &rec, decompressionSessionOut: &session)
    info["decode_create_status"] = Int(st)
    guard st == noErr, let s = session else { return dres }
    var v: CFTypeRef?
    VTSessionCopyProperty(s, key: kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder, allocator: nil, valueOut: &v)
    info["using_hw_decoder"] = (v as? Bool).map { $0 ? 1 : 0 } ?? -1
    v = nil
    VTSessionCopyProperty(s, key: kVTDecompressionPropertyKey_PixelBufferPoolIsShared, allocator: nil, valueOut: &v)
    info["decoder_pool_is_shared"] = (v as? Bool).map { $0 ? 1 : 0 } ?? -1
    for sb in enc.result.samples {
        let pts = CMSampleBufferGetPresentationTimeStamp(sb).value
        dres.lock.with { dres.submitted[pts] = nowNs() }
        let r = VTDecompressionSessionDecodeFrame(s, sampleBuffer: sb, flags: [._EnableAsynchronousDecompression], frameRefcon: nil, infoFlagsOut: nil)
        if r != noErr { info["decode_frame_status"] = Int(r) }
    }
    VTDecompressionSessionWaitForAsynchronousFrames(s)
    VTDecompressionSessionInvalidate(s)
    info["decoded_frames"] = dres.outputs.count
    info["decode_errors"] = dres.errors
    info["decode_output_format"] = dres.outputs.first.map { fourcc(CVPixelBufferGetPixelFormatType($0)) } ?? "none"
    info["decode_output_iosurface"] = dres.outputs.first.map { CVPixelBufferGetIOSurface($0) != nil } ?? false
    info["decode_ms_p50"] = (percentile(dres.latenciesMs, 0.5) * 100).rounded() / 100
    info["decode_ms_p90"] = (percentile(dres.latenciesMs, 0.9) * 100).rounded() / 100
    return dres
}

// MARK: - probes

func probeCaps() {
    var out: [String: Any] = ["probe": "caps"]
    var list: CFArray?
    VTCopyVideoEncoderList(nil, &list)
    var encoders: [[String: Any]] = []
    for e in (list as? [[CFString: Any]]) ?? [] {
        encoders.append(["id": e[kVTVideoEncoderList_EncoderID] as? String ?? "?",
                         "name": e[kVTVideoEncoderList_EncoderName] as? String ?? "?",
                         "codec": (e[kVTVideoEncoderList_CodecType] as? OSType).map(fourcc) ?? "?",
                         "hw": (e[kVTVideoEncoderList_IsHardwareAccelerated] as? Bool) ?? false])
    }
    out["encoders"] = encoders
    for (codec, name) in [(kCMVideoCodecType_HEVC, "hevc"), (kCMVideoCodecType_H264, "h264")] {
        var id: CFString?; var dict: CFDictionary?
        let spec: [CFString: Any] = [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true]
        let st = VTCopySupportedPropertyDictionaryForEncoder(width: 1920, height: 1080, codecType: codec, encoderSpecification: spec as CFDictionary,
                                                             encoderIDOut: &id, supportedPropertiesOut: &dict)
        var entry: [String: Any] = ["status": Int(st), "encoder_id": id.map { String($0) } ?? "none"]
        if let d = dict as? [String: Any], let pl = d[kVTCompressionPropertyKey_ProfileLevel as String] as? [String: Any],
           let vals = pl[kVTPropertySupportedValueListKey as String] as? [String] {
            entry["profiles"] = vals
            entry["has_444"] = vals.contains { $0.contains("444") }
        }
        if let d = dict as? [String: Any] {
            entry["supports_enable_ltr"] = d[kVTCompressionPropertyKey_EnableLTR as String] != nil
            entry["supports_max_qp"] = d[kVTCompressionPropertyKey_MaxAllowedFrameQP as String] != nil
        }
        out["hw_encoder_\(name)"] = entry
    }
    out["hw_decode"] = ["h264": VTIsHardwareDecodeSupported(kCMVideoCodecType_H264),
                        "hevc": VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC),
                        "av1": VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1)]
    record(out)
    emit("== caps")
    for e in encoders where (e["hw"] as? Bool) == true { emit("  hw encoder \(e["codec"]!) \(e["id"]!)") }
    for n in ["hevc", "h264"] { if let e = out["hw_encoder_\(n)"] as? [String: Any] { emit("  \(n): has_444=\(e["has_444"] ?? "?") ltr_key=\(e["supports_enable_ltr"] ?? "?") profiles=\(e["profiles"] ?? [])") } }
    emit("  hw decode: \(out["hw_decode"]!)")
}

/// Picks the decoder output format with the same chroma as requested and the same
/// range as the stream. Asking for full range on a video-range stream (or the
/// reverse) makes VideoToolbox keep a private pool and copy every frame, which
/// showed up as 3x to 8x decode latency and `decoder_pool_is_shared = 0`.
func matchedDestFormat(chroma444: Bool, stream: CMFormatDescription?) -> OSType {
    var full = false
    if let f = stream, let ext = CMFormatDescriptionGetExtensions(f) as? [String: Any], let fr = ext["FullRangeVideo"] as? Bool { full = fr }
    switch (chroma444, full) {
    case (true, true): return kCVPixelFormatType_444YpCbCr8BiPlanarFullRange
    case (true, false): return kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange
    case (false, true): return kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
    case (false, false): return kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
    }
}

func probeRoundtrip(_ cfg: EncodeConfig, chroma444: Bool, forceDest: OSType? = nil) {
    emit("== roundtrip \(cfg.name)")
    let enc = runEncode(cfg)
    let destFormat = forceDest ?? matchedDestFormat(chroma444: chroma444, stream: enc.result.format)
    var info = enc.info; info["probe"] = "roundtrip"; info["dest_format"] = fourcc(destFormat)
    emit("  dest_format = \(fourcc(destFormat))")
    if enc.ok {
        let dec = runDecode(enc, destFormat: destFormat, info: &info)
        // colour round trip on the last decoded frame vs its BGRA source
        if let last = dec.outputs.last, let src = enc.frames.last, dec.outputs.count == enc.result.samples.count {
            let xfer = Transfer()
            if let back = xfer.convert(last, to: kCVPixelFormatType_32BGRA) {
                info["rgb_roundtrip"] = compareBGRA(src, back)
            } else { info["rgb_roundtrip"] = "transfer to BGRA failed" }
        }
    }
    record(info)
    for k in ["encoder_id", "using_hw_encoder", "set_profile_status", "sps_general_profile_idc", "sps_profile_idc", "sps_chroma_format_idc",
              "fmt_FullRangeVideo", "fmt_CVImageBufferYCbCrMatrix", "frames_out", "keyframes", "dropped", "kbit_per_s_at_60fps",
              "warmup_first_frame_ms", "encode_ms_p50", "encode_ms_p90", "using_hw_decoder", "decoder_pool_is_shared",
              "decode_output_format", "decoded_frames", "decode_errors", "decode_ms_p50", "decode_ms_p90", "rgb_roundtrip"] {
        if let v = info[k] { emit("  \(k) = \(v)") }
    }
}

func probeLTR() {
    emit("== ltr")
    for (codec, profile) in [(kCMVideoCodecType_HEVC, "HEVC_Main444_AutoLevel"), (kCMVideoCodecType_HEVC, "HEVC_Main_AutoLevel"), (kCMVideoCodecType_H264, "H264_High_AutoLevel")] {
        var cfg = EncodeConfig(); cfg.codec = codec; cfg.profile = profile; cfg.lowLatency = true; cfg.enableLTR = true
        cfg.srcFormat = codec == kCMVideoCodecType_HEVC && profile.contains("444") ? kCVPixelFormatType_444YpCbCr8BiPlanarFullRange : kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        cfg.frames = 120
        let enc = runEncode(cfg)
        var info = enc.info; info["probe"] = "ltr"
        record(info)
        emit("  \(cfg.name): create=\(info["create_status"] ?? -1) enable_ltr=\(info["enable_ltr_status"] ?? -1) tokens=\(info["ltr_tokens"] ?? 0) dropped=\(info["dropped"] ?? 0) sps_profile=\(info["sps_general_profile_idc"] ?? info["sps_profile_idc"] ?? -1) enc_p50=\(info["encode_ms_p50"] ?? -1)")
    }
}

func probeAll() {
    probeCaps()
    let f444 = kCVPixelFormatType_444YpCbCr8BiPlanarFullRange
    let bgra = kCVPixelFormatType_32BGRA
    var c = EncodeConfig()
    // the slice-one plan: normal HEVC encoder, Main 4:4:4, BGRA in (what SCK gives)
    c.codec = kCMVideoCodecType_HEVC; c.profile = "HEVC_Main444_AutoLevel"; c.srcFormat = bgra; probeRoundtrip(c, chroma444: true)
    // the same stream decoded into a mismatched-range format, to show the private-pool copy cost
    probeRoundtrip(c, chroma444: true, forceDest: f444)
    // a 4:4:4 YCbCr source: what BGRA input costs, and full range end to end
    c.srcFormat = f444; probeRoundtrip(c, chroma444: true)
    // untagged BGRA: what happens if we forget colour attachments
    c.srcFormat = bgra; c.tagColour = false; probeRoundtrip(c, chroma444: true); c.tagColour = true
    // the 4:2:0 control, to quantify the chroma loss on text
    c.profile = "HEVC_Main_AutoLevel"; c.srcFormat = bgra; probeRoundtrip(c, chroma444: false)
    // H.264 High 4:4:4 as the knob
    c.codec = kCMVideoCodecType_H264; c.profile = "H264_High444Predictive_AutoLevel"; probeRoundtrip(c, chroma444: true)
    // HEVC low-latency mode with 4:4:4
    c.codec = kCMVideoCodecType_HEVC; c.profile = "HEVC_Main444_AutoLevel"; c.lowLatency = true; c.srcFormat = f444; probeRoundtrip(c, chroma444: true); c.lowLatency = false
    // 1440p latency on the slice-one path
    c.srcFormat = bgra; c.width = 2560; c.height = 1440; probeRoundtrip(c, chroma444: true)
    probeLTR()
}

// MARK: - main

var args = Array(CommandLine.arguments.dropFirst())
if let i = args.firstIndex(of: "--json") { jsonMode = true; args.remove(at: i) }
report["machine"] = machineInfo()
report["date"] = ISO8601DateFormatter().string(from: Date())
emit("vt-probe on \(report["machine"]!)")
let cmd = args.first ?? "all"
switch cmd {
case "caps": probeCaps()
case "ltr": probeLTR()
case "roundtrip":
    // roundtrip <hevc|h264> <profile|default> <bgra|444f|420f> [WxH] [--lowlatency] [--untagged]
    var c = EncodeConfig()
    c.codec = (args.count > 1 && args[1] == "h264") ? kCMVideoCodecType_H264 : kCMVideoCodecType_HEVC
    if args.count > 2 { c.profile = args[2] == "default" ? nil : args[2] }
    if args.count > 3 { c.srcFormat = ["bgra": kCVPixelFormatType_32BGRA, "444f": kCVPixelFormatType_444YpCbCr8BiPlanarFullRange, "420f": kCVPixelFormatType_420YpCbCr8BiPlanarFullRange][args[3]] ?? kCVPixelFormatType_32BGRA }
    if args.count > 4 {
        let p = args[4].split(separator: "x")
        if p.count == 2 { c.width = Int(p[0]) ?? 1920; c.height = Int(p[1]) ?? 1080 }
    }
    c.lowLatency = args.contains("--lowlatency"); c.tagColour = !args.contains("--untagged")
    probeRoundtrip(c, chroma444: (c.profile ?? "").contains("444"))
case "all": probeAll()
default:
    FileHandle.standardError.write("usage: vt-probe [--json] [all|caps|ltr|roundtrip <hevc|h264> <profile|default> <bgra|444f|420f> [WxH] [--lowlatency] [--untagged]]\n".data(using: .utf8)!)
    exit(2)
}
if jsonMode {
    let data = try! JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted, .sortedKeys])
    print(String(data: data, encoding: .utf8)!)
}
