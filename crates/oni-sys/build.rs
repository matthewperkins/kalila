fn main() {
    let dir = "liboni";
    for f in ["oni.c", "onix.c", "onidriverloader.c", "oni.h", "onidefs.h", "onidriver.h"] {
        println!("cargo:rerun-if-changed={dir}/{f}");
    }
    cc::Build::new()
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
