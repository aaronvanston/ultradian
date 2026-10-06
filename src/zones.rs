//! IANA time zones: every zone and link name chrono-tz knows, and each
//! zone's UTC offset over time, compiled by build.rs from the tz files
//! chrono-tz ships into plain arrays. Answers match chrono-tz's exactly;
//! the test below checks every zone against it.

mod data {
    include!(concat!(env!("OUT_DIR"), "/zones.rs"));
}

/// A named IANA zone or link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedZone(usize);

impl NamedZone {
    /// The zone named `name`, ignoring ASCII case as chrono-tz's
    /// `from_str_insensitive` does.
    pub fn find(name: &str) -> Option<Self> {
        let lower = |text: &str| {
            text.bytes()
                .map(|byte| byte.to_ascii_lowercase())
                .collect::<Vec<_>>()
        };
        let wanted = lower(name);
        let (mut low, mut high) = (0, data::ZONES.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match lower(Self(middle).name()).cmp(&wanted) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(Self(middle)),
            }
        }
        None
    }

    /// The name as the tz database spells it.
    pub fn name(self) -> &'static str {
        let start = self
            .0
            .checked_sub(1)
            .map_or(0, |previous| data::ZONES[previous].0 as usize);
        &data::NAMES[start..data::ZONES[self.0].0 as usize]
    }

    /// Seconds east of UTC at `utc`, a Unix time in seconds.
    pub fn offset_seconds(self, utc: i64) -> i64 {
        let (_, first, start, count, mut time) = data::ZONES[self.0];
        let start = start as usize;
        // Transitions take effect at their own second.
        let mut passed = 0;
        for (index, delta) in data::DELTAS[start..start + count as usize]
            .iter()
            .enumerate()
        {
            time += i64::from(*delta);
            if time > utc {
                break;
            }
            passed = index + 1;
        }
        match passed {
            0 => i64::from(first),
            passed => {
                i64::from(data::OFFSET_VALUES[usize::from(data::OFFSETS[start + passed - 1])])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Offset, TimeZone};

    use super::{NamedZone, data};

    /// A zone's transition times, decoded from the tables.
    fn transitions(zone: NamedZone) -> Vec<i64> {
        let (_, _, start, count, mut time) = data::ZONES[zone.0];
        data::DELTAS[start as usize..(start + count) as usize]
            .iter()
            .map(|delta| {
                time += i64::from(*delta);
                time
            })
            .collect()
    }

    /// Every chrono-tz zone and link resolves by name in any case and gives
    /// chrono-tz's offset at each of its transitions, the second before,
    /// and every half year from 1800 to 2100.
    #[test]
    fn every_zone_matches_chrono_tz() {
        assert_eq!(data::ZONES.len(), chrono_tz::TZ_VARIANTS.len());
        for tz in chrono_tz::TZ_VARIANTS {
            let zone =
                NamedZone::find(tz.name()).unwrap_or_else(|| panic!("{} is missing", tz.name()));
            assert_eq!(zone.name(), tz.name());
            assert_eq!(NamedZone::find(&tz.name().to_ascii_uppercase()), Some(zone));
            let transitions = transitions(zone)
                .into_iter()
                .flat_map(|time| [time - 1, time]);
            let sweep = (-5_364_662_400_i64..4_102_444_800).step_by(182 * 86_400);
            for utc in transitions.chain(sweep) {
                let expected = tz
                    .offset_from_utc_datetime(
                        &chrono::DateTime::from_timestamp(utc, 0)
                            .expect("in range")
                            .naive_utc(),
                    )
                    .fix()
                    .local_minus_utc();
                assert_eq!(
                    zone.offset_seconds(utc),
                    i64::from(expected),
                    "{} at {utc}",
                    tz.name()
                );
            }
        }
        assert_eq!(NamedZone::find("Mars/Olympus"), None);
    }
}
