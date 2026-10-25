use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::Config;
use crate::credentials::Credentials;

/// Flags for every bisync run.
const BISYNC_FLAGS: &[&str] = &[
    // an in-memory config: the remote comes from the environment
    "--config=",
    "--use-json-log",
    "--color=NEVER",
    "--resilient",
    "--recover",
    "--max-lock=5m",
    "--create-empty-src-dirs",
    "--conflict-resolve=newer",
    "--conflict-loser=num",
    "--conflict-suffix=conflict",
    // Nextcloud answers one directory per WebDAV request; 16 in parallel list about
    // 30% faster than rclone's default of 8, more gain nothing
    "--checkers=16",
    // a progress entry every half minute, so long syncs show where they are
    "--stats=30s",
    "--stats-log-level=NOTICE",
];

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Normal,
    /// Rebuilds the listings by merging both sides; never deletes.
    Resync,
    /// Bypasses the "too many deletes" safety check.
    Force,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Synced {
        conflicts: usize,
    },
    NeedsResync,
    /// rclone refused to sync; `forceable` if `--force` overrides it.
    SafetyAbort {
        reason: String,
        forceable: bool,
    },
    /// The server rejected the app password. rclone cannot tell this apart from
    /// other failures, so the caller sets it after asking the server.
    Unauthorized,
    Failed(String),
}

/// Runs `rclone bisync` between the local folder and one remote.
pub struct Bisync {
    rclone: PathBuf,
    local: PathBuf,
    remote: String,
    state: PathBuf,
    env: Vec<(&'static str, String)>,
    extra_args: Vec<String>,
}

impl Bisync {
    /// Syncs against the user's Nextcloud files over WebDAV.
    pub async fn nextcloud(config: &Config, credentials: &Credentials) -> Result<Self> {
        let password = obscure(&config.rclone, &credentials.app_password).await?;
        let env = vec![
            ("RCLONE_WEBDAV_URL", credentials.dav_url()),
            ("RCLONE_WEBDAV_VENDOR", "nextcloud".into()),
            ("RCLONE_WEBDAV_USER", credentials.login_name.clone()),
            ("RCLONE_WEBDAV_PASS", password),
        ];
        let remote = format!(":webdav:{}", config.remote_dir.trim_matches('/'));
        Self::new(config, remote, env).await
    }

    pub async fn new(
        config: &Config,
        remote: String,
        env: Vec<(&'static str, String)>,
    ) -> Result<Self> {
        let state = config.state_dir();
        for dir in [&state.join("bisync"), &config.local_dir] {
            tokio::fs::create_dir_all(dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
        }

        // bisync stores a hash of the filters and demands a resync when it changes,
        // so the file is only rewritten when its content differs
        let filters_file = state.join("filters.txt");
        let rules = config.filter_rules();
        if tokio::fs::read_to_string(&filters_file).await.ok() != Some(rules.clone()) {
            tokio::fs::write(&filters_file, rules).await?;
        }

        Ok(Self {
            rclone: config.rclone.clone(),
            local: config.local_dir.clone(),
            remote,
            state,
            env,
            extra_args: config.rclone_args.clone(),
        })
    }

    /// Syncs in `mode`; a normal sync that rclone cannot continue without a resync
    /// (first run, changed filters) merges both sides once instead.
    pub async fn sync(&self, mode: Mode) -> Result<Outcome> {
        let outcome = self.run(mode).await?;
        if matches!(mode, Mode::Normal) && outcome == Outcome::NeedsResync {
            tracing::warn!("bisync needs a resync, merging both sides");
            return self.run(Mode::Resync).await;
        }
        Ok(outcome)
    }

    pub async fn run(&self, mode: Mode) -> Result<Outcome> {
        tracing::info!("running bisync ({mode:?})");
        let mut command = Command::new(&self.rclone);
        command
            .arg("bisync")
            .arg(&self.local)
            .arg(&self.remote)
            .arg("--workdir")
            .arg(self.state.join("bisync"))
            .arg("--filters-file")
            .arg(self.state.join("filters.txt"))
            .args(BISYNC_FLAGS);
        match mode {
            Mode::Normal => {}
            Mode::Resync => {
                command.arg("--resync-mode=newer");
            }
            Mode::Force => {
                command.arg("--force");
            }
        }
        let mut child = command
            .args(&self.extra_args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", self.rclone.display()))?;

        // rclone logs everything to stderr
        let mut lines = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
        let mut scan = Scan::default();
        while let Some(line) = lines.next_line().await? {
            let entry = LogEntry::parse(&line);
            match entry.stats() {
                // the message repeats the stats as a multi-line table
                Some(stats) => tracing::info!(target: "rclone", "progress: {stats}"),
                None if entry.stats.is_some() => {}
                None => {
                    tracing::info!(target: "rclone", "{}: {}", entry.level, entry.msg);
                    scan.entry(&entry);
                }
            }
        }

        let status = child.wait().await?;
        Ok(scan.outcome(status.success()))
    }
}

/// One line of `--use-json-log` output.
#[derive(Deserialize)]
struct LogEntry {
    level: String,
    msg: String,
    #[serde(default)]
    object: Option<String>,
    /// Kept raw, so an unexpected shape only loses the progress line, not the entry.
    #[serde(default)]
    stats: Option<serde_json::Value>,
}

impl LogEntry {
    /// Anything rclone prints outside its logger, such as a panic, counts as an error.
    fn parse(line: &str) -> Self {
        serde_json::from_str(line).unwrap_or_else(|_| Self {
            level: "error".into(),
            msg: line.to_string(),
            object: None,
            stats: None,
        })
    }

    fn stats(&self) -> Option<Stats> {
        serde_json::from_value(self.stats.clone()?).ok()
    }
}

/// The counters of a `--stats` entry.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Stats {
    listed: f64,
    checks: f64,
    total_checks: f64,
    transfers: f64,
    total_transfers: f64,
    bytes: f64,
    total_bytes: f64,
    speed: f64,
    eta: Option<f64>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            listed: 0.0,
            checks: 0.0,
            total_checks: 0.0,
            transfers: 0.0,
            total_transfers: 0.0,
            bytes: 0.0,
            total_bytes: 0.0,
            speed: 0.0,
            eta: None,
        }
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "listed {}", self.listed)?;
        if self.total_checks > 0.0 {
            write!(f, ", checked {}/{}", self.checks, self.total_checks)?;
        }
        if self.total_transfers > 0.0 {
            write!(
                f,
                ", transferred {}/{} files, {} of {} at {}/s",
                self.transfers,
                self.total_transfers,
                mib(self.bytes),
                mib(self.total_bytes),
                mib(self.speed),
            )?;
            if let Some(eta) = self.eta {
                write!(f, ", {eta:.0}s left")?;
            }
        }
        Ok(())
    }
}

fn mib(bytes: f64) -> String {
    format!("{:.1} MiB", bytes / (1024.0 * 1024.0))
}

/// What a bisync run reported, collected from its log entries.
#[derive(Default)]
struct Scan {
    conflicts: usize,
    needs_resync: bool,
    safety_abort: Option<(String, bool)>,
    first_error: Option<String>,
}

impl Scan {
    fn entry(&mut self, entry: &LogEntry) {
        let msg = entry.msg.trim();
        let lower = msg.to_lowercase();
        // "New or changed in both paths" is also logged for files that turn out to be
        // identical; only a renamed loser copy is a real conflict
        if (lower.contains("renaming path1 copy") || lower.contains("renaming path2 copy"))
            && !lower.contains("not renaming")
        {
            self.conflicts += 1;
        }
        // --resilient words the missing listings of a first run as retryable, but
        // only a resync ever recovers from them
        if lower.contains("must run --resync")
            || lower.contains("cannot find prior path1 or path2 listings")
        {
            self.needs_resync = true;
        }
        if entry.object.as_deref() == Some("Safety abort")
            || lower.starts_with("safety abort")
            || lower.contains("cannot sync to an empty directory")
        {
            self.safety_abort
                .get_or_insert_with(|| (msg.to_string(), lower.contains("--force")));
        }
        if entry.level == "error" {
            self.first_error.get_or_insert_with(|| msg.to_string());
        }
    }

    fn outcome(self, success: bool) -> Outcome {
        if success {
            Outcome::Synced {
                conflicts: self.conflicts,
            }
        } else if let Some((reason, forceable)) = self.safety_abort {
            Outcome::SafetyAbort { reason, forceable }
        } else if self.needs_resync {
            Outcome::NeedsResync
        } else {
            Outcome::Failed(
                self.first_error
                    .unwrap_or_else(|| "rclone exited with an error".into()),
            )
        }
    }
}

/// Turns a password into rclone's obscured form, as its config and env vars expect.
async fn obscure(rclone: &Path, password: &str) -> Result<String> {
    let mut child = Command::new(rclone)
        .args(["obscure", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", rclone.display()))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(password.as_bytes()).await?;
    drop(stdin);
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        bail!(
            "rclone obscure failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, msg: &str) -> String {
        serde_json::json!({ "level": level, "msg": msg }).to_string()
    }

    fn scan(lines: &[String], success: bool) -> Outcome {
        let mut scan = Scan::default();
        for line in lines {
            scan.entry(&LogEntry::parse(line));
        }
        scan.outcome(success)
    }

    #[test]
    fn counts_renamed_losers_as_conflicts() {
        let outcome = scan(
            &[
                entry(
                    "notice",
                    "- WARNING   New or changed in both paths   - a.txt",
                ),
                entry(
                    "notice",
                    "- Path1     Renaming Path1 copy            - l/a.txt.conflict1",
                ),
                entry(
                    "notice",
                    "- Path2     Not renaming Path2 copy, as it was determined the winner - r/a.txt",
                ),
            ],
            true,
        );
        assert_eq!(outcome, Outcome::Synced { conflicts: 1 });
    }

    #[test]
    fn identical_files_changed_on_both_sides_are_no_conflict() {
        let outcome = scan(
            &[
                entry(
                    "notice",
                    "- WARNING   New or changed in both paths   - a.txt",
                ),
                entry("error", "a.txt: failed to check as hash is missing"),
            ],
            true,
        );
        assert_eq!(outcome, Outcome::Synced { conflicts: 0 });
    }

    #[test]
    fn first_run_in_resilient_mode_needs_resync() {
        let outcome = scan(
            &[
                entry(
                    "error",
                    "Bisync critical error: cannot find prior Path1 or Path2 listings, likely due to critical error on prior run",
                ),
                entry(
                    "error",
                    "Bisync aborted. Error is retryable without --resync due to --resilient mode.",
                ),
            ],
            false,
        );
        assert_eq!(outcome, Outcome::NeedsResync);
    }

    #[test]
    fn safety_abort_is_reported_with_its_reason() {
        let line = serde_json::json!({
            "level": "error",
            "msg": "too many deletes (>50%, 10 of 12) on Path1 \"/home/x\". Run with --force if desired.",
            "object": "Safety abort",
        })
        .to_string();
        let outcome = scan(
            &[line, entry("notice", "Failed to bisync: too many deletes")],
            false,
        );
        assert_eq!(
            outcome,
            Outcome::SafetyAbort {
                reason: "too many deletes (>50%, 10 of 12) on Path1 \"/home/x\". Run with --force if desired."
                    .into(),
                forceable: true,
            }
        );
    }

    #[test]
    fn failure_reports_the_first_error() {
        let outcome = scan(
            &[
                entry("error", "couldn't list files: 503 Service Unavailable"),
                entry(
                    "error",
                    "Bisync aborted. Error is retryable without --resync due to --resilient mode.",
                ),
            ],
            false,
        );
        assert_eq!(
            outcome,
            Outcome::Failed("couldn't list files: 503 Service Unavailable".into())
        );
    }

    #[test]
    fn stats_entries_become_one_progress_line() {
        let line = r#"{"level":"notice","msg":"\nTransferred: ...","stats":{"bytes":21093120,"checks":12,"elapsedTime":1.0,"eta":2,"listed":30,"speed":21123216.7,"totalBytes":90000000,"totalChecks":40,"totalTransfers":30,"transfers":4}}"#;
        let stats = LogEntry::parse(line).stats().expect("a stats entry");
        assert_eq!(
            stats.to_string(),
            "listed 30, checked 12/40, transferred 4/30 files, 20.1 MiB of 85.8 MiB at 20.1 MiB/s, 2s left"
        );
    }

    #[test]
    fn listing_progress_shows_only_what_is_known() {
        let line = r#"{"level":"notice","msg":"","stats":{"bytes":0,"checks":0,"eta":null,"listed":1200,"speed":0,"totalBytes":0,"totalChecks":0,"totalTransfers":0,"transfers":0}}"#;
        let stats = LogEntry::parse(line).stats().expect("a stats entry");
        assert_eq!(stats.to_string(), "listed 1200");
    }

    #[test]
    fn unexpected_stats_shape_keeps_the_entry() {
        let entry = LogEntry::parse(r#"{"level":"notice","msg":"m","stats":{"listed":"many"}}"#);
        assert_eq!(entry.msg, "m");
        assert!(entry.stats().is_none());
    }

    #[test]
    fn non_json_output_counts_as_an_error() {
        let outcome = scan(&["panic: something broke".to_string()], false);
        assert_eq!(outcome, Outcome::Failed("panic: something broke".into()));
    }

    /// Runs real rclone between two local folders, so the log matching above is
    /// checked against what rclone actually prints.
    mod with_rclone {
        use super::super::*;
        use std::time::{Duration, SystemTime};

        struct Fixture {
            _root: tempfile::TempDir,
            local: PathBuf,
            remote: PathBuf,
            bisync: Bisync,
        }

        async fn fixture() -> Fixture {
            let root = tempfile::tempdir().unwrap();
            let local = root.path().join("local");
            let remote = root.path().join("remote");
            std::fs::create_dir_all(&remote).unwrap();
            let config: Config = toml::from_str(&format!(
                "server = \"https://cloud.example.com\"\nlocal_dir = {:?}\nstate_dir = {:?}",
                local,
                root.path().join("state"),
            ))
            .unwrap();
            let bisync = Bisync::new(&config, remote.to_string_lossy().into_owned(), vec![])
                .await
                .unwrap();
            Fixture {
                _root: root,
                local,
                remote,
                bisync,
            }
        }

        fn write(path: &Path, content: &str, age: Duration) {
            std::fs::write(path, content).unwrap();
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(SystemTime::now() - age).unwrap();
        }

        fn read(path: PathBuf) -> String {
            std::fs::read_to_string(path).unwrap()
        }

        fn count(dir: &Path) -> usize {
            std::fs::read_dir(dir).unwrap().count()
        }

        #[tokio::test]
        async fn first_run_resyncs_then_syncs() {
            let f = fixture().await;
            write(&f.remote.join("a.txt"), "remote", Duration::ZERO);

            assert_eq!(
                f.bisync.run(Mode::Normal).await.unwrap(),
                Outcome::NeedsResync
            );
            assert_eq!(
                f.bisync.sync(Mode::Normal).await.unwrap(),
                Outcome::Synced { conflicts: 0 }
            );
            assert_eq!(read(f.local.join("a.txt")), "remote");

            write(&f.local.join("b.txt"), "local", Duration::ZERO);
            assert_eq!(
                f.bisync.sync(Mode::Normal).await.unwrap(),
                Outcome::Synced { conflicts: 0 }
            );
            assert_eq!(read(f.remote.join("b.txt")), "local");
        }

        #[tokio::test]
        async fn conflicts_keep_the_newer_file_and_are_counted() {
            let f = fixture().await;
            write(&f.remote.join("a.txt"), "base", Duration::from_secs(600));
            // with a single file, a conflict would count as "all files changed"
            write(
                &f.remote.join("other.txt"),
                "other",
                Duration::from_secs(600),
            );
            f.bisync.run(Mode::Resync).await.unwrap();

            write(
                &f.local.join("a.txt"),
                "older local edit",
                Duration::from_secs(120),
            );
            write(
                &f.remote.join("a.txt"),
                "newer remote edit",
                Duration::from_secs(60),
            );
            assert_eq!(
                f.bisync.run(Mode::Normal).await.unwrap(),
                Outcome::Synced { conflicts: 1 }
            );
            assert_eq!(read(f.local.join("a.txt")), "newer remote edit");
            assert_eq!(read(f.local.join("a.txt.conflict1")), "older local edit");
        }

        #[tokio::test]
        async fn mass_delete_is_a_forceable_safety_abort() {
            let f = fixture().await;
            for i in 0..10 {
                write(&f.remote.join(format!("{i}.txt")), "x", Duration::ZERO);
            }
            f.bisync.run(Mode::Resync).await.unwrap();
            for i in 0..6 {
                std::fs::remove_file(f.local.join(format!("{i}.txt"))).unwrap();
            }
            let outcome = f.bisync.run(Mode::Normal).await.unwrap();
            assert!(
                matches!(&outcome, Outcome::SafetyAbort { reason, forceable: true } if reason.contains("too many deletes")),
                "{outcome:?}"
            );
            assert_eq!(count(&f.remote), 10);

            assert_eq!(
                f.bisync.run(Mode::Force).await.unwrap(),
                Outcome::Synced { conflicts: 0 }
            );
            assert_eq!(count(&f.remote), 4);
        }

        #[tokio::test]
        async fn emptied_folder_is_a_safety_abort_even_with_force() {
            let f = fixture().await;
            for i in 0..3 {
                write(&f.remote.join(format!("{i}.txt")), "x", Duration::ZERO);
            }
            f.bisync.run(Mode::Resync).await.unwrap();
            std::fs::remove_dir_all(&f.local).unwrap();
            std::fs::create_dir(&f.local).unwrap();
            for mode in [Mode::Normal, Mode::Force] {
                let outcome = f.bisync.run(mode).await.unwrap();
                assert!(
                    matches!(
                        outcome,
                        Outcome::SafetyAbort {
                            forceable: false,
                            ..
                        }
                    ),
                    "{mode:?}: {outcome:?}"
                );
            }
            assert_eq!(count(&f.remote), 3);
        }

        #[tokio::test]
        async fn excluded_files_stay_local() {
            let f = fixture().await;
            write(&f.remote.join("existing.txt"), "x", Duration::ZERO);
            f.bisync.run(Mode::Resync).await.unwrap();
            write(
                &f.local.join(".owncloudsync.log"),
                "old client",
                Duration::ZERO,
            );
            write(&f.local.join("doc.odt"), "doc", Duration::ZERO);
            write(&f.local.join(".~lock.doc.odt#"), "lock", Duration::ZERO);
            assert_eq!(
                f.bisync.run(Mode::Normal).await.unwrap(),
                Outcome::Synced { conflicts: 0 }
            );
            assert!(f.remote.join("doc.odt").exists());
            assert!(!f.remote.join(".owncloudsync.log").exists());
            assert!(!f.remote.join(".~lock.doc.odt#").exists());
        }
    }
}
