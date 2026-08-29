//! Run the MCP server standalone over stdio, without the `attempt` CLI:
//!
//! ```text
//! cargo run -p attemptdb-mcp --example serve -- <db_dir> [--snapshot FILE] [--data-dir DIR] [--max-rows N]
//! ```

use attemptdb_mcp::{ServerConfig, serve_stdio};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(db_dir) = args.next() else {
        anyhow::bail!("usage: serve <db_dir> [--snapshot FILE] [--data-dir DIR] [--max-rows N]");
    };
    let mut config = ServerConfig::new(db_dir);
    config.project_root = std::env::current_dir().ok();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))?;
        match flag.as_str() {
            "--snapshot" => config.snapshot = Some(PathBuf::from(value)),
            "--data-dir" => config.data_dir = Some(PathBuf::from(value)),
            "--max-rows" => config.max_rows = value.parse()?,
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    serve_stdio(config)
}
