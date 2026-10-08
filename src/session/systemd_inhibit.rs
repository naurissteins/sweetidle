use super::inhibit::diagnostic_text;

pub(crate) type LogindInhibitor = (String, String, String, String, u32, u32);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SystemdIdleInhibitor {
    pub who: String,
    pub why: String,
    pub uid: u32,
    pub pid: u32,
}

pub(crate) fn idle_block_inhibitors(inhibitors: Vec<LogindInhibitor>) -> Vec<SystemdIdleInhibitor> {
    // logind lists several operations in one colon separated field
    let mut inhibitors = inhibitors
        .into_iter()
        .filter_map(|(what, who, why, mode, uid, pid)| {
            (mode == "block" && what.split(':').any(|operation| operation == "idle")).then(|| {
                SystemdIdleInhibitor {
                    who: diagnostic_text(&who),
                    why: diagnostic_text(&why),
                    uid,
                    pid,
                }
            })
        })
        .collect::<Vec<_>>();
    // deterministic ordering keeps status snapshots easy to compare
    inhibitors.sort_unstable();
    inhibitors
}

#[cfg(test)]
mod tests {
    use super::{SystemdIdleInhibitor, idle_block_inhibitors};

    #[test]
    fn selects_only_block_mode_idle_inhibitors() {
        let inhibitors = idle_block_inhibitors(vec![
            inhibitor("sleep:idle", "video", "playing", "block", 1000, 30),
            inhibitor("idle", "backup", "working", "delay", 1000, 20),
            inhibitor("sleep", "update", "installing", "block", 0, 10),
        ]);

        assert_eq!(
            inhibitors,
            vec![SystemdIdleInhibitor {
                who: "video".to_string(),
                why: "playing".to_string(),
                uid: 1000,
                pid: 30,
            }]
        );
    }

    #[test]
    fn matches_idle_as_a_complete_operation() {
        let inhibitors = idle_block_inhibitors(vec![inhibitor(
            "not-idle",
            "application",
            "reason",
            "block",
            1000,
            20,
        )]);

        assert!(inhibitors.is_empty());
    }

    #[test]
    fn bounds_diagnostic_text_and_sorts_snapshots() {
        let long = "x".repeat(256 + 10);
        let inhibitors = idle_block_inhibitors(vec![
            inhibitor("idle", "z-last", "reason", "block", 1000, 30),
            inhibitor("idle", &long, &long, "block", 1000, 20),
        ]);

        assert_eq!(inhibitors[0].who.chars().count(), 256);
        assert_eq!(inhibitors[0].why.chars().count(), 256);
        assert_eq!(inhibitors[1].who, "z-last");
    }

    #[test]
    fn diagnostic_bounds_preserve_short_text_and_multibyte_character_boundaries() {
        for (input, expected) in [
            (String::new(), String::new()),
            ("short text".to_string(), "short text".to_string()),
            ("é🦀".to_string(), "é🦀".to_string()),
            ("x".repeat(256), "x".repeat(256)),
            ("é🦀".repeat(128), "é🦀".repeat(128)),
            ("é🦀".repeat(138), "é🦀".repeat(128)),
        ] {
            let inhibitors =
                idle_block_inhibitors(vec![inhibitor("idle", &input, &input, "block", 1000, 20)]);

            assert_eq!(inhibitors.len(), 1);
            assert_eq!(inhibitors[0].who, expected);
            assert_eq!(inhibitors[0].why, expected);
        }
    }

    fn inhibitor(
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
        uid: u32,
        pid: u32,
    ) -> (String, String, String, String, u32, u32) {
        (
            what.to_string(),
            who.to_string(),
            why.to_string(),
            mode.to_string(),
            uid,
            pid,
        )
    }
}
