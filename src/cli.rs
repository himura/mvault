//! The command line.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};

use crate::auth::{Env, Tokens, default_config_dir};
use crate::download::{Client, Progress};
use crate::error::{Error, Result};
use crate::mirror::config::Config;
use crate::mirror::exclude::Excludes;
use crate::mirror::{CivitaiPull, Event, HfPull, Mirror, Outcome, Pulled, Selection};
use crate::source::{Defaults, Reference, Source};
use crate::upstream::civitai::Site;
use crate::upstream::hf::Hub;

/// A vault of ML models: mirror them once, place them on every node.
#[derive(Debug, Parser)]
#[command(name = "mvault", version)]
pub struct Cli {
    /// Directory of the settings and tokens of mvault
    /// (by default mvault in the config directory of the user)
    #[arg(long, env = "MVAULT_CONFIG_DIR", global = true, value_name = "DIR")]
    config_dir: Option<PathBuf>,
    /// Write a log of what is done and why on stderr
    #[arg(long, env = "MVAULT_LOG", global = true, value_name = "LEVEL")]
    log: Option<crate::logger::Detail>,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    /// The level of the log that `--log` asks for; off without it.
    #[must_use]
    pub fn log_level(&self) -> log::LevelFilter {
        crate::logger::level(self.log)
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Make a directory a workspace
    Init(node::InitArgs),
    /// Add a model to the manifest of the workspace
    Add(node::AddArgs),
    /// Fetch and place the models of the manifest
    Sync(node::SyncArgs),
    /// Resolve branches and latest versions again, into the lock file
    Update(node::UpdateArgs),
    /// List the files the manifest places, and their state
    Ls(node::LsArgs),
    /// Show how each model of the manifest stands
    Status(node::StatusArgs),
    /// Remove the blobs of this node that no file is linked to
    Gc(node::GcArgs),
    /// Maintain a mirror (run on the host that stores it)
    Mirror(MirrorArgs),
}

#[derive(Debug, Args)]
struct MirrorArgs {
    /// Directory of the mirror
    #[arg(long, env = "MVAULT_MIRROR_ROOT", global = true, value_name = "DIR")]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: MirrorCommand,
}

#[derive(Debug, Subcommand)]
enum MirrorCommand {
    /// Make a directory a mirror
    Init,
    /// Fetch a repository or a model version from its upstream
    Pull(PullArgs),
    /// List the snapshots of the mirror; with SOURCE, also their files
    Ls(MirrorLsArgs),
    /// Write the files nodes read again, from the database of the mirror
    Export,
    /// Remove the blobs no snapshot uses, and unfinished downloads
    Gc(MirrorGcArgs),
}

#[derive(Debug, Args)]
struct MirrorLsArgs {
    /// A page URL or canonical form, with `@<revision>` for one snapshot
    #[arg(value_name = "SOURCE")]
    source: Option<String>,
}

#[derive(Debug, Args)]
struct MirrorGcArgs {
    /// Only show what would be removed
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Keep blobs stored more recently than this, such as 12h or 7d: a
    /// pull that is still running may be about to use them
    #[arg(long, value_name = "AGE", default_value = "1d", value_parser = crate::gc::parse_duration)]
    min_age: Duration,
}

#[derive(Debug, Args)]
struct PullArgs {
    /// A page URL, `hf:<org>/<repo>`, `civitai:<host>:<modelId>[@<versionId>]`,
    /// or two words: `hf <org>/<repo>`, `civitai <modelId>[@<versionId>]`
    #[arg(required = true, num_args = 1..=2, value_name = "SOURCE")]
    source: Vec<String>,
    /// Branch, tag or commit of a repository [default: main]
    #[arg(long, value_name = "REVISION")]
    revision: Option<String>,
    /// Do not fetch the files of a repository matching this glob, read as
    /// in .gitignore (repeatable)
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,
    /// Do not fetch the files of a repository whose path matches this
    /// regular expression (repeatable)
    #[arg(long, value_name = "REGEXP")]
    exclude_regexp: Vec<String>,
    /// Host of the upstream, for the two-word form of SOURCE
    #[arg(long, value_name = "HOST")]
    host: Option<String>,
    /// Fetch this file of a model version instead of the primary one
    /// (repeatable)
    #[arg(long, value_name = "ID")]
    file_id: Vec<u64>,
    /// Fetch every file of a model version
    #[arg(long, conflicts_with = "file_id")]
    all_files: bool,
}

/// What the commands need from outside the process.
pub struct Context<'a> {
    /// Looks up an environment variable.
    pub env: Env<'a>,
    /// The base URL of an upstream host.
    pub base_url: &'a dyn Fn(&str) -> String,
    /// The HTTP client.
    pub http: Client,
    /// The directory the command is run in.
    pub cwd: PathBuf,
    /// Where results go.
    pub out: &'a mut dyn Write,
    /// Where progress and warnings go.
    pub err: &'a mut dyn Write,
    /// Whether `err` is a terminal, on which a line can be redrawn.
    pub live: bool,
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

/// Runs the command `cli` describes.
///
/// # Errors
///
/// Returns the error of the command.
pub fn execute(cli: Cli, context: &mut Context<'_>) -> Result<()> {
    let config_dir = match cli.config_dir {
        Some(dir) => {
            let dir = context.cwd.join(dir);
            // Given on purpose, so a mistyped one must not pass for an
            // empty one.
            if !dir.is_dir() {
                return Err(Error::Invalid(format!(
                    "{}: the settings directory (--config-dir, MVAULT_CONFIG_DIR) is not a directory",
                    dir.display()
                )));
            }
            Some(dir)
        }
        None => default_config_dir(context.env),
    };
    let config_dir = config_dir.as_deref();
    match cli.command {
        Command::Init(args) => node::init(&args, context),
        Command::Add(args) => node::add(&args, config_dir, context),
        Command::Sync(args) => node::sync(&args, config_dir, context),
        Command::Update(args) => node::update(&args, config_dir, context),
        Command::Ls(args) => node::ls(&args, config_dir, context),
        Command::Status(args) => node::status(&args, config_dir, context),
        Command::Gc(args) => node::gc(&args, config_dir, context),
        Command::Mirror(mirror) => {
            let root = mirror.root.ok_or_else(|| {
                Error::Invalid("no mirror given: pass --root or set MVAULT_MIRROR_ROOT".into())
            })?;
            log::debug!("mirror {}", root.display());
            match mirror.command {
                MirrorCommand::Init => mirror_init(&root, context),
                MirrorCommand::Pull(args) => pull(&root, &args, config_dir, context),
                MirrorCommand::Ls(args) => mirror_ls(&root, args.source.as_deref(), context),
                MirrorCommand::Export => {
                    let count = Mirror::open(&root)?.export_all()?;
                    writeln!(context.out, "wrote {count} snapshot(s)")
                        .map_err(|e| Error::Invalid(format!("cannot write the result: {e}")))
                }
                MirrorCommand::Gc(args) => mirror_gc(&root, &args, context),
            }
        }
    }
}

fn pull(
    root: &Path,
    args: &PullArgs,
    config_dir: Option<&Path>,
    context: &mut Context<'_>,
) -> Result<()> {
    let config = Config::load(root)?;
    let default_civitai_host = config.civitai.default_host;
    let reference = parse_source(args, default_civitai_host.as_deref())?;
    let mut mirror = Mirror::open(root)?;
    // Any https URL of the right shape is taken for a site of its kind, so
    // a token goes only to the host its file is named by.
    let tokens = Tokens::new(config_dir, context.env);
    let mut reporter = Reporter {
        err: &mut *context.err,
        live: context.live,
        label: String::new(),
        drawn: None,
    };

    let pulled = match &reference.source {
        Source::HuggingFace { host, repo } => {
            if !args.file_id.is_empty() || args.all_files {
                return Err(Error::Invalid(
                    "--file-id and --all-files are for model versions, not repositories".into(),
                ));
            }
            if reference.path.is_some() {
                return Err(Error::Invalid(
                    "the URL names a file; a mirror pulls whole repositories (use --exclude to leave files out)"
                        .into(),
                ));
            }
            let revision = match (&args.revision, &reference.revision) {
                (Some(given), Some(in_url)) if given != in_url => {
                    return Err(Error::Invalid(format!(
                        "--revision {given} contradicts the revision {in_url} in the URL"
                    )));
                }
                (Some(revision), _) | (None, Some(revision)) => revision.as_str(),
                (None, None) => "main",
            };
            let token = tokens.get(host)?;
            let hub = Hub::new(&(context.base_url)(host), token.as_deref());
            let pull = HfPull {
                hub: &hub,
                host,
                repo,
                revision,
                excludes: &Excludes::new(&args.exclude, &args.exclude_regexp)?,
            };
            mirror.pull_hf(&context.http, &pull, &mut |event| reporter.handle(event))?
        }
        Source::Civitai {
            host,
            model_id,
            version_id,
        } => {
            if args.revision.is_some()
                || !args.exclude.is_empty()
                || !args.exclude_regexp.is_empty()
            {
                return Err(Error::Invalid(
                    "--revision, --exclude and --exclude-regexp are for repositories, not model versions".into(),
                ));
            }
            let mut file_ids = args.file_id.clone();
            file_ids.extend(reference.file_id);
            let selection = if args.all_files {
                Selection::All
            } else if file_ids.is_empty() {
                Selection::Primary
            } else {
                Selection::Files(file_ids)
            };
            let token = tokens.get(host)?;
            let site = Site::new(&(context.base_url)(host), token.as_deref());
            let pull = CivitaiPull {
                site: &site,
                host,
                model_id: *model_id,
                version_id: *version_id,
                selection: &selection,
            };
            mirror.pull_civitai(&context.http, &pull, &mut |event| reporter.handle(event))?
        }
    };
    let Pulled {
        source,
        revision,
        downloaded,
        reused,
        present,
        skipped,
    } = pulled;
    writeln!(
        context.out,
        "{source}@{revision}: {downloaded} downloaded, {reused} reused, {present} already present, {skipped} not fetched"
    )
    .map_err(|e| Error::Invalid(format!("cannot write the result: {e}")))
}

fn mirror_init(root: &std::path::Path, context: &mut Context<'_>) -> Result<()> {
    let (_, created) = Mirror::create(root)?;
    let message = if created {
        format!("created a mirror in {}", root.display())
    } else {
        format!("{} is already a mirror", root.display())
    };
    writeln!(context.out, "{message}")
        .map_err(|e| Error::Invalid(format!("cannot write the result: {e}")))
}

/// Reads a snapshot given on the command line: a source in any form
/// [`Reference::parse`] takes, a `hf:` form followed by `@<revision>`. Returns
/// the source in canonical form without a version, and the revision if one
/// was given.
fn parse_snapshot(text: &str, root: &std::path::Path) -> Result<(String, Option<String>)> {
    let (text, revision) = match text
        .strip_prefix("hf:")
        .and_then(|rest| rest.rsplit_once('@'))
    {
        Some((repo, revision)) => (format!("hf:{repo}"), Some(revision.to_owned())),
        None => (text.to_owned(), None),
    };
    let defaults = Defaults {
        civitai_host: Config::load(root)?.civitai.default_host,
    };
    let reference =
        Reference::parse(&text, &defaults).map_err(|e| Error::Invalid(e.to_string()))?;
    if reference.path.is_some() || reference.file_id.is_some() {
        return Err(Error::Invalid(
            "the URL names a file; a snapshot holds a repository or a model version".into(),
        ));
    }
    match reference.source {
        Source::HuggingFace { .. } => Ok((
            reference.source.to_string(),
            revision.or(reference.revision),
        )),
        Source::Civitai {
            host,
            model_id,
            version_id,
        } => Ok((
            Source::Civitai {
                host,
                model_id,
                version_id: None,
            }
            .to_string(),
            version_id.map(|id| id.to_string()),
        )),
    }
}

fn mirror_ls(
    root: &std::path::Path,
    source: Option<&str>,
    context: &mut Context<'_>,
) -> Result<()> {
    let mirror = Mirror::open(root)?;
    let (source, revision) = match source {
        Some(text) => {
            let (source, revision) = parse_snapshot(text, root)?;
            (Some(source), revision)
        }
        None => (None, None),
    };
    let listings = mirror.list(source.as_deref(), revision.as_deref())?;
    if listings.is_empty() && source.is_some() {
        return Err(Error::Invalid(format!(
            "the mirror has no snapshot of {}",
            source.unwrap_or_default()
        )));
    }
    let mut text = String::new();
    for listing in &listings {
        use std::fmt::Write as _;
        let fetched: Vec<_> = listing.files.iter().filter(|file| file.fetched).collect();
        let size: u64 = fetched.iter().filter_map(|file| file.size).sum();
        let names = if listing.refs.is_empty() {
            String::new()
        } else {
            format!(" ({})", listing.refs.join(", "))
        };
        let _ = writeln!(
            text,
            "{}@{}{names}  {} of {} file(s) fetched, {}",
            listing.source,
            listing.revision,
            fetched.len(),
            listing.files.len(),
            human_size(size)
        );
        if source.is_none() {
            continue;
        }
        let rows: Vec<[String; 3]> = listing
            .files
            .iter()
            .map(|file| {
                let name = match file.file_id {
                    Some(id) => format!("{id}: {}", file.path),
                    None => file.path.clone(),
                };
                let mut state = if file.fetched {
                    "fetched"
                } else {
                    "not fetched"
                }
                .to_owned();
                if file.primary {
                    state.push_str(", primary");
                }
                [
                    name,
                    file.size.map_or_else(|| "-".to_owned(), human_size),
                    state,
                ]
            })
            .collect();
        text.push_str(&table(&rows, "  "));
    }
    write!(context.out, "{text}")
        .map_err(|e| Error::Invalid(format!("cannot write the result: {e}")))
}

/// Lines of columns, each column as wide as its widest cell, the last one
/// not padded. Every line starts with `indent`.
fn table<const N: usize>(rows: &[[String; N]], indent: &str) -> String {
    let mut widths = [0; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in rows {
        let mut line = indent.to_owned();
        for (index, cell) in row.iter().enumerate() {
            if index + 1 == N {
                line.push_str(cell);
            } else {
                let pad = widths[index] - cell.chars().count();
                line.push_str(cell);
                line.push_str(&" ".repeat(pad + 2));
            }
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

fn mirror_gc(root: &std::path::Path, args: &MirrorGcArgs, context: &mut Context<'_>) -> Result<()> {
    let collected = Mirror::open(root)?.gc(args.min_age, args.dry_run)?;
    for path in &collected.untracked {
        let _ = writeln!(
            context.err,
            "warning: {} is not in the database; left alone",
            path.display()
        );
    }
    for blob in &collected.missing {
        let _ = writeln!(
            context.err,
            "warning: blob {} is in the database but its file is gone; used by {}",
            blob.sha256,
            if blob.snapshots.is_empty() {
                "no snapshot".to_owned()
            } else {
                blob.snapshots.join(", ")
            }
        );
    }
    let sizes: Vec<u64> = collected.blobs.iter().map(|(_, size)| *size).collect();
    report_gc(context, args.dry_run, &sizes, collected.partials.len())?;
    if collected.missing.is_empty() {
        Ok(())
    } else {
        let advice = if args.dry_run {
            "run without --dry-run to mark their files as not fetched, then pull the snapshots above again"
        } else {
            "their files are now marked as not fetched; pull the snapshots above again"
        };
        Err(Error::Invalid(format!(
            "{} blob(s) are missing: {advice}",
            collected.missing.len()
        )))
    }
}

/// Prints the result line of a garbage collection.
fn report_gc(
    context: &mut Context<'_>,
    dry_run: bool,
    sizes: &[u64],
    partials: usize,
) -> Result<()> {
    let verb = if dry_run { "would remove" } else { "removed" };
    writeln!(
        context.out,
        "{verb} {} blob(s) ({}), {partials} unfinished download(s)",
        sizes.len(),
        human_size(sizes.iter().sum())
    )
    .map_err(|e| Error::Invalid(format!("cannot write the result: {e}")))
}

/// Reads SOURCE: one word in any form [`Reference::parse`] takes, or a kind
/// followed by what the canonical form has after the host.
fn parse_source(args: &PullArgs, default_civitai_host: Option<&str>) -> Result<Reference> {
    let host = args.host.as_deref();
    let (text, civitai_host) = match args.source.as_slice() {
        [one] => {
            if host.is_some() {
                return Err(Error::Invalid(
                    "--host is for the forms `hf <repo>` and `civitai <modelId>`".into(),
                ));
            }
            (one.clone(), default_civitai_host)
        }
        [kind, rest] if kind == "hf" => match host {
            Some(host) => (format!("hf:{host}:{rest}"), None),
            None => (format!("hf:{rest}"), None),
        },
        [kind, rest] if kind == "civitai" => {
            (format!("civitai:{rest}"), host.or(default_civitai_host))
        }
        _ => {
            return Err(Error::Invalid(format!(
                "expected `hf <repo>` or `civitai <modelId>[@<versionId>]`, got {:?}",
                args.source.join(" ")
            )));
        }
    };
    let defaults = Defaults {
        civitai_host: civitai_host.map(str::to_owned),
    };
    Reference::parse(&text, &defaults).map_err(|e| Error::Invalid(e.to_string()))
}

/// Tells the user how a pull is going: one line per file, redrawn while the
/// file downloads if the output is a terminal.
struct Reporter<'a> {
    err: &'a mut dyn Write,
    live: bool,
    label: String,
    drawn: Option<Instant>,
}

impl Reporter<'_> {
    const REDRAW_EVERY: Duration = Duration::from_millis(100);

    fn handle(&mut self, event: Event<'_>) {
        // Progress output is best effort: a closed stderr must not stop a pull.
        let _ = self.write(event);
    }

    fn write(&mut self, event: Event<'_>) -> std::io::Result<()> {
        match event {
            Event::Started { index, count, path } => {
                self.begin(format!("[{index}/{count}] {path}"))
            }
            Event::Progress(progress) => self.progress(progress),
            Event::Finished(outcome) => self.end(match outcome {
                Outcome::Downloaded => "downloaded",
                Outcome::Reused => "reused",
                Outcome::Present => "already present",
            }),
            Event::Failed(message) => self.end(&format!("failed: {message}")),
            Event::Warning(message) => writeln!(self.err, "warning: {message}"),
        }
    }

    /// Starts the line of an item.
    fn begin(&mut self, label: String) -> std::io::Result<()> {
        self.label = label;
        self.drawn = None;
        if self.live {
            write!(self.err, "{}", self.label)?;
            self.err.flush()?;
        }
        Ok(())
    }

    /// Redraws the line of the current item with how far it has come.
    fn progress(&mut self, progress: Progress) -> std::io::Result<()> {
        let due = self
            .drawn
            .is_none_or(|at| at.elapsed() >= Self::REDRAW_EVERY);
        if self.live && due {
            self.drawn = Some(Instant::now());
            let total = progress
                .total
                .map_or_else(String::new, |total| format!(" / {}", human_size(total)));
            write!(
                self.err,
                "\r\x1b[K{}  {}{total}",
                self.label,
                human_size(progress.done)
            )?;
            self.err.flush()?;
        }
        Ok(())
    }

    /// Ends the line of the current item with what became of it.
    fn end(&mut self, word: &str) -> std::io::Result<()> {
        let redraw = if self.live { "\r\x1b[K" } else { "" };
        writeln!(self.err, "{redraw}{}  {word}", self.label)
    }
}

/// A size with a binary unit and one decimal, such as `1.5 GiB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut unit = 0;
    let mut scale: u64 = 1024;
    while unit + 1 < UNITS.len() && bytes / scale >= 1024 {
        scale *= 1024;
        unit += 1;
    }
    let tenths = (u128::from(bytes) * 10 / u128::from(scale)).min(u128::from(u64::MAX));
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

mod node;
#[cfg(test)]
mod tests;
