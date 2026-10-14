use clap::builder::RangedI64ValueParser;

/// Parses a zstd level, or 0 for no compression.
pub fn zstd_level() -> RangedI64ValueParser<i32> {
    clap::value_parser!(i32).range(0..=19)
}
