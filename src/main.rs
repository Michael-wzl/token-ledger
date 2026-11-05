use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use token_ledger::{
    collect, safety, server,
    store::{ImportRequest, Store},
};

#[derive(Parser)]
#[command(
    version,
    about = "Offline AI token ledger. Reads AI sources; writes only its own data directory."
)]
struct Cli {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Use a fixture/alternate home directory. Does not change the process HOME.
    #[arg(long, global = true)]
    source_home: Option<PathBuf>,
    /// Explicit directory of remote-machine Claude/Codex JSONL logs (never followed through symlinks).
    #[arg(long, global = true)]
    source_root: Vec<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Serve the embedded dashboard on 127.0.0.1 only.
    Serve {
        #[arg(long)]
        open: bool,
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Read local usage and update only the counter's numeric database.
    Scan,
    /// Print the counter's numeric ledger as JSON.
    Export,
    /// Import an explicit vendor export or remote-machine JSONL file.
    Import {
        file: PathBuf,
        #[arg(long)]
        app: String,
        #[arg(long, default_value = "unknown")]
        account: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        format: Option<String>,
    },
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = cli
        .source_home
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()));
    if !home.is_absolute() || !home.is_dir() {
        anyhow::bail!("Source home must be an existing absolute directory");
    }
    let default = std::env::current_exe()?
        .parent()
        .context("Executable directory unavailable")?
        .join("data");
    let data = safety::prepare_data_dir(&cli.data_dir.unwrap_or(default), &home, &cli.source_root)?;
    let mut store = Store::open(&data.join("ledger.sqlite3"))?;
    match cli.command {
        Command::Serve { port, open } => server::serve(store, home, cli.source_root, port, open),
        Command::Scan => {
            let scan = collect::scan(&home, &cli.source_root);
            let count = scan.records.len();
            let errors: usize = scan.coverage.iter().map(|c| c.errors).sum();
            store.apply_scan(scan)?;
            println!("Saved {count} observed numeric records. Coverage is partial; {errors} source diagnostics. AI source files were opened read-only.");
            Ok(())
        }
        Command::Export => {
            println!("{}", serde_json::to_string_pretty(&store.export()?)?);
            Ok(())
        }
        Command::Import {
            file,
            app,
            account,
            from,
            to,
            format,
        } => {
            let name = file
                .file_name()
                .and_then(|s| s.to_str())
                .context("Import file name unavailable")?;
            let lower = name.to_lowercase();
            if [
                "auth",
                "credential",
                "cookie",
                "config",
                "settings",
                "keychain",
            ]
            .iter()
            .any(|s| lower.contains(s))
            {
                anyhow::bail!("Credential/configuration files are not usage imports");
            }
            let fmt = format.unwrap_or_else(|| {
                file.extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_owned()
            });
            if !["csv", "json", "jsonl"].contains(&fmt.as_str()) {
                anyhow::bail!("Import must be CSV, JSON, or JSONL");
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut f = options.open(&file)?;
            if f.metadata()?.len() > 32 * 1024 * 1024 {
                anyhow::bail!("Import exceeds 32 MiB");
            }
            use std::io::Read;
            let mut content = String::new();
            f.by_ref()
                .take(32 * 1024 * 1024 + 1)
                .read_to_string(&mut content)?;
            if content.len() > 32 * 1024 * 1024 {
                anyhow::bail!("Import exceeds 32 MiB");
            }
            let result = store.import(&ImportRequest {
                format: fmt,
                app,
                account,
                from,
                to,
                name: name.to_owned(),
                content,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
    }
}
