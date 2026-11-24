fn main() {
    let dir = "liboni";
    for f in ["oni.c", "onix.c", "onidriverloader.c", "oni.h", "onidefs.h", "onidriver.h"] {
        println!("cargo:rerun-if-changed={dir}/{f}");
    }
    let mut build = cc::Build::new();
    // liboni includes glibc's <execinfo.h> and uses nothing from it; musl has no such header.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("musl") {
        let compat = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("musl-compat");
        std::fs::create_dir_all(&compat).unwrap();
        std::fs::write(compat.join("execinfo.h"), "/* empty: liboni uses nothing from it */\n").unwrap();
        build.include(compat);
    }
    build
        .files([format!("{dir}/oni.c"), format!("{dir}/onix.c"), format!("{dir}/onidriverloader.c")])
        .include(dir)
        .opt_level(3)
        .flag_if_supported("-fPIC")
        // liboni asserts on internal invariants; keep them out of release builds like upstream.
        .define("NDEBUG", None)
        .warnings(false)
        .compile("oni");
    // liboni dlopen()s `libonidriver_<name>.so` at runtime.
    println!("cargo:rustc-link-lib=dylib=dl");
}
