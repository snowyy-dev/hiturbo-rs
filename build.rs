use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let output = Command::new("xcrun")
        .args(["clang", "--print-file-name=libclang_rt.osx.a"])
        .output()
        .expect("Install the Xcode command-line tools with xcode-select --install");
    assert!(
        output.status.success(),
        "Could not locate Apple's Clang runtime"
    );
    let runtime = PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("Clang runtime path is not UTF-8")
            .trim(),
    );
    assert!(
        runtime.is_file(),
        "Clang runtime not found: {}",
        runtime.display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        runtime.parent().unwrap().display()
    );
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
}
