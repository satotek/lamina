fn main() {
    println!("cargo:rerun-if-changed=resources/icon/strata.ico");

    // GPUI's Windows backend gives every window the executable's icon
    // resource with ID 1, which is also what Explorer and the taskbar show.
    // The resource compiler only exists on a Windows host, and cross-compiling
    // the app to Windows is not supported, so the host check is enough.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("resources/icon/strata.ico")
            .compile()
            .expect("embed the application icon");
    }
}
