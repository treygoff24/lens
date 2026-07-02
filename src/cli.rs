use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = env!("CARGO_PKG_NAME"),
    version,
    about = "Agent-first image library search.",
    long_about = None,
    arg_required_else_help = true,
    color = clap::ColorChoice::Never,
    rename_all = "kebab-case"
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Force the machine JSON envelope on stdout/stderr.
    #[arg(long, global = true, default_value_t = false)]
    pub json: bool,

    /// Override the Cerebras model.
    #[arg(long, global = true, value_name = "MODEL")]
    pub model: Option<String>,

    /// Hard spend cap in dollars.
    #[arg(long, global = true, value_name = "X", value_parser = parse_positive_f64)]
    pub max_dollars: Option<f64>,

    /// Hard wall-clock cap in seconds.
    #[arg(long, global = true, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    pub max_seconds: Option<u64>,

    /// Override the index directory.
    #[arg(long, global = true, value_name = "PATH", value_hint = clap::ValueHint::DirPath)]
    pub index_path: Option<PathBuf>,

    /// Print planned work and projected cost without spending.
    #[arg(long, global = true, default_value_t = false)]
    pub dry_run: bool,

    /// Worker concurrency for index, capped at 50.
    #[arg(long, global = true, value_name = "N", default_value_t = 25, value_parser = clap::value_parser!(u16).range(1..=50))]
    pub concurrency: u16,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Index a directory of images.
    Index(IndexArgs),
    /// Search the image index with a natural-language query.
    Find(FindArgs),
    /// Report index freshness without network calls.
    Status(StatusArgs),
    /// Diagnose config, credentials, and local dependencies.
    Doctor(DoctorArgs),
    /// Print the machine-readable CLI contract.
    Capabilities,
    /// Print JSON Schema for response and error envelopes.
    Schema(SchemaArgs),
}

#[derive(Debug, Clone, Args)]
pub struct IndexArgs {
    /// Directory to index.
    #[arg(value_name = "DIR", default_value = ".", value_hint = clap::ValueHint::DirPath)]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct FindArgs {
    /// Natural-language image query.
    #[arg(value_name = "QUERY", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    pub query: String,

    /// Library directory.
    #[arg(long, value_name = "DIR", default_value = ".", value_hint = clap::ValueHint::DirPath)]
    pub dir: PathBuf,

    /// Number of hits to return.
    #[arg(long, value_name = "N", default_value_t = 8, value_parser = parse_top)]
    pub top: usize,

    /// Filter to records of these kinds before searching (repeatable, comma-separable; e.g. photo, graphic).
    #[arg(long = "kind", value_name = "KIND", value_delimiter = ',', value_parser = parse_kind)]
    pub kind: Vec<String>,

    /// Write a dark contact-sheet HTML file for the hits.
    #[arg(long, value_name = "PATH", value_hint = clap::ValueHint::FilePath)]
    pub gallery: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    /// Library directory.
    #[arg(long, value_name = "DIR", default_value = ".", value_hint = clap::ValueHint::DirPath)]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    /// Probe Cerebras online with a minimal chat call.
    #[arg(long, default_value_t = false)]
    pub online: bool,
}

#[derive(Debug, Clone, Args)]
pub struct SchemaArgs {
    /// Which schema to print.
    #[arg(value_enum, default_value_t = SchemaTarget::All)]
    pub target: SchemaTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SchemaTarget {
    Response,
    Error,
    All,
}

fn parse_top(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{value:?} is not an integer"))?;
    if (1..=100).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("value must be between 1 and 100".to_string())
    }
}

fn parse_positive_f64(value: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("{value:?} is not a number"))?;
    if parsed.is_finite() && parsed >= 0.0 {
        Ok(parsed)
    } else {
        Err("value must be a non-negative finite number".to_string())
    }
}

fn parse_kind(value: &str) -> Result<String, String> {
    let kind = value.trim().to_lowercase();
    if kind.is_empty() {
        Err("--kind value must not be empty".to_string())
    } else {
        Ok(kind)
    }
}

pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let mut costs: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = costs[0];
        costs[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let old = costs[j + 1];
            costs[j + 1] = if ca == cb {
                prev
            } else {
                1 + prev.min(costs[j]).min(costs[j + 1])
            };
            prev = old;
        }
    }
    costs[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use clap::error::ErrorKind;

    #[test]
    fn no_default_query_subcommand() {
        let err = Cli::try_parse_from(["lens", "beach club"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn parses_find() {
        let cli = Cli::try_parse_from(["lens", "--json", "find", "hero", "--top", "3"]).unwrap();
        assert!(cli.global.json);
        match cli.command {
            Commands::Find(args) => {
                assert_eq!(args.query, "hero");
                assert_eq!(args.top, 3);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn parses_repeatable_and_comma_separated_kind_filters() {
        let cli =
            Cli::try_parse_from(["lens", "find", "q", "--kind", "photo", "--kind", "graphic"])
                .unwrap();
        match cli.command {
            Commands::Find(args) => assert_eq!(args.kind, ["photo", "graphic"]),
            _ => panic!("wrong command"),
        }

        let cli = Cli::try_parse_from(["lens", "find", "q", "--kind", "photo,graphic"]).unwrap();
        match cli.command {
            Commands::Find(args) => assert_eq!(args.kind, ["photo", "graphic"]),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn empty_kind_filter_is_parse_error() {
        let err = Cli::try_parse_from(["lens", "find", "q", "--kind", ""]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        assert!(err.to_string().contains("--kind"));
    }

    #[test]
    fn top_range_is_validated_at_parse_boundary() {
        assert!(Cli::try_parse_from(["lens", "find", "q", "--top", "0"]).is_err());
        assert!(Cli::try_parse_from(["lens", "find", "q", "--top", "101"]).is_err());
        let cli = Cli::try_parse_from(["lens", "find", "q", "--top", "100"]).unwrap();
        match cli.command {
            Commands::Find(args) => assert_eq!(args.top, 100),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn concurrency_range_and_default_are_validated_at_parse_boundary() {
        assert!(Cli::try_parse_from(["lens", "--concurrency", "0", "find", "q"]).is_err());
        assert!(Cli::try_parse_from(["lens", "--concurrency", "51", "find", "q"]).is_err());
        let cli = Cli::try_parse_from(["lens", "find", "q"]).unwrap();
        assert_eq!(cli.global.concurrency, 25);
    }

    #[test]
    fn budget_flags_are_validated_at_parse_boundary() {
        assert!(Cli::try_parse_from(["lens", "--max-dollars", "-1", "find", "q"]).is_err());
        assert!(Cli::try_parse_from(["lens", "--max-dollars", "0", "find", "q"]).is_ok());
        assert!(Cli::try_parse_from(["lens", "--max-seconds", "0", "find", "q"]).is_err());
    }

    #[test]
    fn global_json_flag_parses_after_find_subcommand() {
        let cli = Cli::try_parse_from(["lens", "find", "q", "--json"]).unwrap();
        assert!(cli.global.json);
    }

    #[test]
    fn find_query_is_required_and_non_empty() {
        let err = Cli::try_parse_from(["lens", "find"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);

        let err = Cli::try_parse_from(["lens", "find", ""]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidValue);
    }
}
