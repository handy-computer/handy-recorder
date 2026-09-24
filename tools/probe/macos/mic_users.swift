import CoreAudio
import Foundation

func get<T>(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal, _ value: T) -> T? {
    var addr = AudioObjectPropertyAddress(mSelector: sel, mScope: scope, mElement: kAudioObjectPropertyElementMain)
    var v = value
    var size = UInt32(MemoryLayout<T>.size)
    let st = AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &v)
    return st == noErr ? v : nil
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
func fourcc(_ v: UInt32) -> String {
    let b = [24, 16, 8, 0].map { Character(UnicodeScalar(UInt8((v >> UInt32($0)) & 0xff))) }
    return String(b)
}

print("processes using audio input:")
var any = false
for p in getArray(AudioObjectID(kAudioObjectSystemObject), kAudioHardwarePropertyProcessObjectList) {
    let input: UInt32 = get(p, kAudioProcessPropertyIsRunningInput, kAudioObjectPropertyScopeGlobal, UInt32(0)) ?? 0
    if input != 0 {
        any = true
        let pid: pid_t = get(p, kAudioProcessPropertyPID, kAudioObjectPropertyScopeGlobal, pid_t(0)) ?? -1
        print("  pid \(pid)  \(getString(p, kAudioProcessPropertyBundleID))")
    }
}
if !any { print("  none") }

print("input devices:")
for d in getArray(AudioObjectID(kAudioObjectSystemObject), kAudioHardwarePropertyDevices) {
    let streams = getArray(d, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput)
    if streams.isEmpty { continue }
    let alive: UInt32 = get(d, kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal, UInt32(0)) ?? 0
    let running: UInt32 = get(d, kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioObjectPropertyScopeGlobal, UInt32(0)) ?? 0
    let transport: UInt32 = get(d, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal, UInt32(0)) ?? 0
    let rate: Float64 = get(d, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, Float64(0)) ?? 0
    print("  \(getString(d, kAudioObjectPropertyName)): alive=\(alive) runningSomewhere=\(running) transport=\(fourcc(transport)) rate=\(rate)")
}
