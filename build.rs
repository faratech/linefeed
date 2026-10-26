use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The year the project started, for the LegalCopyright range.
const FIRST_YEAR: i64 = 2025;

/// The last year of the LegalCopyright range when the build is not pinned by
/// SOURCE_DATE_EPOCH. A constant, not the build clock, so one commit always
/// builds to the same bytes. Bump it with the first release of a new year.
const FALLBACK_YEAR: i64 = 2026;

fn llvm_mingw_tool(tool: &str) -> String {
    if let Ok(home) = env::var("LLVM_MINGW_HOME") {
        let path = PathBuf::from(home).join("bin").join(tool);
        return path.to_string_lossy().into_owned();
    }
    tool.to_string()
}

/// Resource script with the icon and VERSIONINFO. The string fields keep the
/// full version (including any prerelease suffix).
fn resource_script(icon_dir: &str, version: &str, numeric: [u16; 3], year: i64) -> String {
    let [major, minor, patch] = numeric;
    let copyright = if year > FIRST_YEAR {
        format!("{FIRST_YEAR}-{year}")
    } else {
        FIRST_YEAR.to_string()
    };
    format!(
        r#"1 ICON "{icon_dir}/assets/linefeed.ico"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904E4"
        BEGIN
            VALUE "ProductName", "Linefeed"
            VALUE "FileDescription", "Linefeed IRC Client"
            VALUE "FileVersion", "{version}"
            VALUE "ProductVersion", "{version}"
            VALUE "LegalCopyright", "Copyright {copyright}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1252
    END
END
"#
    )
}

/// Calendar year (UTC) of a Unix timestamp (Hinnant's civil-from-days).
fn year_from_epoch(seconds: u64) -> i64 {
    let z = (seconds / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    // The shifted calendar starts in March, so January and February (index 10
    // and 11) belong to the following civil year.
    year_of_era + era * 400 + i64::from(month_index >= 10)
}

/// Year for the copyright range: SOURCE_DATE_EPOCH when it is set and valid.
fn build_year(source_date_epoch: Option<&str>) -> i64 {
    source_date_epoch
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(FALLBACK_YEAR, year_from_epoch)
}

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
        llvm_mingw_tool("aarch64-w64-mingw32-windres")
    } else if target.contains("i686") && target.contains("gnullvm") {
        llvm_mingw_tool("i686-w64-mingw32-windres")
    } else if target.contains("x86_64") && target.contains("gnullvm") {
        llvm_mingw_tool("x86_64-w64-mingw32-windres")
    } else {
        "windres".to_string()
    };

    // Create resource script; VERSIONINFO tracks the version in Cargo.toml.
    // FILEVERSION takes plain numbers, so a prerelease ("0.2.0-rc1") or build
    // metadata suffix must not leak into it: cargo pre-splits the numeric parts.
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = ["MAJOR", "MINOR", "PATCH"].map(|part| {
        env::var(format!("CARGO_PKG_VERSION_{part}"))
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0)
    });
    let year = build_year(env::var("SOURCE_DATE_EPOCH").ok().as_deref());
    let rc_path = Path::new(&out_dir).join("linefeed.rc");
    let rc_content = resource_script(
        &manifest_dir.replace('\\', "/"),
        &version,
        numeric,
        year,
    );
    std::fs::write(&rc_path, rc_content).unwrap();

    // Compile resource
    let res_path = Path::new(&out_dir).join("linefeed.res");
    let status = Command::new(&windres)
        .args([
            rc_path.to_str().unwrap(),
            "-O",
            "coff",
            "-o",
            res_path.to_str().unwrap(),
        ])
        .status();

    // Failures use cargo:warning so they are actually visible in build output
    // (eprintln! from a build script is swallowed unless the script fails).
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg={}", res_path.display());
        }
        Ok(s) => {
            println!(
                "cargo:warning=windres failed with status {}: the Windows exe will have no icon/version resources",
                s
            );
        }
        Err(e) => {
            println!(
                "cargo:warning=failed to run windres ({}): {}. The Windows exe will have no icon/version resources; is llvm-mingw on PATH or LLVM_MINGW_HOME set?",
                windres, e
            );
        }
    }

    println!("cargo:rerun-if-changed=assets/linefeed.ico");
    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when the toolchain location changes, so installing/fixing
    // llvm-mingw actually restores the resources on the next build instead of
    // silently reusing the resource-less result until `cargo clean`.
    println!("cargo:rerun-if-env-changed=LLVM_MINGW_HOME");
    println!("cargo:rerun-if-env-changed=PATH");
    // The copyright year is fixed unless SOURCE_DATE_EPOCH pins a build date.
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prerelease_versions_keep_fileversion_numeric() {
        let script = resource_script("/src", "0.2.0-rc1", [0, 2, 0], 2026);
        assert!(script.contains("FILEVERSION 0,2,0,0\n"));
        assert!(script.contains("PRODUCTVERSION 0,2,0,0\n"));
        assert!(script.contains(r#"VALUE "FileVersion", "0.2.0-rc1""#));
        assert!(script.contains(r#"VALUE "ProductVersion", "0.2.0-rc1""#));
    }

    #[test]
    fn copyright_spans_first_year_to_build_year() {
        assert!(resource_script("/src", "1.0.0", [1, 0, 0], 2025).contains("\"Copyright 2025\""));
        assert!(
            resource_script("/src", "1.0.0", [1, 0, 0], 2027).contains("\"Copyright 2025-2027\"")
        );
    }

    #[test]
    fn copyright_year_is_fixed_unless_the_build_is_pinned() {
        // The same commit must build to the same bytes whatever the clock says.
        assert_eq!(build_year(None), FALLBACK_YEAR);
        assert_eq!(build_year(Some("not a number")), FALLBACK_YEAR);
        assert_eq!(build_year(Some("1798761600")), 2027); // 2027-01-01 00:00:00
        assert!(
            resource_script("/src", "1.0.0", [1, 0, 0], build_year(None))
                .contains("\"Copyright 2025-2026\"")
        );
    }

    #[test]
    fn epoch_seconds_map_to_the_utc_year() {
        assert_eq!(year_from_epoch(0), 1970);
        assert_eq!(year_from_epoch(1_767_225_599), 2025); // 2025-12-31 23:59:59
        assert_eq!(year_from_epoch(1_767_225_600), 2026); // 2026-01-01 00:00:00
        assert_eq!(year_from_epoch(1_782_000_000), 2026);
        assert_eq!(year_from_epoch(1_798_761_600), 2027); // 2027-01-01 00:00:00
    }
}
