use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use super::mcp::daemon::{run_daemon, serve};
use super::mcp::server::serve_direct;
use super::util::json::print_json;
use super::{
    FetchArgs, Paths, build_index, coverage_repair, refresh, search_docs, shopify_fetch, status,
};

#[derive(Debug, Parser)]
#[command(name = "shopify-rextant")]
#[command(version)]
#[command(about = "Local Shopify docs map MCP server")]
struct Cli {
    #[arg(long, env = "SHOPIFY_REXTANT_HOME")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        direct: bool,
    },
    #[command(hide = true)]
    Daemon {
        #[arg(long, default_value_t = 600)]
        idle_timeout_secs: u64,
    },
    Build {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        limit: Option<usize>,
    },
    Refresh {
        path: Option<String>,
        #[arg(long)]
        url: Option<String>,
    },
    Coverage {
        #[command(subcommand)]
        command: CoverageCommand,
    },
    Status,
    Search {
        query: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    Show {
        path: String,
        #[arg(long)]
        anchor: Option<String>,
        #[arg(long, default_value_t = true)]
        include_code_blocks: bool,
        #[arg(long)]
        max_chars: Option<usize>,
    },
    Version,
}

#[derive(Debug, Subcommand)]
enum CoverageCommand {
    Repair,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new(cli.home)?;

    match cli.command {
        Command::Serve { direct } => {
            if direct {
                serve_direct(paths).await
            } else {
                serve(paths).await
            }
        }
        Command::Daemon { idle_timeout_secs } => {
            run_daemon(paths, Duration::from_secs(idle_timeout_secs)).await
        }
        Command::Build { force, limit } => build_index(&paths, force, limit).await,
        Command::Refresh { path, url } => refresh(&paths, path, url).await,
        Command::Coverage {
            command: CoverageCommand::Repair,
        } => print_json(&coverage_repair(&paths).await?),
        Command::Status => print_json(&status(&paths)?),
        Command::Search {
            query,
            version,
            limit,
        } => print_json(&search_docs(&paths, &query, version.as_deref(), limit)?),
        Command::Show {
            path,
            anchor,
            include_code_blocks,
            max_chars,
        } => {
            let response = shopify_fetch(
                &paths,
                &FetchArgs {
                    path: Some(path),
                    url: None,
                    anchor,
                    include_code_blocks: Some(include_code_blocks),
                    max_chars,
                },
            )
            .await?;
            println!("{}", response.content);
            Ok(())
        }
        Command::Version => {
            println!("shopify-rextant {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
