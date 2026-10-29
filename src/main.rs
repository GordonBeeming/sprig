use clap::{Args, Parser, Subcommand};
use serde_json::json;
use sprig::{
    backup, config,
    git::Repository,
    operations::{self, CreateOptions},
    Error, Output, Result,
};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = env!("CARGO_PKG_DESCRIPTION"),
    after_help = "Time Machine exclusions include local uncommitted work. Stop editors, builds and Git operations before creation or conversion. CoW never falls back to a full copy."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Emit one schema-versioned JSON result on stdout"
    )]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Args)]
struct BackupFlags {
    #[arg(
        long,
        conflicts_with = "no_time_machine_exclude",
        help = "Exclude the workspace and its local uncommitted work from future Time Machine backups"
    )]
    time_machine_exclude: bool,
    #[arg(long, help = "Do not add a new exclusion; existing exclusions remain")]
    no_time_machine_exclude: bool,
}
impl BackupFlags {
    fn value(&self) -> Option<bool> {
        if self.time_machine_exclude {
            Some(true)
        } else if self.no_time_machine_exclude {
            Some(false)
        } else {
            None
        }
    }
}
#[derive(Subcommand)]
enum Commands {
    Create {
        destination: PathBuf,
        #[arg(long)]
        from: PathBuf,
        #[arg(long, conflicts_with_all=["new_branch","detach","reference"])]
        branch: Option<String>,
        #[arg(long, conflicts_with = "detach")]
        new_branch: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long = "ref")]
        reference: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        backup: BackupFlags,
    },
    Convert {
        existing_worktree: PathBuf,
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        backup: BackupFlags,
    },
    Status {
        worktree: Option<PathBuf>,
    },
    Doctor {
        path: Option<PathBuf>,
    },
    Recover {
        path: Option<PathBuf>,
        #[arg(
            long,
            help = "Accept retained creation directories/refs and clear their journals without changing any workspace files"
        )]
        acknowledge_create: bool,
    },
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    Capabilities,
}
#[derive(Subcommand)]
enum BackupAction {
    Apply {
        path: PathBuf,
        #[arg(
            long,
            help = "Explicitly accept ownership of an exclusion left by an interrupted Sprig operation"
        )]
        resolve_pending: bool,
    },
    Remove {
        path: PathBuf,
    },
}
#[derive(Subcommand)]
enum ConfigAction {
    Get {
        key: String,
    },
    Set {
        key: String,
        value: String,
        #[arg(long, default_value="user", value_parser=["user","repository"])]
        scope: String,
    },
}
fn execute(cli: &Cli) -> Result<Output> {
    match &cli.command {
        Commands::Create {
            destination,
            from,
            branch,
            new_branch,
            detach,
            reference,
            dry_run,
            backup,
        } => operations::create(
            from,
            destination,
            &CreateOptions {
                branch: branch.clone(),
                new_branch: new_branch.clone(),
                detach: *detach,
                reference: reference.clone(),
                backup: backup.value(),
                dry_run: *dry_run,
            },
        ),
        Commands::Convert {
            existing_worktree,
            source,
            dry_run,
            backup,
        } => operations::convert(
            existing_worktree,
            source.as_deref(),
            *dry_run,
            backup.value(),
        ),
        Commands::Status { worktree } => {
            operations::status(worktree.as_deref().unwrap_or(Path::new(".")), false)
        }
        Commands::Doctor { path } => {
            operations::status(path.as_deref().unwrap_or(Path::new(".")), true)
        }
        Commands::Recover {
            path,
            acknowledge_create,
        } => operations::recover(
            path.as_deref().unwrap_or(Path::new(".")),
            *acknowledge_create,
        ),
        Commands::Capabilities => Ok(Output::success(operations::capabilities())),
        Commands::Backup { action } => {
            let (path, remove, resolve_pending) = match action {
                BackupAction::Apply {
                    path,
                    resolve_pending,
                } => (path, false, *resolve_pending),
                BackupAction::Remove { path } => (path, true, false),
            };
            let repo = Repository::open(path)?;
            if path.canonicalize()? != repo.root {
                return Err(Error::new(
                    "E_ARGUMENT",
                    "backup operations require the exact worktree root",
                ));
            }
            let _lock = sprig::state::Lock::acquire(&repo)?;
            let status = if remove {
                backup::remove(&repo, &repo.root)?
            } else {
                backup::apply_with_resolution(&repo, &repo.root, resolve_pending)?
            };
            Ok(Output::success(
                json!({"worktree":repo.root,"backup":status}),
            ))
        }
        Commands::Config { action } => {
            let repo = Repository::open(Path::new(".")).ok();
            match action {
                ConfigAction::Get { key } => {
                    let effective = config::effective(repo.as_ref(), None)?;
                    let (value, origin) = match key.as_str() {
                        "backup.time_machine_exclude" => (
                            json!(effective.time_machine_exclude),
                            effective.backup_origin,
                        ),
                        "copy.ignored" => (json!(effective.ignored), effective.ignored_origin),
                        _ => return Err(Error::new("E_CONFIG", "unknown configuration key")),
                    };
                    Ok(Output::success(
                        json!({"key":key,"value":value,"origin":origin}),
                    ))
                }
                ConfigAction::Set { key, value, scope } => {
                    let path = config::set(repo.as_ref(), scope, key, value)?;
                    Ok(Output::success(
                        json!({"path":path,"key":key,"value":value}),
                    ))
                }
            }
        }
    }
}
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let wants_json = args.iter().any(|s| s == "--json");
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) if e.use_stderr() && wants_json => {
            println!(
                "{}",
                json!({"schema_version":1,"outcome":"failure","code":"E_ARGUMENT","data":{},"message":e.to_string(),"warnings":[]})
            );
            std::process::exit(2);
        }
        Err(e) => e.exit(),
    };
    match execute(&cli) {
        Ok(output) => {
            for warning in &output.warnings {
                eprintln!("WARNING {}: {}", warning.code, warning.message);
            }
            if cli.json {
                println!("{}", json!(output));
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&output.data)
                        .unwrap_or_else(|e| format!("result encoding failed: {e}"))
                );
            }
            if output.outcome == "partial_success" {
                std::process::exit(6);
            }
        }
        Err(e) => {
            if cli.json {
                println!(
                    "{}",
                    json!({"schema_version":1,"outcome":if e.exit_code()==3 {"unsupported"} else {"failure"},"code":e.code,"message":e.message,"data":e.data,"warnings":[]})
                );
            } else {
                eprintln!("{e}");
            }
            std::process::exit(e.exit_code());
        }
    }
}
