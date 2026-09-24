import CoreAudio
import Foundation
import IOBluetooth

// Polls every 100 ms and prints a timestamped line whenever anything about
// the input devices, their users, or a Bluetooth device's connection changes.
let btAddress = CommandLine.arguments.dropFirst().first ?? "34-0E-22-09-9C-73"

func get<T>(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope, _ value: T) -> T? {
    var addr = AudioObjectPropertyAddress(mSelector: sel, mScope: scope, mElement: kAudioObjectPropertyElementMain)
    var v = value
    var size = UInt32(MemoryLayout<T>.size)
    return AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &v) == noErr ? v : nil
}
func getArray(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> [AudioObjectID] {
    var addr = AudioObjectPropertyAddress(mSelector: sel, mScope: scope, mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(obj, &addr, 0, nil, &size) == noErr else { return [] }
    var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
    guard AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &ids) == noErr else { return [] }
    return ids
}
func getString(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector) -> String {
    var addr = AudioObjectPropertyAddress(mSelector: sel, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
    var s: Unmanaged<CFString>? = nil
    var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    guard AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &s) == noErr, let v = s else { return "?" }
    return v.takeRetainedValue() as String
}
let sys = AudioObjectID(kAudioObjectSystemObject)
func u32(_ o: AudioObjectID, _ s: AudioObjectPropertySelector, _ sc: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> String {
    get(o, s, sc, UInt32(0)).map { String($0) } ?? "-"
}

func snapshot() -> [String: String] {
    var out: [String: String] = [:]
    let defaultIn: AudioObjectID = get(sys, kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyScopeGlobal, AudioObjectID(0)) ?? 0
    out["default input"] = getString(defaultIn, kAudioObjectPropertyName)
    for d in getArray(sys, kAudioHardwarePropertyDevices) {
        let streams = getArray(d, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput)
        if streams.isEmpty { continue }
        let name = getString(d, kAudioObjectPropertyName)
        let rate = get(d, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, Float64(0)) ?? 0
        let streamActive = streams.map { u32($0, kAudioStreamPropertyIsActive) }.joined(separator: ",")
        let mute = u32(d, kAudioDevicePropertyMute, kAudioObjectPropertyScopeInput)
        out["dev \(name)"] = "id=\(d) alive=\(u32(d, kAudioDevicePropertyDeviceIsAlive)) runningSomewhere=\(u32(d, kAudioDevicePropertyDeviceIsRunningSomewhere)) running=\(u32(d, kAudioDevicePropertyDeviceIsRunning)) rate=\(rate) streams=\(streams.count) streamActive=\(streamActive) mute=\(mute) jackConnected=\(u32(d, kAudioDevicePropertyJackIsConnected, kAudioObjectPropertyScopeInput))"
    }
    var users: [String] = []
    for p in getArray(sys, kAudioHardwarePropertyProcessObjectList) {
        if (get(p, kAudioProcessPropertyIsRunningInput, kAudioObjectPropertyScopeGlobal, UInt32(0)) ?? 0) != 0 {
            let pid = get(p, kAudioProcessPropertyPID, kAudioObjectPropertyScopeGlobal, pid_t(0)) ?? -1
            users.append("\(pid):\(getString(p, kAudioProcessPropertyBundleID))")
        }
    }
    out["input users"] = users.sorted().joined(separator: " ")
    if let bt = IOBluetoothDevice(addressString: btAddress) {
        out["bluetooth \(btAddress)"] = "connected=\(bt.isConnected()) name=\(bt.name ?? "?")"
    }
    return out
}

let fmt = ISO8601DateFormatter()
fmt.formatOptions = [.withTime, .withFractionalSeconds, .withColonSeparatorInTime]
var last: [String: String] = [:]
setvbuf(stdout, nil, _IOLBF, 0)
while true {
    let now = snapshot()
    for key in Set(now.keys).union(last.keys).sorted() where now[key] != last[key] {
        print("\(fmt.string(from: Date()))Z  \(key): \(now[key] ?? "(gone)")")
    }
    last = now
    Thread.sleep(forTimeInterval: 0.1)
}
