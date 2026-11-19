// Compile and link the OpenColorIO C++ shim — but ONLY when the `ocio`
// feature is on. With the feature off this build script is a no-op, so
// the crate compiles on machines without the native library.
//
// Build scripts are compiled with the crate's feature cfgs, so
// `cfg!(feature = "ocio")` reflects whether `--features ocio` was passed.
fn main() {
    println!("cargo:rerun-if-changed=src/shim.cpp");
    println!("cargo:rerun-if-changed=build.rs");

    if !cfg!(feature = "ocio") {
        return;
    }

    // pkg-config emits the cargo:rustc-link-search / -link-lib directives
    // for libOpenColorIO and hands back the include paths the shim needs.
    let lib = pkg_config::Config::new().probe("OpenColorIO").expect(
        "feature `ocio` is on but pkg-config could not find OpenColorIO. \
             Install it (macOS: `brew install opencolorio`) and ensure \
             pkg-config can see it (PKG_CONFIG_PATH).",
    );

    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").file("src/shim.cpp");
    for inc in &lib.include_paths {
        build.include(inc);
    }
    build.compile("vance_ocio_shim");
}
