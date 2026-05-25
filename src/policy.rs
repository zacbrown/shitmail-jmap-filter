pub fn should_quarantine(age_days: i64, max_age_days: i64) -> bool {
    age_days < max_age_days
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cutoff_quarantines() {
        assert!(should_quarantine(0, 365));
        assert!(should_quarantine(1, 365));
        assert!(should_quarantine(364, 365));
    }

    #[test]
    fn at_or_above_cutoff_keeps() {
        assert!(!should_quarantine(365, 365));
        assert!(!should_quarantine(366, 365));
        assert!(!should_quarantine(10_000, 365));
    }
}
