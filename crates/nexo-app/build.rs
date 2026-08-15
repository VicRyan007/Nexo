use std::path::Path;
use std::process::Command;

fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        println!("cargo:rerun-if-changed=resources/nexo.rc");
        println!("cargo:rerun-if-changed=resources/nexo.ico");

        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR must be set");
        let res_file = Path::new(&out_dir).join("nexo_res.o");
        let rc_file = "resources/nexo.rc";

        let status = Command::new("windres")
            .arg("-i")
            .arg(rc_file)
            .arg("-o")
            .arg(&res_file)
            .status();

        if let Ok(s) = status {
            if s.success() {
                println!("cargo:rustc-link-arg-bins={}", res_file.display());
            }
        }
    }
}
