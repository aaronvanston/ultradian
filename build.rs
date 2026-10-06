// Bakes the git commit into the binary for `version`, the way 0.2.1's build
// script defined ULTRADIAN_COMMIT. An explicit ULTRADIAN_COMMIT wins; a
// build outside a git checkout simply has no commit.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Release builds are stripped by the linker rather than by `strip` in the
/// profile: rustc strips macOS binaries with LLVM's objcopy, which leaves
/// chained fixups' string pool misaligned (dyld refuses such dylibs, and
/// `dyld_info` such executables). On Linux this is the flag rustc's own
/// stripping passes.
fn link_args() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let target = |name: &str| std::env::var(format!("CARGO_CFG_TARGET_{name}")).unwrap_or_default();
    match target("OS").as_str() {
        "macos" => {
            println!("cargo:rustc-link-arg-bins=-Wl,-x,-S");
            // Chained fixups let dyld slide each pointer when its page is
            // first touched (page-in linking) instead of walking all ~33,000
            // of them, mostly chrono-tz's zone tables, on every launch.
            // Apple Silicon's dyld reads them from macOS 11, the target's
            // minimum; Intel builds keep supporting older systems.
            if target("ARCH") == "aarch64" {
                println!("cargo:rustc-link-arg-bins=-Wl,-fixup_chains");
            }
        }
        "linux" => println!("cargo:rustc-link-arg-bins=-Wl,--strip-all"),
        _ => {}
    }
}

/// Writes src/catalog.json to OUT_DIR without the whitespace between
/// tokens. The binary parses the catalog on every launch to build its
/// parser, and indentation was more than half of the text to scan.
fn minify_catalog() {
    println!("cargo:rerun-if-changed=src/catalog.json");
    let text = std::fs::read_to_string("src/catalog.json").expect("src/catalog.json is readable");
    let mut out = String::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    for character in text.chars() {
        if in_string {
            in_string = escaped || character != '"';
            escaped = !escaped && character == '\\';
        } else if character == '"' {
            in_string = true;
        } else if character.is_ascii_whitespace() {
            continue;
        }
        out.push(character);
    }
    let path = std::path::Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("catalog.json");
    std::fs::write(path, out).expect("OUT_DIR is writable");
}

fn main() {
    link_args();
    minify_catalog();
    println!("cargo:rerun-if-env-changed=ULTRADIAN_COMMIT");
    for path in ["HEAD", "logs/HEAD"] {
        if let Some(file) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={file}");
        }
    }
    let commit = std::env::var("ULTRADIAN_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]));
    if let Some(commit) = commit {
        println!("cargo:rustc-env=ULTRADIAN_COMMIT={commit}");
    }
}
