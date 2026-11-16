fn main() {
    // `option_env!("LAMINA_BUILD_ID")` reads rustc's environment. Cargo does
    // not rebuild this crate when the variable changes unless the build script
    // names it, and a build script that only prints `rerun-if-env-changed`
    // leaves the compiled value stale when its other output stays the same.
    // Emitting the token as `rustc-env` puts it in the fingerprint, so a
    // release app and the helper it uploads cannot disagree about the id.
    println!("cargo:rerun-if-env-changed=LAMINA_BUILD_ID");
    let id = std::env::var("LAMINA_BUILD_ID")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "dev".to_owned());
    if id.chars().any(char::is_whitespace) {
        panic!("LAMINA_BUILD_ID must be a single token, got {id:?}");
    }
    println!("cargo:rustc-env=LAMINA_BUILD_ID={id}");
}
