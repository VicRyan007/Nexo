fn main() {
    println!("cargo:rerun-if-changed=c/vpx_codec.c");
    println!("cargo:rerun-if-changed=build.rs");

    cc::Build::new()
        .file("c/vpx_codec.c")
        .warnings(false)
        .compile("nexo_vpx");
}
