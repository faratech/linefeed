use std::process::Command;
use std::path::Path;
use std::env;

fn main() {
    // Only compile resources when targeting Windows
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let out_dir = env::var("OUT_DIR").unwrap();
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    // Determine the windres tool based on target
    let windres = if target.contains("aarch64") && target.contains("gnullvm") {
        "/root/toolchains/llvm-mingw/bin/aarch64-w64-mingw32-windres"
    } else if target.contains("i686") && target.contains("gnullvm") {
        "/root/toolchains/llvm-mingw/bin/i686-w64-mingw32-windres"
    } else if target.contains("x86_64") && target.contains("gnullvm") {
        "/root/toolchains/llvm-mingw/bin/x86_64-w64-mingw32-windres"
    } else {
        "windres"
    };

    // Create resource script
    let rc_path = Path::new(&out_dir).join("linefeed.rc");
    let rc_content = format!(
        r#"1 ICON "{}/assets/linefeed.ico"
1 VERSIONINFO
FILEVERSION 0,0,1,0
PRODUCTVERSION 0,0,1,0
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904E4"
        BEGIN
            VALUE "ProductName", "Linefeed"
            VALUE "FileDescription", "Linefeed IRC Client"
            VALUE "FileVersion", "0.0.1"
            VALUE "ProductVersion", "0.0.1"
            VALUE "LegalCopyright", "Copyright 2025"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1252
    END
END
"#,
        manifest_dir.replace('\\', "/")
    );
    std::fs::write(&rc_path, rc_content).unwrap();

    // Compile resource
    let res_path = Path::new(&out_dir).join("linefeed.res");
    let status = Command::new(windres)
        .args([
            rc_path.to_str().unwrap(),
            "-O", "coff",
            "-o", res_path.to_str().unwrap(),
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg={}", res_path.display());
        }
        Ok(s) => {
            eprintln!("Warning: windres failed with status: {}", s);
        }
        Err(e) => {
            eprintln!("Warning: Failed to run windres: {}", e);
        }
    }

    println!("cargo:rerun-if-changed=assets/linefeed.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
