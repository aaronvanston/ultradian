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
            // Nothing looks symbols up in the executable itself.
            println!("cargo:rustc-link-arg-bins=-Wl,-no_exported_symbols");
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

/// Writes src/catalog.json into OUT_DIR twice: as JSON without the
/// whitespace between tokens, which `schema` and `describe` print, and as
/// Rust statics the command-line parser is built from, so no launch has to
/// parse JSON just to read argv.
fn catalog() {
    println!("cargo:rerun-if-changed=src/catalog.json");
    let text = std::fs::read_to_string("src/catalog.json").expect("src/catalog.json is readable");
    let catalog: serde_json::Value =
        serde_json::from_str(&text).expect("src/catalog.json is valid JSON");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join("catalog.json"), catalog.to_string()).expect("OUT_DIR is writable");
    std::fs::write(out.join("catalog.rs"), catalog_statics(&catalog)).expect("OUT_DIR is writable");
}

/// The catalog's commands as a `&[CatalogCommand]` expression.
fn catalog_statics(catalog: &serde_json::Value) -> String {
    use serde_json::Value;
    let text = |value: &Value| format!("{:?}", value.as_str().expect("a string"));
    let optional = |value: Option<&Value>| match value.and_then(Value::as_str) {
        Some(value) => format!("Some({value:?})"),
        None => "None".to_owned(),
    };
    let list = |value: &Value| {
        let items: Vec<String> = value.as_array().expect("a list").iter().map(text).collect();
        format!("&[{}]", items.join(", "))
    };
    let flag = |value: &Value, key: &str| value[key].as_bool().expect("a boolean");
    let mut out = String::from("&[\n");
    for command in catalog["commands"].as_array().expect("commands") {
        let arguments: Vec<String> = command["arguments"]
            .as_array()
            .expect("arguments")
            .iter()
            .map(|argument| {
                format!(
                    "CatalogArgument {{ name: {}, required: {}, variadic: {}, description: {} }}",
                    text(&argument["name"]),
                    flag(argument, "required"),
                    flag(argument, "variadic"),
                    optional(argument.get("description")),
                )
            })
            .collect();
        let options: Vec<String> = command["options"]
            .as_array()
            .expect("options")
            .iter()
            .map(|option| {
                let choices = match option.get("choices") {
                    Some(choices) if !choices.is_null() => format!("Some({})", list(choices)),
                    _ => "None".to_owned(),
                };
                // Kept as JSON text: a default may be any JSON value.
                let default = match option.get("defaultValue") {
                    Some(value) if !value.is_null() => format!("Some({:?})", value.to_string()),
                    _ => "None".to_owned(),
                };
                format!(
                    "CatalogOption {{ flags: {}, description: {}, choices: {choices}, default_json: {default} }}",
                    text(&option["flags"]),
                    text(&option["description"]),
                )
            })
            .collect();
        out.push_str(&format!(
            "CatalogCommand {{ path: {}, aliases: {}, module: {}, summary: {}, description: {}, examples: {}, arguments: &[{}], options: &[{}] }},\n",
            list(&command["path"]),
            list(&command["aliases"]),
            text(&command["module"]),
            text(&command["summary"]),
            optional(command.get("description")),
            list(&command["examples"]),
            arguments.join(", "),
            options.join(", "),
        ));
    }
    out.push(']');
    out
}

/// The tz database chrono-tz embeds, as plain arrays for `src/zones.rs`:
/// every zone and link name, and each zone's UTC offset over time. It is
/// computed from the tz files chrono-tz ships, with the same parser and the
/// same transitions chrono-tz's own build generates, so every answer
/// matches it (a test checks every zone against chrono-tz). chrono-tz
/// stores a name and a pointer per transition, a megabyte of the binary;
/// these arrays are a third of that and need no relocation at launch.
fn zones() {
    use parse_zoneinfo::line::Line;
    use parse_zoneinfo::table::TableBuilder;
    use parse_zoneinfo::transitions::TableTransitions;

    let tz = chrono_tz_dir().join("tz");
    let mut builder = TableBuilder::new();
    for file in parse_zoneinfo::FILES {
        let path = tz.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default();
            builder
                .add_line(Line::new(line).expect("a tz line"))
                .expect("a consistent tz table");
        }
    }
    let table = builder.build();
    let mut names: Vec<&String> = table.zonesets.keys().chain(table.links.keys()).collect();
    // Sorted ignoring ASCII case, the order zones.rs searches in.
    names.sort_by_key(|name| name.to_ascii_lowercase());
    names.dedup_by_key(|name| name.to_ascii_lowercase());
    assert_eq!(
        names.len(),
        table.zonesets.len() + table.links.len(),
        "two zone names differ only in case"
    );

    // Per zone: the offset before its first transition, then each
    // transition and the offset from then on. Transitions that keep the
    // total offset are dropped, and zones with the same history (links)
    // share one run.
    let mut times: Vec<i64> = Vec::new();
    let mut offsets: Vec<i64> = Vec::new();
    // (offset before, transitions) -> (first transition, count)
    type History = (i64, Vec<(i64, i64)>);
    let mut runs: std::collections::HashMap<History, (usize, usize)> =
        std::collections::HashMap::new();
    let mut zones = String::new();
    let mut joined = String::new();
    for name in &names {
        let spans = table.timespans(name).expect("every name has timespans");
        let first = spans.first.utc_offset + spans.first.dst_offset;
        let mut history: Vec<(i64, i64)> = Vec::new();
        let mut current = first;
        for (start, span) in &spans.rest {
            let offset = span.utc_offset + span.dst_offset;
            if offset != current {
                history.push((*start, offset));
                current = offset;
            }
        }
        let (start, len) = *runs.entry((first, history.clone())).or_insert_with(|| {
            let start = times.len();
            for (time, offset) in &history {
                times.push(*time);
                offsets.push(*offset);
            }
            (start, history.len())
        });
        joined.push_str(name);
        let first_time = times.get(start).copied().unwrap_or_default();
        zones.push_str(&format!(
            "({}, {first}, {start}, {len}, {first_time}), ",
            joined.len()
        ));
    }
    let list = |values: &[i64]| {
        values
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    // Each transition is stored as the seconds since the one before it in
    // its zone (the first as 0, its time being in ZONES): four bytes where
    // the times themselves, 1844 to 2099, need eight.
    let deltas: Vec<i64> = (0..times.len())
        .map(|index| {
            let starts_run = runs.values().any(|&(start, _)| start == index);
            let delta = if starts_run {
                0
            } else {
                times[index] - times[index - 1]
            };
            i64::from(u32::try_from(delta).expect("transitions less than 136 years apart"))
        })
        .collect();
    // A zone's offsets come from a short list (about 120 values), so each
    // transition stores a one-byte index into it.
    let mut distinct = offsets.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let indexes: Vec<i64> = offsets
        .iter()
        .map(|offset| {
            let index = distinct.binary_search(offset).expect("listed");
            i64::from(u8::try_from(index).expect("at most 256 distinct offsets"))
        })
        .collect();
    let rust = format!(
        "/// Every name, joined; ZONES holds where each ends.\n\
         pub const NAMES: &str = {joined:?};\n\
         /// (end of the name in NAMES, offset before the first transition,\n\
         /// first transition, transition count, time of the first\n\
         /// transition), sorted by lowercase name.\n\
         pub static ZONES: [(u32, i32, u32, u32, i64); {}] = [{zones}];\n\
         /// Seconds from each transition's predecessor in its zone.\n\
         pub static DELTAS: [u32; {}] = [{}];\n\
         /// Each transition's offset, as an index into OFFSET_VALUES.\n\
         pub static OFFSETS: [u8; {}] = [{}];\n\
         pub static OFFSET_VALUES: [i32; {}] = [{}];\n",
        names.len(),
        deltas.len(),
        list(&deltas),
        indexes.len(),
        list(&indexes),
        distinct.len(),
        list(&distinct),
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join("zones.rs"), rust).expect("OUT_DIR is writable");
}

/// Where cargo unpacked chrono-tz, whose tz files `zones` reads.
fn chrono_tz_dir() -> std::path::PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata prints JSON");
    let manifest = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|package| package["name"] == "chrono-tz")
        .and_then(|package| package["manifest_path"].as_str())
        .expect("chrono-tz is in the dependency graph");
    std::path::Path::new(manifest)
        .parent()
        .expect("a manifest has a folder")
        .to_path_buf()
}

fn main() {
    link_args();
    catalog();
    zones();
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
