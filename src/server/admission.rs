use std::{
    fmt,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

/// Which lookups fill the block cache, by the ply of the requested
/// position: deep positions are rarely requested again.
///
/// Written as `PLY:PERCENT` tiers, followed by the percentage for all
/// deeper plies, like `15:100,20:5,25:2,1`: positions before ply 15 always
/// fill the cache, before ply 20 every twentieth lookup, and so on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheAdmission {
    /// Ply limits (exclusive) and their percentages, ascending.
    tiers: Vec<(u32, u64)>,
    /// Percentage for all deeper plies.
    rest: u64,
}

impl Default for CacheAdmission {
    fn default() -> CacheAdmission {
        CacheAdmission {
            tiers: vec![(15, 100), (20, 5), (25, 2)],
            rest: 1,
        }
    }
}

impl CacheAdmission {
    fn percent(&self, ply: u32) -> u64 {
        self.tiers
            .iter()
            .find(|&&(limit, _)| ply < limit)
            .map_or(self.rest, |&(_, percent)| percent)
    }

    /// Whether a lookup of a position at the ply should fill the cache.
    /// `sample` spreads the admitted lookups evenly.
    pub fn fill(&self, ply: u32, sample: &AtomicU64) -> bool {
        match self.percent(ply) {
            100.. => true,
            0 => false,
            percent => sample.fetch_add(1, Ordering::Relaxed) % 100 < percent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCacheAdmission;

impl fmt::Display for InvalidCacheAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected PLY:PERCENT tiers and a final PERCENT, like 15:100,20:5,25:2,1")
    }
}

impl std::error::Error for InvalidCacheAdmission {}

impl FromStr for CacheAdmission {
    type Err = InvalidCacheAdmission;

    fn from_str(s: &str) -> Result<CacheAdmission, InvalidCacheAdmission> {
        let percent = |s: &str| {
            s.parse::<u64>()
                .ok()
                .filter(|&percent| percent <= 100)
                .ok_or(InvalidCacheAdmission)
        };
        let mut parts: Vec<&str> = s.split(',').collect();
        let rest = percent(parts.pop().ok_or(InvalidCacheAdmission)?)?;
        let tiers = parts
            .into_iter()
            .map(|tier| {
                let (ply, pct) = tier.split_once(':').ok_or(InvalidCacheAdmission)?;
                Ok((
                    ply.parse::<u32>().map_err(|_| InvalidCacheAdmission)?,
                    percent(pct)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !tiers.is_sorted_by(|a, b| a.0 < b.0) {
            return Err(InvalidCacheAdmission);
        }
        Ok(CacheAdmission { tiers, rest })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        assert_eq!(
            "15:100,20:5,25:2,1".parse::<CacheAdmission>(),
            Ok(CacheAdmission::default())
        );
        let admission: CacheAdmission = "100".parse().unwrap();
        assert_eq!(admission.percent(1000), 100);
        assert!("20:5,15:100,1".parse::<CacheAdmission>().is_err());
        assert!("15:101,1".parse::<CacheAdmission>().is_err());
        assert!("".parse::<CacheAdmission>().is_err());
    }

    #[test]
    fn test_fill() {
        let admission = CacheAdmission::default();
        let sample = AtomicU64::new(0);
        assert!(admission.fill(14, &sample));
        let filled = (0..1000).filter(|_| admission.fill(22, &sample)).count();
        assert_eq!(filled, 20);
        let never: CacheAdmission = "10:100,0".parse().unwrap();
        assert!(!never.fill(10, &sample));
    }
}
