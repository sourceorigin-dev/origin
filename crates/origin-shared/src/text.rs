//! Values written as text.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// A length of time, written as a whole number of seconds (`s`), minutes
/// (`m`), hours (`h`) or days (`d`), such as `90d` or `30s`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Period(pub Duration);

impl Period {
    /// The units, longest first, so a period displays in the largest one
    /// that divides it.
    const UNITS: [(char, u64); 4] = [('d', 24 * 60 * 60), ('h', 60 * 60), ('m', 60), ('s', 1)];
}

impl FromStr for Period {
    type Err = InvalidPeriod;

    fn from_str(period: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidPeriod(period.to_owned());
        let unit = period.chars().last().ok_or_else(invalid)?;
        let &(_, seconds) = Self::UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .ok_or_else(invalid)?;
        let count: u64 = period[..period.len() - 1].parse().map_err(|_| invalid())?;
        let total = count.checked_mul(seconds).ok_or_else(invalid)?;
        Ok(Self(Duration::from_secs(total)))
    }
}

/// In whole seconds, in the largest unit that divides them.
impl fmt::Display for Period {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs();
        let (unit, size) = Self::UNITS
            .into_iter()
            .find(|&(_, size)| seconds.is_multiple_of(size) && seconds >= size)
            .unwrap_or(('s', 1));
        write!(f, "{}{unit}", seconds / size)
    }
}

/// A string that [`Period`] doesn't parse.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a period like `90d`, `12h`, `30m` or `45s`")]
pub struct InvalidPeriod(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_parse_as_they_display() {
        for (text, seconds) in [
            ("90d", 90 * 86_400),
            ("12h", 43_200),
            ("30m", 1800),
            ("45s", 45),
        ] {
            let period: Period = text.parse().unwrap();
            assert_eq!(period.0, Duration::from_secs(seconds));
            assert_eq!(period.to_string(), text);
        }
        assert_eq!(Period(Duration::from_secs(120)).to_string(), "2m");
        assert_eq!(Period(Duration::ZERO).to_string(), "0s");
        for invalid in ["", "d", "5", "5w", "-1s", "1.5h"] {
            assert!(invalid.parse::<Period>().is_err(), "{invalid:?}");
        }
    }
}
