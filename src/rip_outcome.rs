use crate::makemkv::RipReport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipOutcome {
    Complete {
        titles_saved: u32,
    },
    /// Partial or failed: at least one expected title is missing.
    Incomplete {
        titles_saved: u32,
        titles_expected: u32,
        reasons: Vec<String>,
    },
}

impl RipOutcome {
    pub fn summary(&self) -> String {
        match self {
            RipOutcome::Complete { titles_saved } => format!("{} titles saved", titles_saved),
            RipOutcome::Incomplete {
                titles_saved,
                titles_expected,
                reasons,
            } => format!(
                "{}/{} titles saved: {}",
                titles_saved,
                titles_expected,
                reasons.join("; ")
            ),
        }
    }
}

/// Decides whether a rip succeeded. `titles_listed` is how many titles the disc scan listed at
/// or above the minimum length; `output_files` is how many files the rip left in its directory.
pub fn evaluate_rip(report: &RipReport, titles_listed: u32, output_files: usize) -> RipOutcome {
    let titles_saved = report.titles_saved.unwrap_or(0);
    let titles_failed = report.titles_failed.unwrap_or(0);
    let titles_expected = titles_listed
        .max(report.titles_to_save.unwrap_or(0))
        .max(titles_saved + titles_failed);

    let mut reasons = Vec::new();
    match report.exit_code {
        Some(0) => {}
        Some(code) => reasons.push(format!("makemkvcon exited with code {}", code)),
        None => reasons.push("makemkvcon was killed by a signal".to_string()),
    }
    if report.titles_saved.is_none() {
        reasons.push("MakeMKV reported no end-of-rip summary".to_string());
    }
    if titles_failed > 0 {
        reasons.push(format!("MakeMKV reported {} titles failed", titles_failed));
    }
    if titles_saved < titles_expected && titles_failed == 0 {
        reasons.push(format!(
            "disc scan listed {} titles but fewer were saved",
            titles_expected
        ));
    }
    if output_files == 0 {
        reasons.push("no files were written".to_string());
    }

    if reasons.is_empty() {
        RipOutcome::Complete { titles_saved }
    } else {
        RipOutcome::Incomplete {
            titles_saved,
            titles_expected,
            reasons,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(exit_code: i32, to_save: u32, saved: u32, failed: u32) -> RipReport {
        RipReport {
            exit_code: Some(exit_code),
            titles_to_save: Some(to_save),
            titles_saved: Some(saved),
            titles_failed: Some(failed),
            title_errors: Vec::new(),
        }
    }

    #[test]
    fn clean_rip_is_complete() {
        let outcome = evaluate_rip(&report(0, 2, 2, 0), 2, 2);
        assert_eq!(outcome, RipOutcome::Complete { titles_saved: 2 });
        assert_eq!(outcome.summary(), "2 titles saved");
    }

    #[test]
    fn exit_zero_with_failed_titles_is_incomplete() {
        let outcome = evaluate_rip(&report(0, 10, 2, 8), 10, 2);
        assert!(matches!(
            outcome,
            RipOutcome::Incomplete {
                titles_saved: 2,
                titles_expected: 10,
                ..
            }
        ));
        assert!(outcome.summary().starts_with("2/10 titles saved: "));
    }

    #[test]
    fn exit_zero_with_no_files_is_incomplete() {
        let outcome = evaluate_rip(&report(0, 1, 1, 0), 1, 0);
        let RipOutcome::Incomplete { reasons, .. } = &outcome else {
            panic!("expected incomplete, got {:?}", outcome);
        };
        assert_eq!(reasons, &vec!["no files were written".to_string()]);
    }

    #[test]
    fn fewer_saved_than_disc_scan_listed_is_incomplete() {
        // MakeMKV gave up before reporting the rest, e.g. because the disc vanished.
        let outcome = evaluate_rip(&report(0, 2, 2, 0), 10, 2);
        assert!(matches!(
            outcome,
            RipOutcome::Incomplete {
                titles_saved: 2,
                titles_expected: 10,
                ..
            }
        ));
    }

    #[test]
    fn missing_summary_is_incomplete() {
        let report = RipReport {
            exit_code: Some(0),
            titles_to_save: Some(3),
            ..Default::default()
        };
        let outcome = evaluate_rip(&report, 3, 1);
        assert_eq!(
            outcome,
            RipOutcome::Incomplete {
                titles_saved: 0,
                titles_expected: 3,
                reasons: vec![
                    "MakeMKV reported no end-of-rip summary".to_string(),
                    "disc scan listed 3 titles but fewer were saved".to_string(),
                ],
            }
        );
    }

    #[test]
    fn nonzero_exit_is_incomplete() {
        let outcome = evaluate_rip(&report(1, 2, 2, 0), 2, 2);
        assert!(matches!(outcome, RipOutcome::Incomplete { .. }));
    }

    #[test]
    fn more_saved_than_disc_scan_listed_is_complete() {
        // The scan uses MakeMKV's own default minimum length, which can be stricter.
        let outcome = evaluate_rip(&report(0, 4, 4, 0), 2, 4);
        assert_eq!(outcome, RipOutcome::Complete { titles_saved: 4 });
    }
}
