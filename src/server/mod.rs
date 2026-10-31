//! Read-only HTTP server for the opening explorer API, serving the SST files
//! produced by `parquet_to_games` and `parquet_to_positions`.

pub mod app;
pub mod db;
pub mod explore;
pub mod metrics;
pub mod openings;
pub mod pgn;
pub mod query;
pub mod replay;
pub mod response;

pub use app::{AppConfig, Kind, serve};
pub use db::{Database, DbConfig, IngestStats, ingest};
