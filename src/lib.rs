pub mod cli;
pub mod config;
pub mod index;
pub mod paths;
pub mod providers;
pub mod query;
pub mod resume;
pub mod scan;
pub mod tokenizer;
pub mod tui;
pub mod types;
mod varint;

pub use config::AppConfig;
pub use index::{IndexManager, SearchIndex};
pub use types::{MessageDoc, Provider, Role, SearchOptions, SessionHit, SourceRef};
