fn main() {
    napi_build::setup();

    // CPAL links two CoreAudio functions that exist only on macOS 14.2+
    // (the handy-recorder README, "macOS < 14.2"). Weak-link CoreAudio so the
    // addon still loads on older macOS; those functions are never called.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
    }
}
