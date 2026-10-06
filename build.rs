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

fn main() {
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
