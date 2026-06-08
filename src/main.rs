#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]

use anyhow::{Context, Result};
use clap::Parser;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{IsTerminal, stdout};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use url::form_urlencoded;

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

    /// Run the provided .sql script against all matching service accounts in parallel
    Map {
        /// Glob pattern to match service account names (e.g. "prod_*" or "{dev,test}_*").
        glob: String,

        /// Path to the .sql script to run
        script_path: PathBuf,

        /// Use read-only token
        #[arg(short, long, default_value_t = false)]
        read_only: bool,

        #[arg(short, long, default_value_t = false)]
        database_is_username: bool,

        /// Max number of duckdb processes to run concurrently
        #[arg(long, default_value_t = 16)]
        concurrency: usize,

        /// Optional session_hint to pass to MotherDuck
        #[arg(long)]
        session_hint: Option<String>,

        /// Extra arguments to pass to duckdb
        #[arg(last = true)]
        extra_args: Vec<String>,
    },

    /// Create skeleton for use with `hq map`
    Init { name: String },
}

#[derive(Debug, Clone)]
enum OutputLine {
    Stdout(String, String), // (username, line)
    Stderr(String, String), // (username, line)
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
            let duck = duckdb::Connection::open_in_memory()?;
            duck.execute_batch("load motherduck; attach 'md:';")
                .context("Attaching to MotherDuck")?;

            let effective_user: String =
                duck.query_row("select current_user()", [], |row| row.get(0))?;
            println!("{}", effective_user);
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
        Command::Map {
            glob,
            script_path,
            read_only,
            concurrency,
            extra_args,
            session_hint,
            database_is_username,
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

            let pb = ProgressBar::new(service_accounts.len() as u64);

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
                            OutputLine::Stdout(username, line) => {
                                println!("{}: {}", username, line)
                            }
                            OutputLine::Stderr(username, line) => {
                                eprintln!("{}: {}", style(username).red(), line)
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

            // TODO order databases by size, so we can run largest first which is ideal for queuing

            for sa in service_accounts {
                let md_options = {
                    let mut q = form_urlencoded::Serializer::new(String::new());
                    if let Some(session_hint) = &session_hint {
                        q.append_pair("session_hint", session_hint);
                    }
                    let q = q.finish();
                    if !q.is_empty() {
                        format!("?{}", q)
                    } else {
                        "".to_string()
                    }
                };

                let attach_to = if database_is_username {
                    sa.name.to_string()
                } else {
                    "".to_string()
                };

                let mut md_args = vec![
                    format!("md:{attach_to}{md_options}"),
                    "-f".to_owned(),
                    script_path.display().to_string(),
                ];

                md_args.extend(extra_args.clone());

                let job = Job {
                    member: sa,
                    token_type: TokenType::from_read_only(read_only),
                    client: client.clone(),
                    args: md_args,
                };

                if args.verbose {
                    pb.suspend(|| {
                        println!("duckdb {}", job.args.join(" "));
                    });
                }
                job_sender
                    .send(job)
                    .context("Failed to send token to worker")?;
            }

            drop(job_sender);

            for handle in worker_handles {
                handle.await.context("Worker task panicked")?;
            }

            output_handle
                .await
                .context("Output printer task panicked")?;

            pb.finish();
            println!("Completed in {:?}", pb.elapsed())
        }
        Command::Init { name } => {
            let project_dir = std::path::Path::new(&name);
            if project_dir.exists() {
                anyhow::bail!("Directory {} already exists", name);
            }
            std::fs::create_dir(project_dir)
                .with_context(|| format!("Creating project directory {}", name))?;

            let template_sql = include_str!("../map_template.sql");
            let script_path = format!("{}.sql", name);
            std::fs::write(&script_path, template_sql)
                .with_context(|| format!("Writing example SQL script to {}", script_path))?;

            println!(
                "Created new HyperQuack map project:\n\
                 - Results directory: {name}/\n\
                 - SQL script: {script_path}\n\n\
                 Edit {script_path} and run:\n\
                 hq map <glob_pattern> {script_path}\n",
            );
        }
    }

    Ok(())
}

#[derive(Debug)]
struct Job {
    member: Member,
    client: MotherduckClient,
    token_type: TokenType,
    args: Vec<String>,
}

impl Job {
    async fn perform(&self, output_sender: flume::Sender<OutputLine>) -> Result<()> {
        let token = self
            .client
            .create_token(&self.member, self.token_type, None)?;
        let mut child = tokio::process::Command::new("duckdb")
            .args(&self.args)
            .env("motherduck_user", &self.member.name)
            .env("motherduck_token", &token.token)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("Failed to spawn duckdb process")?;

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
                            let _ = output_sender.send(OutputLine::Stdout(self.member.name.clone(), line));
                        }
                        Ok(None) => stdout_done = true,
                        Err(_) => stdout_done = true,
                    }
                }
                result = stderr_reader.next_line(), if !stderr_done => {
                    match result {
                        Ok(Some(line)) => {
                            let _ = output_sender.send(OutputLine::Stderr(self.member.name.clone(), line));
                        }
                        Ok(None) => stderr_done = true,
                        Err(_) => stderr_done = true,
                    }
                }
            }
        }

        let status = child
            .wait()
            .await
            .context("Failed to wait for duckdb process")?;

        if !status.success() {
            anyhow::bail!("duckdb exited with status: {}", status);
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
                    job.member.name,
                    format!("Job failed: {}", e),
                ));
            }
            pb.inc(1);
        }
    }
}
