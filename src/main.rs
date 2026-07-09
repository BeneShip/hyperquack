#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]

use anyhow::{Context, Result};
use clap::Parser;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{IsTerminal, stdout};
use std::os::unix::process::CommandExt;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

mod motherduck_client;
use motherduck_client::*;

#[derive(Parser, Debug)]
#[command(name = "hq")]
#[command(about, long_about = env!("CARGO_PKG_DESCRIPTION"))]
struct Args {
    /// Enable verbose output
    #[arg(short, long, default_value_t = false)]
    verbose: bool,

    /// MotherDuck region
    #[arg(short, long, default_value = "us-east-1-aws")]
    region: String,

    /// Subcommand to run
    #[command(subcommand)]
    command: Command,
}

#[derive(Parser, Debug)]
enum Command {
    /// List all service accounts in the MotherDuck organization
    List {
        #[arg(short, long, default_value_t = false)]
        json: bool,
    },
    /// Print name of user associated with current motherduck_token environment variable
    Identify,
    /// Provide MotherDuck shell environment variables for the given service account
    Env {
        service_account: String,

        /// Use read-only token
        #[arg(short, long, default_value_t = false)]
        read_only: bool,
    },

    /// Start a DuckDB session for the given service account
    #[command(name = "duckdb")]
    DuckDb {
        service_account: String,

        #[arg(short, long, default_value_t = false)]
        read_only: bool,

        #[arg(short, long, default_value_t = false)]
        database_is_username: bool,

        /// Extra arguments to pass to duckdb
        #[arg(last = true)]
        extra_args: Vec<String>,
    },

    /// Run a command against all matching service accounts in parallel.
    ///
    /// The command runs in an environment prepared with `motherduck_token` and
    /// `motherduck_user` set for the matched service account.
    Map {
        /// Glob pattern to match service account names (e.g. "prod_*" or "{dev,test}_*").
        glob: String,

        /// Use read-only token
        #[arg(short, long, default_value_t = false)]
        read_only: bool,

        /// Max number of commands to run concurrently
        #[arg(long, default_value_t = 8)]
        concurrency: usize,

        /// Command (and arguments) to run, after `--`. Use a shell for scripts,
        /// e.g. `-- bash -c 'duckdb md: -c "..."'`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },

    /// Run a command against each matching database of the given service account in parallel.
    ///
    /// The command runs in an environment prepared with `motherduck_token`,
    /// `motherduck_user` and `motherduck_database` set. `motherduck_database`
    /// names the database the command should operate on.
    #[command(name = "map-db")]
    MapDb {
        /// Service account whose databases should be mapped over
        service_account: String,

        /// Glob pattern to match database names (e.g. "prod_*" or "*").
        glob: String,

        /// Use read-only token
        #[arg(short, long, default_value_t = false)]
        read_only: bool,

        /// Max number of commands to run concurrently
        #[arg(long, default_value_t = 1)]
        concurrency: usize,

        /// Command (and arguments) to run, after `--`. Use a shell for scripts,
        /// e.g. `-- bash -c 'duckdb "md:$motherduck_database" -c "..."'`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },

    /// Start a new instance of your shell ($SHELL) for the given service account.
    ///
    /// The shell inherits an environment with `motherduck_token`,
    /// `motherduck_user` and `motherduck_database` populated.
    Shell {
        service_account: String,

        /// Database to set as `motherduck_database`
        database: String,

        /// Use read-only token
        #[arg(short, long, default_value_t = false)]
        read_only: bool,
    },

    /// Create skeleton for use with `hq map`
    Init { name: String },
}

#[derive(Debug, Clone)]
enum OutputLine {
    Stdout(String, String), // (label, line)
    Stderr(String, String), // (label, line)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let client = MotherduckClient::builder()
        .admin_token(
            std::env::var("motherduck_token")
                .context("motherduck_token environment variable not set")?,
        )
        .region(args.region.clone())
        .build();

    match args.command {
        Command::List { json } => {
            let service_accounts = client.get_service_accounts()?;
            for service_account in service_accounts {
                if json {
                    println!("{}", serde_json::to_string(&service_account)?);
                } else {
                    println!("{}", service_account.name);
                }
            }
        }
        Command::Identify => {
            let rows = duckdb_query_json("select current_user() as user", &[])
                .context("Identifying current MotherDuck user")?;
            let user = rows
                .first()
                .and_then(|row| row.get("user"))
                .and_then(|user| user.as_str())
                .context("duckdb did not return a current user")?;
            println!("{}", user);
        }
        Command::Env {
            service_account,
            read_only,
        } => {
            if stdout().is_terminal() {
                eprintln!(
                    "Try this:\n\teval $({})\n",
                    std::env::args().collect::<Vec<_>>().join(" ")
                )
            }

            let member = client.get_member_by_name(&service_account)?;

            let token = client.create_token(&member, TokenType::from_read_only(read_only), None)?;

            println!("export motherduck_token={}", token.token);
            println!("export motherduck_user={}", member.name);
        }
        Command::DuckDb {
            service_account,
            read_only,
            database_is_username,
            extra_args,
        } => {
            let member = client.get_member_by_name(&service_account)?;
            let token = client.create_token(&member, TokenType::from_read_only(read_only), None)?;

            let err = std::process::Command::new("duckdb")
                .arg(if database_is_username {
                    format!("md:{}", member.name)
                } else {
                    "md:".to_string()
                })
                .args(extra_args)
                .env("motherduck_user", &member.name)
                .env("motherduck_token", token.token)
                .exec();

            return Err(err).context("exec duckdb");
        }
        Command::Shell {
            service_account,
            database,
            read_only,
        } => {
            let member = client.get_member_by_name(&service_account)?;
            let token = client.create_token(&member, TokenType::from_read_only(read_only), None)?;

            // Fall back to /bin/sh when $SHELL is not set (e.g. in some cron/CI contexts).
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

            if stdout().is_terminal() {
                eprintln!(
                    "Starting {} as {} (database {})\n",
                    shell, member.name, database
                );
            }

            let err = std::process::Command::new(&shell)
                .env("motherduck_user", &member.name)
                .env("motherduck_token", token.token)
                .env("motherduck_database", &database)
                .exec();

            return Err(err).with_context(|| format!("exec {}", shell));
        }
        Command::Map {
            glob,
            read_only,
            concurrency,
            command,
        } => {
            let glob = globset::Glob::new(&glob)
                .context("Compiling glob pattern")?
                .compile_matcher();

            let mut service_accounts = client.get_service_accounts()?;
            service_accounts.retain(|sa| glob.is_match(&sa.name));

            if service_accounts.is_empty() {
                println!(
                    "No service_accounts matched the provided glob pattern {}",
                    glob.glob()
                );
                return Ok(());
            }

            let jobs = service_accounts
                .into_iter()
                .map(|sa| Job {
                    label: sa.name.clone(),
                    member: sa,
                    token_type: TokenType::from_read_only(read_only),
                    client: client.clone(),
                    command: command.clone(),
                    extra_env: Vec::new(),
                })
                .collect();

            run_jobs(jobs, concurrency, args.verbose).await?;
        }
        Command::MapDb {
            service_account,
            glob,
            read_only,
            concurrency,
            command,
        } => {
            let glob = globset::Glob::new(&glob)
                .context("Compiling glob pattern")?
                .compile_matcher();

            let member = client.get_member_by_name(&service_account)?;

            // List the service account's databases by shelling out to duckdb
            // with a short-lived token for that account.
            let list_token = client.create_token(&member, TokenType::ReadScaling, None)?;
            let rows = duckdb_query_json(
                "select alias from md_all_databases() where type = 'motherduck'",
                &[
                    ("motherduck_user", &member.name),
                    ("motherduck_token", &list_token.token),
                ],
            )
            .with_context(|| format!("Listing databases for {}", member.name))?;

            let databases: Vec<String> = rows
                .iter()
                .filter_map(|row| row.get("alias"))
                .filter_map(|name| name.as_str())
                .map(|name| name.to_owned())
                .filter(|name| glob.is_match(name))
                .collect();

            if databases.is_empty() {
                println!(
                    "No databases for {} matched the provided glob pattern {}",
                    member.name,
                    glob.glob()
                );
                return Ok(());
            }

            let token_type = TokenType::from_read_only(read_only);
            let jobs = databases
                .into_iter()
                .map(|database| Job {
                    label: database.clone(),
                    member: member.clone(),
                    token_type,
                    client: client.clone(),
                    command: command.clone(),
                    extra_env: vec![("motherduck_database".to_owned(), database)],
                })
                .collect();

            run_jobs(jobs, concurrency, args.verbose).await?;
        }
        Command::Init { name } => {
            let template = include_str!("../map_template.sh");
            let script_path = format!("{}.sh", name);
            if std::path::Path::new(&script_path).exists() {
                anyhow::bail!("File {} already exists", script_path);
            }
            std::fs::write(&script_path, template)
                .with_context(|| format!("Writing example script to {}", script_path))?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&script_path)
                    .with_context(|| format!("Reading permissions of {}", script_path))?
                    .permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&script_path, perms)
                    .with_context(|| format!("Making {} executable", script_path))?;
            }

            println!(
                "Created new HyperQuack map script: {script_path}\n\n\
                 Edit it and run:\n\
                 hq map <glob_pattern> -- ./{script_path}\n",
            );
        }
    }

    Ok(())
}

fn duckdb_query_json(query: &str, env: &[(&str, &str)]) -> Result<Vec<serde_json::Value>> {
    let mut command = std::process::Command::new("duckdb");
    command.args(["-no-init", "-json", "md:", "-c", query]);
    for (key, value) in env {
        command.env(key, value);
    }

    let output = command.output().context("Failed to run duckdb")?;

    if !output.status.success() {
        anyhow::bail!(
            "duckdb exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // `duckdb -json` emits an empty string for queries with no rows.
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }

    serde_json::from_slice(&output.stdout).context("Parsing duckdb JSON output")
}

/// Run the given jobs across a pool of `concurrency` workers, streaming their
/// prefixed stdout/stderr beneath a progress bar.
async fn run_jobs(jobs: Vec<Job>, concurrency: usize, verbose: bool) -> Result<()> {
    let pb = ProgressBar::new(jobs.len() as u64);

    pb.set_style(
        ProgressStyle::with_template(
            "{elapsed_precise} {spinner} {wide_bar:40.yellow/white} {human_pos}/{human_len} ETA {eta}",
        )?
        .progress_chars("█▉▊▋▌▍▎▏  "),
    );
    pb.enable_steady_tick(Duration::from_millis(100));

    let (job_sender, job_receiver) = flume::bounded(0);
    let (output_sender, output_receiver) = flume::unbounded::<OutputLine>();

    let output_handle = {
        let pb = pb.clone();
        tokio::task::spawn(async move {
            while let Ok(output) = output_receiver.recv_async().await {
                pb.suspend(|| match output {
                    OutputLine::Stdout(label, line) => {
                        println!("{}: {}", label, line)
                    }
                    OutputLine::Stderr(label, line) => {
                        eprintln!("{}: {}", style(label).red(), line)
                    }
                });
            }
        })
    };

    let concurrency = concurrency.max(1);
    let mut worker_handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let job_receiver = job_receiver.clone();
        let output_sender = output_sender.clone();
        let pb = pb.clone();
        let handle = tokio::task::spawn(async move {
            Job::worker(job_receiver, output_sender, pb).await;
        });
        worker_handles.push(handle);
    }

    drop(job_receiver);
    drop(output_sender);

    for job in jobs {
        if verbose {
            pb.suspend(|| {
                println!("{}: {}", job.label, job.command.join(" "));
            });
        }
        job_sender
            .send(job)
            .context("Failed to send job to worker")?;
    }

    drop(job_sender);

    for handle in worker_handles {
        handle.await.context("Worker task panicked")?;
    }

    output_handle
        .await
        .context("Output printer task panicked")?;

    pb.finish();
    println!("Completed in {:?}", pb.elapsed());

    Ok(())
}

#[derive(Debug)]
struct Job {
    /// Prefix used when reporting this job's output (service account or database name)
    label: String,
    member: Member,
    client: MotherduckClient,
    token_type: TokenType,
    /// Command and arguments to run
    command: Vec<String>,
    /// Additional environment variables to set (on top of the MotherDuck token/user)
    extra_env: Vec<(String, String)>,
}

impl Job {
    async fn perform(&self, output_sender: flume::Sender<OutputLine>) -> Result<()> {
        let token = self
            .client
            .create_token(&self.member, self.token_type, None)?;

        let (program, program_args) = self
            .command
            .split_first()
            .context("Empty command provided")?;

        let mut command = tokio::process::Command::new(program);
        command
            .args(program_args)
            .env("motherduck_user", &self.member.name)
            .env("motherduck_token", &token.token);
        for (key, value) in &self.extra_env {
            command.env(key, value);
        }

        let mut child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to spawn command: {}", program))?;

        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let stderr = child.stderr.take().context("Failed to capture stderr")?;

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let mut stdout_done = false;
        let mut stderr_done = false;

        while !stdout_done || !stderr_done {
            tokio::select! {
                result = stdout_reader.next_line(), if !stdout_done => {
                    match result {
                        Ok(Some(line)) => {
                            let _ = output_sender.send(OutputLine::Stdout(self.label.clone(), line));
                        }
                        Ok(None) => stdout_done = true,
                        Err(_) => stdout_done = true,
                    }
                }
                result = stderr_reader.next_line(), if !stderr_done => {
                    match result {
                        Ok(Some(line)) => {
                            let _ = output_sender.send(OutputLine::Stderr(self.label.clone(), line));
                        }
                        Ok(None) => stderr_done = true,
                        Err(_) => stderr_done = true,
                    }
                }
            }
        }

        let status = child.wait().await.context("Failed to wait for command")?;

        if !status.success() {
            anyhow::bail!("command exited with status: {}", status);
        }

        Ok(())
    }

    async fn worker(
        receiver: flume::Receiver<Job>,
        output_sender: flume::Sender<OutputLine>,
        pb: ProgressBar,
    ) {
        while let Ok(job) = receiver.recv_async().await {
            if let Err(e) = job.perform(output_sender.clone()).await {
                let _ = output_sender.send(OutputLine::Stderr(
                    job.label.clone(),
                    format!("Job failed: {}", e),
                ));
            }
            pb.inc(1);
        }
    }
}
