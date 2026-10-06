//! Record ids and one-shot job names, in 0.2.1's shapes: ids are a prefix,
//! the creation time in base 36 padded to nine characters (so they sort by
//! time), and ten random base-36 characters.

const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

fn random_chars(count: usize) -> String {
    let mut bytes = vec![0_u8; count];
    // The OS random source only fails on systems this never runs on; a
    // failure there would surface as repeated ids, which the store refuses.
    let _ = getrandom::fill(&mut bytes);
    bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect()
}

fn base36(mut value: u64) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(ALPHABET[(value % 36) as usize]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

/// `schedule_…`, `job_…` or `run_…`, created now.
pub fn create_id(prefix: &str) -> String {
    create_id_at(prefix, crate::output::now_ms())
}

fn create_id_at(prefix: &str, now_ms: i64) -> String {
    let time = base36(u64::try_from(now_ms).unwrap_or(0));
    format!("{prefix}_{time:0>9}{}", random_chars(10))
}

/// A one-shot job still needs a name, because the name is what its run
/// record and its log directory are filed under: a slug of the label plus
/// enough entropy that two invocations never collide.
pub fn create_job_name(label: &str) -> String {
    format!("once-{}-{}", job_slug(label), random_chars(6))
}

fn job_slug(label: &str) -> String {
    // JavaScript's toLowerCase, then runs of anything but [a-z0-9] become
    // one dash, cut to 24 UTF-16 units, then edge dashes trimmed.
    let mut slug = String::new();
    let mut in_gap = false;
    for character in label.to_lowercase().chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
            in_gap = false;
        } else if !in_gap {
            slug.push('-');
            in_gap = true;
        }
    }
    slug.truncate(24);
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "job".into()
    } else {
        slug.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_sort_by_creation_time() {
        let id = create_id_at("run", 1_786_012_200_000);
        assert_eq!(&id[..13], "run_0mshdjtc0");
        assert_eq!(id.len(), "run_".len() + 19);
        assert!(id[4..].bytes().all(|byte| ALPHABET.contains(&byte)));
        assert!(create_id_at("run", 1) < create_id_at("run", 36));
    }

    #[test]
    fn job_names_slug_their_label() {
        assert_eq!(job_slug("deploy.sh"), "deploy-sh");
        assert_eq!(job_slug("  Import Users!  "), "import-users");
        assert_eq!(job_slug("???"), "job");
        assert_eq!(
            job_slug("a-very-long-label-that-keeps-going"),
            "a-very-long-label-that-k"
        );
        let name = create_job_name("true");
        assert!(name.starts_with("once-true-") && name.len() == "once-true-".len() + 6);
    }
}
