//! The command line.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};

use crate::auth::{self, Env};
use crate::download::Client;
use crate::error::{Error, Result};
use crate::mirror::config::Config;
use crate::mirror::exclude::Excludes;
use crate::mirror::{CivitaiPull, Event, HfPull, Mirror, Outcome, Pulled, Selection};
use crate::source::{DEFAULT_HF_HOST, Defaults, Reference, Source};
use crate::upstream::civitai::Site;
use crate::upstream::hf::Hub;

/// A vault of ML models: mirror them once, place them on every node.
#[derive(Debug, Parser)]
#[command(name = "mvault", version)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
    /// Fetch a repository or a model version from its upstream
    Pull(PullArgs),
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
    /// Do not fetch the files of a repository matching this glob, or
    /// `/regex/` (repeatable)
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,
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
    match cli.command {
        Command::Mirror(mirror) => {
            let root = mirror.root.ok_or_else(|| {
                Error::Invalid("no mirror given: pass --root or set MVAULT_MIRROR_ROOT".into())
            })?;
            match mirror.command {
                MirrorCommand::Pull(args) => pull(&root, &args, context),
            }
        }
    }
}

fn pull(root: &std::path::Path, args: &PullArgs, context: &mut Context<'_>) -> Result<()> {
    let config = Config::load(root)?;
    let default_civitai_host = config.civitai.default_host;
    let reference = parse_source(args, default_civitai_host.as_deref())?;
    let mut mirror = Mirror::open(root)?;
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
            // The token of the default hub is not for other hosts.
            let token = if host == DEFAULT_HF_HOST {
                auth::hf_token(context.env)?
            } else {
                None
            };
            let hub = Hub::new(&(context.base_url)(host), token.as_deref());
            let pull = HfPull {
                hub: &hub,
                host,
                repo,
                revision,
                excludes: &Excludes::new(&args.exclude)?,
            };
            mirror.pull_hf(&context.http, &pull, &mut |event| reporter.handle(event))?
        }
        Source::Civitai {
            host,
            model_id,
            version_id,
        } => {
            if args.revision.is_some() || !args.exclude.is_empty() {
                return Err(Error::Invalid(
                    "--revision and --exclude are for repositories, not model versions".into(),
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
            // The token is for the configured site only: any https URL of
            // the right shape is taken for a site of this kind.
            let token = if default_civitai_host.as_deref() == Some(host) {
                auth::civitai_token(context.env)?
            } else {
                None
            };
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
                self.label = format!("[{index}/{count}] {path}");
                self.drawn = None;
                if self.live {
                    write!(self.err, "{}", self.label)?;
                    self.err.flush()?;
                }
            }
            Event::Progress(progress) => {
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
            }
            Event::Finished(outcome) => {
                let word = match outcome {
                    Outcome::Downloaded => "downloaded",
                    Outcome::Reused => "reused",
                    Outcome::Present => "already present",
                };
                let redraw = if self.live { "\r\x1b[K" } else { "" };
                writeln!(self.err, "{redraw}{}  {word}", self.label)?;
            }
            Event::Warning(message) => writeln!(self.err, "warning: {message}")?,
        }
        Ok(())
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

#[cfg(test)]
mod tests;
