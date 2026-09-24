import AudioToolbox
import CoreAudio
import Foundation
import IOBluetooth

// Records from one input device through an AUHAL input unit (as CPAL does)
// and logs, with UTC timestamps in the probe's format:
// - every CoreAudio property notification, from wildcard listeners on the
//   system object, every device (output-only too), everything each device
//   owns (streams, controls), boxes, clock devices, and process objects;
// - the input unit's own property changes (running, format, render errors);
// - when exact-zero audio starts and ends, and gaps in the callback cadence;
// - the Bluetooth connection state of the headset.
//
// Some notifications reach only a process doing I/O on the device, which is
// why this records instead of only listening.
//
// Experiments for pulling a headset back from a phone (see README.md):
// - `--voice-processing` records through the voice-processing unit (the one
//   calling apps use) instead of a plain AUHAL input unit;
// - `--play silence|tone` runs an output stream on the headset's output
//   device, starting `--play-after` seconds in and, with `--play-for`,
//   stopping after that many seconds.
//
// Usage: handoff_listener [--device <UID or name substring>] [--bt <address>] [--no-tap]
//        [--voice-processing] [--play silence|tone] [--play-after <s>] [--play-for <s>]
//        [--output <UID or name substring>]

// MARK: Arguments

var deviceArg: String?
var btArg: String?
var tapEnabled = true
var voiceProcessing = false
enum Play: String { case silence, tone }
var play: Play?
var playAfter = 5.0
var playFor: Double?
var outputArg: String?
let usage = """
    usage: handoff_listener [--device <UID or name substring>] [--bt <address>] [--no-tap]
           [--voice-processing] [--play silence|tone] [--play-after <s>] [--play-for <s>]
           [--output <UID or name substring>]
    """
func seconds(_ s: String?) -> Double {
    guard let s = s, let v = Double(s), v >= 0 else { print(usage); exit(2) }
    return v
}
do {
    var args = CommandLine.arguments.dropFirst().makeIterator()
    while let a = args.next() {
        switch a {
        case "--device": deviceArg = args.next()
        case "--bt": btArg = args.next()
        case "--no-tap": tapEnabled = false
        case "--voice-processing": voiceProcessing = true
        case "--play":
            guard let p = args.next().flatMap(Play.init(rawValue:)) else { print(usage); exit(2) }
            play = p
        case "--play-after": playAfter = seconds(args.next())
        case "--play-for": playFor = seconds(args.next())
        case "--output": outputArg = args.next()
        default:
            print(usage)
            exit(2)
        }
    }
}

// MARK: Logging

let timeFormat: DateFormatter = {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.timeZone = TimeZone(identifier: "UTC")
    f.dateFormat = "HH:mm:ss.SSS'Z'"
    return f
}()
setvbuf(stdout, nil, _IOLBF, 0)
func log(_ s: String, at date: Date = Date()) {
    print("\(timeFormat.string(from: date))  \(s)")
}

// Host time (mach ticks) to wall-clock time, for timestamps taken in the callback.
var timebase = mach_timebase_info_data_t()
mach_timebase_info(&timebase)
let startHost = mach_absolute_time()
let startDate = Date()
func date(fromHost host: UInt64) -> Date {
    let ticks = Double(Int64(bitPattern: host &- startHost))
    return startDate.addingTimeInterval(ticks * Double(timebase.numer) / Double(timebase.denom) / 1e9)
}

// MARK: CoreAudio helpers

let sys = AudioObjectID(kAudioObjectSystemObject)

func address(_ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
             _ element: AudioObjectPropertyElement = kAudioObjectPropertyElementMain) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: sel, mScope: scope, mElement: element)
}
func get<T>(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal, _ value: T) -> T? {
    var addr = address(sel, scope)
    var v = value
    var size = UInt32(MemoryLayout<T>.size)
    return AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &v) == noErr ? v : nil
}
func getArray(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> [AudioObjectID] {
    var addr = address(sel, scope)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(obj, &addr, 0, nil, &size) == noErr, size > 0 else { return [] }
    var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
    guard AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &ids) == noErr else { return [] }
    return ids
}
func getString(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
               _ element: AudioObjectPropertyElement = kAudioObjectPropertyElementMain) -> String? {
    var addr = address(sel, scope, element)
    var s: Unmanaged<CFString>? = nil
    var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    guard AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &s) == noErr, let v = s else { return nil }
    return v.takeRetainedValue() as String
}
func fourcc(_ v: UInt32) -> String {
    if v == 0xFFFF_FFFF { return "*" }
    let bytes = [24, 16, 8, 0].map { UInt8((v >> UInt32($0)) & 0xff) }
    if bytes.allSatisfy({ $0 >= 0x20 && $0 < 0x7f }) {
        return "'" + String(decoding: bytes, as: UTF8.self) + "'"
    }
    return String(v)
}

// A short description of any audio object, for log lines.
func describe(_ obj: AudioObjectID) -> String {
    if obj == sys { return "system" }
    let cls = get(obj, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, AudioClassID(0)).map(fourcc) ?? "?"
    var name = getString(obj, kAudioObjectPropertyName)
    if cls == fourcc(kAudioProcessClassID) {
        let pid = get(obj, kAudioProcessPropertyPID, kAudioObjectPropertyScopeGlobal, pid_t(0)) ?? -1
        name = "pid \(pid) \(getString(obj, kAudioProcessPropertyBundleID) ?? "")"
    } else if name == nil || name == "" {
        // Streams and controls usually have no name: name their owner instead.
        if let owner = get(obj, kAudioObjectPropertyOwner, kAudioObjectPropertyScopeGlobal, AudioObjectID(0)), owner != 0, owner != obj {
            name = "of \(owner) \(getString(owner, kAudioObjectPropertyName) ?? "?")"
        }
    }
    return "\(obj) \(cls) \(name ?? "?")"
}

// Selectors whose value is a CFString (read and released properly, not hex-dumped).
let stringSelectors: Set<AudioObjectPropertySelector> = [
    kAudioObjectPropertyName, kAudioObjectPropertyManufacturer, kAudioObjectPropertyModelName,
    kAudioObjectPropertyElementName, kAudioObjectPropertySerialNumber, kAudioObjectPropertyFirmwareVersion,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyModelUID, kAudioDevicePropertyConfigurationApplication,
    kAudioProcessPropertyBundleID, kAudioBoxPropertyBoxUID,
]

// The current value of a notified property, decoded where the shape is obvious.
func value(_ obj: AudioObjectID, _ a: AudioObjectPropertyAddress) -> String {
    var addr = a
    if listSelectors.contains(a.mSelector) {
        return "(list changed; new objects are logged as \"listening\")"
    }
    if stringSelectors.contains(a.mSelector) {
        return getString(obj, a.mSelector, a.mScope, a.mElement).map { "\"\($0)\"" } ?? "(unreadable)"
    }
    var size: UInt32 = 0
    let st = AudioObjectGetPropertyDataSize(obj, &addr, 0, nil, &size)
    guard st == noErr else { return "(size status \(fourcc(UInt32(bitPattern: st))))" }
    guard size > 0 else { return "(empty)" }
    guard size <= 256 else { return "(\(size) bytes)" }
    var bytes = [UInt8](repeating: 0, count: Int(size))
    let rst = AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &bytes)
    guard rst == noErr else { return "(read status \(fourcc(UInt32(bitPattern: rst))))" }
    bytes = Array(bytes.prefix(Int(size)))
    return bytes.withUnsafeBytes { raw -> String in
        switch bytes.count {
        case 4:
            let v = raw.loadUnaligned(as: UInt32.self)
            let f = raw.loadUnaligned(as: Float32.self)
            return "u32=\(v) \(fourcc(v)) f32=\(f)"
        case 8:
            return "f64=\(raw.loadUnaligned(as: Float64.self)) u64=\(raw.loadUnaligned(as: UInt64.self))"
        case MemoryLayout<AudioStreamBasicDescription>.size:
            let d = raw.loadUnaligned(as: AudioStreamBasicDescription.self)
            return "asbd rate=\(d.mSampleRate) format=\(fourcc(d.mFormatID)) flags=\(d.mFormatFlags) ch=\(d.mChannelsPerFrame) bits=\(d.mBitsPerChannel)"
        case let n where n % 4 == 0:
            let words = (0..<n / 4).map { raw.loadUnaligned(fromByteOffset: $0 * 4, as: UInt32.self) }
            return "u32[\(words.count)]=" + words.prefix(16).map(String.init).joined(separator: ",")
        default:
            return "hex=" + bytes.map { String(format: "%02x", $0) }.joined()
        }
    }
}

// MARK: Wildcard listeners

let queue = DispatchQueue(label: "handoff-listener")
var registered: Set<AudioObjectID> = []

// Every object reachable from the system object: owned objects recursively,
// plus the lists the system object keeps (in case they are not "owned").
func allObjects() -> Set<AudioObjectID> {
    var found: Set<AudioObjectID> = [sys]
    var pending: [AudioObjectID] = [sys]
    pending += getArray(sys, kAudioHardwarePropertyDevices)
    pending += getArray(sys, kAudioHardwarePropertyBoxList)
    pending += getArray(sys, kAudioHardwarePropertyClockDeviceList)
    pending += getArray(sys, kAudioHardwarePropertyProcessObjectList)
    pending += getArray(sys, kAudioHardwarePropertyPlugInList)
    while let o = pending.popLast() {
        found.insert(o)
        for child in getArray(o, kAudioObjectPropertyOwnedObjects) where !found.contains(child) {
            pending.append(child)
        }
    }
    return found
}

// Registers listeners on objects not yet registered. Runs on `queue`.
func registerAll() {
    for obj in allObjects().subtracting(registered).sorted() {
        var addr = address(kAudioObjectPropertySelectorWildcard, kAudioObjectPropertyScopeWildcard, kAudioObjectPropertyElementWildcard)
        let st = AudioObjectAddPropertyListenerBlock(obj, &addr, queue) { count, addresses in
            notified(obj, Array(UnsafeBufferPointer(start: addresses, count: Int(count))))
        }
        if st == noErr {
            registered.insert(obj)
            log("listening: \(describe(obj))")
        } else {
            log("could not listen on \(describe(obj)): status \(fourcc(UInt32(bitPattern: st)))")
        }
    }
}

let listSelectors: Set<AudioObjectPropertySelector> = [
    kAudioObjectPropertyOwnedObjects, kAudioHardwarePropertyDevices, kAudioHardwarePropertyBoxList,
    kAudioHardwarePropertyClockDeviceList, kAudioHardwarePropertyProcessObjectList, kAudioHardwarePropertyPlugInList,
    kAudioDevicePropertyStreams,
]

func notified(_ obj: AudioObjectID, _ addresses: [AudioObjectPropertyAddress]) {
    let now = Date()
    let who = describe(obj)
    for a in addresses {
        log("notify \(who): sel=\(fourcc(a.mSelector)) scope=\(fourcc(a.mScope)) el=\(a.mElement) -> \(value(obj, a))", at: now)
    }
    if addresses.contains(where: { listSelectors.contains($0.mSelector) }) {
        registerAll()
    }
}

// MARK: Device choice

func inputDevices() -> [AudioObjectID] {
    getArray(sys, kAudioHardwarePropertyDevices).filter { !getArray($0, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput).isEmpty }
}
let device: AudioObjectID = {
    if let arg = deviceArg {
        let match = inputDevices().first { d in
            getString(d, kAudioDevicePropertyDeviceUID) == arg
                || (getString(d, kAudioObjectPropertyName) ?? "").localizedCaseInsensitiveContains(arg)
        }
        guard let d = match else {
            print("no input device matches \"\(arg)\"; input devices:")
            for d in inputDevices() {
                print("  \(getString(d, kAudioObjectPropertyName) ?? "?")  uid=\(getString(d, kAudioDevicePropertyDeviceUID) ?? "?")")
            }
            exit(1)
        }
        return d
    }
    return get(sys, kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyScopeGlobal, AudioObjectID(0)) ?? 0
}()
let deviceUID = getString(device, kAudioDevicePropertyDeviceUID) ?? "?"
log("device: \(describe(device)) uid=\(deviceUID)")

// AirPods' UID is the Bluetooth address plus a suffix ("34-0E-22-09-9C-73:input").
let btAddress: String? = btArg ?? {
    let prefix = String(deviceUID.prefix(17))
    return prefix.range(of: "^([0-9A-Fa-f]{2}-){5}[0-9A-Fa-f]{2}$", options: .regularExpression) != nil ? prefix : nil
}()

// MARK: Tap

// Written by the input callback, read by the reporting timer. Plain memory:
// torn reads only blur a diagnostic line.
struct TapState {
    var callbacks: UInt64 = 0
    var frames: UInt64 = 0
    var renderErrors: UInt64 = 0
    var lastRenderError: OSStatus = 0
    var zero = false              // the last callback's audio was exact zeros
    var zeroChanges: UInt64 = 0   // how many times `zero` flipped
    var zeroChangeHost: UInt64 = 0
    var lastHost: UInt64 = 0
    var maxGapHost: UInt64 = 0    // longest gap between callbacks since last read
    var peak: Float32 = 0         // since last read
}
let tap = UnsafeMutablePointer<TapState>.allocate(capacity: 1)
tap.initialize(to: TapState())
var unit: AudioUnit?
let maxFrames: UInt32 = 8192
var channels: UInt32 = 1
var sampleBuffer: UnsafeMutablePointer<Float32>!
var bufferList: UnsafeMutableAudioBufferListPointer!

func check(_ st: OSStatus, _ what: String) {
    if st != noErr {
        print("\(what) failed: \(fourcc(UInt32(bitPattern: st))) (\(st))")
        exit(1)
    }
}

let inputCallback: AURenderCallback = { _, flags, timestamp, bus, frames, _ in
    let byteSize = frames * channels * UInt32(MemoryLayout<Float32>.size)
    bufferList[0].mData = UnsafeMutableRawPointer(sampleBuffer)
    bufferList[0].mDataByteSize = byteSize
    bufferList[0].mNumberChannels = channels
    let st = AudioUnitRender(unit!, flags, timestamp, bus, frames, bufferList.unsafeMutablePointer)
    let host = timestamp.pointee.mHostTime
    if tap.pointee.lastHost != 0, host > tap.pointee.lastHost {
        tap.pointee.maxGapHost = max(tap.pointee.maxGapHost, host - tap.pointee.lastHost)
    }
    tap.pointee.lastHost = host
    tap.pointee.callbacks += 1
    guard st == noErr else {
        tap.pointee.renderErrors += 1
        tap.pointee.lastRenderError = st
        return noErr
    }
    tap.pointee.frames += UInt64(frames)
    var peak: Float32 = 0
    for i in 0..<Int(frames * channels) {
        peak = max(peak, abs(sampleBuffer[i]))
    }
    tap.pointee.peak = max(tap.pointee.peak, peak)
    let zero = peak == 0
    if zero != tap.pointee.zero || tap.pointee.callbacks == 1 {
        tap.pointee.zero = zero
        tap.pointee.zeroChanges += 1
        tap.pointee.zeroChangeHost = host
    }
    return noErr
}

let unitPropertyNames: [AudioUnitPropertyID: String] = [
    kAudioOutputUnitProperty_IsRunning: "IsRunning", kAudioUnitProperty_StreamFormat: "StreamFormat",
    kAudioOutputUnitProperty_CurrentDevice: "CurrentDevice", kAudioUnitProperty_LastRenderError: "LastRenderError",
    kAudioOutputUnitProperty_ChannelMap: "ChannelMap",
]
let unitPropertyListener: AudioUnitPropertyListenerProc = { _, au, prop, scope, element in
    var size: UInt32 = 0
    var writable: DarwinBoolean = false
    AudioUnitGetPropertyInfo(au, prop, scope, element, &size, &writable)
    var detail = ""
    if size == 4 {
        var v: UInt32 = 0
        AudioUnitGetProperty(au, prop, scope, element, &v, &size)
        detail = "u32=\(v) \(fourcc(v))"
    } else if size == UInt32(MemoryLayout<AudioStreamBasicDescription>.size) {
        var d = AudioStreamBasicDescription()
        AudioUnitGetProperty(au, prop, scope, element, &d, &size)
        detail = "asbd rate=\(d.mSampleRate) ch=\(d.mChannelsPerFrame)"
    }
    let now = Date()
    let name = unitPropertyNames[prop] ?? fourcc(prop)
    queue.async { log("unit \(name) scope=\(scope) el=\(element) \(detail)", at: now) }
}

// Feeds silence to the voice-processing unit's output element.
let silentRender: AURenderCallback = { _, flags, _, _, _, ioData in
    for buffer in UnsafeMutableAudioBufferListPointer(ioData!) {
        memset(buffer.mData, 0, Int(buffer.mDataByteSize))
    }
    flags.pointee.insert(.unitRenderAction_OutputIsSilence)
    return noErr
}

func startTap() {
    let subType = voiceProcessing ? kAudioUnitSubType_VoiceProcessingIO : kAudioUnitSubType_HALOutput
    var desc = AudioComponentDescription(componentType: kAudioUnitType_Output, componentSubType: subType,
                                         componentManufacturer: kAudioUnitManufacturer_Apple, componentFlags: 0, componentFlagsMask: 0)
    guard let comp = AudioComponentFindNext(nil, &desc) else { print("no \(voiceProcessing ? "voice-processing unit" : "AUHAL")"); exit(1) }
    check(AudioComponentInstanceNew(comp, &unit), "AudioComponentInstanceNew")
    let u = unit!
    var one: UInt32 = 1, zero: UInt32 = 0
    let u32 = UInt32(MemoryLayout<UInt32>.size)
    check(AudioUnitSetProperty(u, kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Input, 1, &one, u32), "enable input")
    var dev = device
    let devSize = UInt32(MemoryLayout<AudioObjectID>.size)
    if voiceProcessing {
        // The voice-processing unit keeps its output element (it cancels echo
        // from it); feed it silence. It may refuse a device other than the
        // default input: log that and carry on.
        var cb = AURenderCallbackStruct(inputProc: silentRender, inputProcRefCon: nil)
        check(AudioUnitSetProperty(u, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &cb,
                                   UInt32(MemoryLayout<AURenderCallbackStruct>.size)), "render callback")
        let st = AudioUnitSetProperty(u, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 1, &dev, devSize)
        if st != noErr {
            log("voice processing: setting the input device failed (\(st)); it records from the default input")
        }
    } else {
        check(AudioUnitSetProperty(u, kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Output, 0, &zero, u32), "disable output")
        check(AudioUnitSetProperty(u, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &dev, devSize), "set device")
    }

    // Deliver the device's own rate and channel count as interleaved f32
    // (mono for voice processing, which mixes the input down).
    var deviceFormat = AudioStreamBasicDescription()
    var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
    check(AudioUnitGetProperty(u, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 1, &deviceFormat, &size), "device format")
    channels = voiceProcessing ? 1 : deviceFormat.mChannelsPerFrame
    var client = AudioStreamBasicDescription(
        mSampleRate: deviceFormat.mSampleRate, mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 4 * channels, mFramesPerPacket: 1, mBytesPerFrame: 4 * channels,
        mChannelsPerFrame: channels, mBitsPerChannel: 32, mReserved: 0)
    check(AudioUnitSetProperty(u, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 1, &client, size), "client format")
    if voiceProcessing {
        check(AudioUnitSetProperty(u, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0, &client, size), "output element format")
    }
    var max = maxFrames
    check(AudioUnitSetProperty(u, kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global, 0, &max, u32), "max frames")
    sampleBuffer = .allocate(capacity: Int(maxFrames * channels))
    bufferList = AudioBufferList.allocate(maximumBuffers: 1)

    var cb = AURenderCallbackStruct(inputProc: inputCallback, inputProcRefCon: nil)
    check(AudioUnitSetProperty(u, kAudioOutputUnitProperty_SetInputCallback, kAudioUnitScope_Global, 0, &cb,
                               UInt32(MemoryLayout<AURenderCallbackStruct>.size)), "input callback")
    for prop in [kAudioOutputUnitProperty_IsRunning, kAudioUnitProperty_StreamFormat, kAudioOutputUnitProperty_CurrentDevice,
                 kAudioUnitProperty_LastRenderError, kAudioOutputUnitProperty_ChannelMap] {
        check(AudioUnitAddPropertyListener(u, prop, unitPropertyListener, nil), "unit listener \(fourcc(prop))")
    }
    check(AudioUnitInitialize(u), "AudioUnitInitialize")
    check(AudioOutputUnitStart(u), "AudioOutputUnitStart")
    var current = AudioObjectID(0)
    var currentSize = devSize
    AudioUnitGetProperty(u, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, voiceProcessing ? 1 : 0, &current, &currentSize)
    log("tap started\(voiceProcessing ? " (voice processing)" : ""): \(describe(current)), \(deviceFormat.mSampleRate) Hz, \(channels) ch")
}

// Reports the tap's state changes, and a heartbeat every 5 s.
var lastZeroChanges: UInt64 = 0
var lastRenderErrors: UInt64 = 0
var lastHeartbeat = Date()
var heartbeatCallbacks: UInt64 = 0
func reportTap() {
    let s = tap.pointee
    if s.zeroChanges != lastZeroChanges {
        let skipped = s.zeroChanges - lastZeroChanges - 1
        log("tap: audio is \(s.zero ? "EXACT ZEROS" : "real (non-zero)")" + (skipped > 0 ? " (\(skipped) more flips since last report)" : ""),
            at: date(fromHost: s.zeroChangeHost))
        lastZeroChanges = s.zeroChanges
    }
    if s.renderErrors != lastRenderErrors {
        log("tap: \(s.renderErrors - lastRenderErrors) render errors, last \(fourcc(UInt32(bitPattern: s.lastRenderError))) (\(s.lastRenderError))")
        lastRenderErrors = s.renderErrors
    }
    let gap = Double(s.maxGapHost) * Double(timebase.numer) / Double(timebase.denom) / 1e6
    if gap > 100 {
        log(String(format: "tap: callback gap %.0f ms", gap))
    }
    tap.pointee.maxGapHost = 0
    let now = Date()
    if s.callbacks == heartbeatCallbacks, s.callbacks > 0, now.timeIntervalSince(lastHeartbeat) >= 1 {
        log("tap: no callbacks for \(String(format: "%.1f", now.timeIntervalSince(lastHeartbeat))) s")
        lastHeartbeat = now
    } else if now.timeIntervalSince(lastHeartbeat) >= 5 {
        let peakDb = s.peak > 0 ? String(format: "%.1f dBFS", 20 * log10(s.peak)) : "-inf"
        log("tap: \(s.callbacks - heartbeatCallbacks) callbacks, \(s.frames) frames total, peak \(peakDb), \(s.zero ? "zeros" : "real")")
        tap.pointee.peak = 0
        heartbeatCallbacks = s.callbacks
        lastHeartbeat = now
    }
}

// MARK: Playback

// The headset's output device: `--output`, else the output device sharing
// the input's Bluetooth address, else the default output.
func outputDevices() -> [AudioObjectID] {
    getArray(sys, kAudioHardwarePropertyDevices).filter { !getArray($0, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput).isEmpty }
}
func findOutputDevice() -> AudioObjectID {
    if let arg = outputArg {
        guard let d = outputDevices().first(where: { d in
            getString(d, kAudioDevicePropertyDeviceUID) == arg
                || (getString(d, kAudioObjectPropertyName) ?? "").localizedCaseInsensitiveContains(arg)
        }) else {
            print("no output device matches \"\(arg)\"")
            exit(1)
        }
        return d
    }
    if let addr = btAddress, let d = outputDevices().first(where: { (getString($0, kAudioDevicePropertyDeviceUID) ?? "").hasPrefix(addr) }) {
        return d
    }
    return get(sys, kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal, AudioObjectID(0)) ?? 0
}

var outputUnit: AudioUnit?
var tonePhase: Double = 0
var toneStep: Double = 0
var outputChannels: UInt32 = 2
// Silence, or a quiet 440 Hz tone (-30 dBFS).
let playRender: AURenderCallback = { _, flags, _, _, frames, ioData in
    let buffers = UnsafeMutableAudioBufferListPointer(ioData!)
    guard play == .tone else {
        for b in buffers { memset(b.mData, 0, Int(b.mDataByteSize)) }
        flags.pointee.insert(.unitRenderAction_OutputIsSilence)
        return noErr
    }
    let out = buffers[0].mData!.assumingMemoryBound(to: Float32.self)
    for i in 0..<Int(frames) {
        let v = Float32(0.0316 * sin(tonePhase))
        tonePhase += toneStep
        for c in 0..<Int(outputChannels) { out[i * Int(outputChannels) + c] = v }
    }
    return noErr
}

func startPlayback() {
    var desc = AudioComponentDescription(componentType: kAudioUnitType_Output, componentSubType: kAudioUnitSubType_HALOutput,
                                         componentManufacturer: kAudioUnitManufacturer_Apple, componentFlags: 0, componentFlagsMask: 0)
    guard let comp = AudioComponentFindNext(nil, &desc) else { print("no AUHAL"); exit(1) }
    check(AudioComponentInstanceNew(comp, &outputUnit), "output AudioComponentInstanceNew")
    let u = outputUnit!
    var dev = findOutputDevice()
    check(AudioUnitSetProperty(u, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &dev,
                               UInt32(MemoryLayout<AudioObjectID>.size)), "set output device")
    var deviceFormat = AudioStreamBasicDescription()
    var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
    check(AudioUnitGetProperty(u, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 0, &deviceFormat, &size), "output device format")
    outputChannels = deviceFormat.mChannelsPerFrame
    toneStep = 2 * Double.pi * 440 / deviceFormat.mSampleRate
    var client = AudioStreamBasicDescription(
        mSampleRate: deviceFormat.mSampleRate, mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 4 * outputChannels, mFramesPerPacket: 1, mBytesPerFrame: 4 * outputChannels,
        mChannelsPerFrame: outputChannels, mBitsPerChannel: 32, mReserved: 0)
    check(AudioUnitSetProperty(u, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0, &client, size), "output client format")
    var cb = AURenderCallbackStruct(inputProc: playRender, inputProcRefCon: nil)
    check(AudioUnitSetProperty(u, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &cb,
                               UInt32(MemoryLayout<AURenderCallbackStruct>.size)), "output render callback")
    check(AudioUnitInitialize(u), "output AudioUnitInitialize")
    check(AudioOutputUnitStart(u), "output AudioOutputUnitStart")
    log("playback started (\(play!.rawValue)): \(describe(dev)), \(deviceFormat.mSampleRate) Hz, \(outputChannels) ch")
}

func stopPlayback() {
    guard let u = outputUnit else { return }
    AudioOutputUnitStop(u)
    AudioUnitUninitialize(u)
    AudioComponentInstanceDispose(u)
    outputUnit = nil
    log("playback stopped")
}

// MARK: Bluetooth

var lastBluetooth = ""
func pollBluetooth() {
    guard let addr = btAddress, let bt = IOBluetoothDevice(addressString: addr) else { return }
    let state = "connected=\(bt.isConnected()) name=\(bt.name ?? "?")"
    if state != lastBluetooth {
        log("bluetooth \(addr): \(state)")
        lastBluetooth = state
    }
}

// MARK: Main

queue.sync { registerAll() }
if tapEnabled { startTap() }
if play != nil {
    log("playback starts in \(playAfter) s" + (playFor.map { ", stops \($0) s later" } ?? ""))
    // Not on `queue`: starting and stopping a unit waits for HAL notifications delivered there.
    let playQueue = DispatchQueue(label: "play")
    playQueue.asyncAfter(deadline: .now() + playAfter) {
        startPlayback()
        if let d = playFor {
            playQueue.asyncAfter(deadline: .now() + d) { stopPlayback() }
        }
    }
}
let timer = DispatchSource.makeTimerSource(queue: queue)
timer.schedule(deadline: .now(), repeating: .milliseconds(50))
timer.setEventHandler {
    if tapEnabled { reportTap() }
    pollBluetooth()
}
timer.resume()
signal(SIGINT, SIG_IGN)
// Not on `queue`: stopping the unit waits for HAL notifications delivered there.
let sigint = DispatchSource.makeSignalSource(signal: SIGINT, queue: DispatchQueue(label: "sigint"))
sigint.setEventHandler {
    if let u = unit { AudioOutputUnitStop(u) }
    if let u = outputUnit { AudioOutputUnitStop(u) }
    queue.sync { log("stopped") }
    exit(0)
}
sigint.resume()
log("running; Ctrl-C to stop")
dispatchMain()
