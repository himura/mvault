//! The commands run in a workspace.

use std::cell::RefCell;
use std::path::PathBuf;

use clap::Args;

use super::{Context, Reporter};
use crate::auth;
use crate::error::{Error, Result};
use crate::node::add::{self, Added};
use crate::node::config::global_path;
use crate::node::manifest::MANIFEST_NAME;
use crate::node::resolve::Access;
use crate::node::sync::{self, Event, Options};
use crate::node::workspace::{self, Workspace};
use crate::source::{DEFAULT_HF_HOST, Reference};
use crate::upstream::civitai::Site;
use crate::upstream::hf::Hub;

#[derive(Debug, Args)]
pub(super) struct InitArgs {
    /// Directory to make a workspace [default: the current directory]
    #[arg(value_name = "DIR")]
    dir: Option<PathBuf>,
    /// A mirror for this workspace, instead of those of the node: a path or
    /// a file://, http:// or https:// URL (repeatable)
    #[arg(long, value_name = "MIRROR")]
    mirror: Vec<String>,
    /// Let this workspace fetch from the upstream what no mirror has
    #[arg(long)]
    allow_official: bool,
}

#[derive(Debug, Args)]
pub(super) struct AddArgs {
    /// A page URL, `hf:<org>/<repo>` or `civitai:<host>:<modelId>[@<versionId>]`
    #[arg(value_name = "SOURCE")]
    source: String,
    /// Where to place the model, relative to the workspace: a directory
    /// (ending in `/`) or, for a single file, a file name
    #[arg(long, value_name = "PATH")]
    dest: String,
    /// Place only this file of a repository, its path taken as it is
    /// (repeatable)
    #[arg(long, value_name = "PATH")]
    path: Vec<String>,
    /// Place only the files of a repository matching this glob, by their
    /// names in the directory --dest (repeatable)
    #[arg(long, value_name = "GLOB")]
    glob: Vec<String>,
    /// Place this file of a model version instead of the primary one
    /// (repeatable)
    #[arg(long, value_name = "ID")]
    file_id: Vec<u64>,
    /// Branch, tag or commit of a repository [default: main]
    #[arg(long, value_name = "REVISION")]
    revision: Option<String>,
    /// The manifest file to append to, relative to the workspace
    #[arg(long, value_name = "FILE", default_value = MANIFEST_NAME)]
    file: String,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct SyncArgs {
    /// Only show what would be done
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Fail if the lock file lacks a model of the manifest, instead of
    /// resolving it
    #[arg(long)]
    locked: bool,
    /// Remove placed files that the manifest no longer has
    #[arg(long)]
    prune: bool,
    /// Replace files that are in the way of a model, and fetch again blobs
    /// that were written to
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
pub(super) struct GcArgs {
    /// Only show what would be removed
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Keep blobs whose last link went away more recently than this, such
    /// as 12h or 7d
    #[arg(long, value_name = "AGE", default_value = "1d", value_parser = crate::gc::parse_duration)]
    min_age: std::time::Duration,
}

pub(super) fn gc(args: &GcArgs, context: &mut Context<'_>) -> Result<()> {
    let workspace = open(context)?;
    let collected = crate::node::gc::gc(&workspace, args.min_age, args.dry_run)?;
    for path in &collected.strays {
        let _ = writeln!(
            context.err,
            "warning: {} is not a blob at its place; left alone",
            path.display()
        );
    }
    let sizes: Vec<u64> = collected.blobs.iter().map(|(_, size)| *size).collect();
    super::report_gc(context, args.dry_run, &sizes, collected.partials.len())
}

pub(super) fn init(args: &InitArgs, context: &mut Context<'_>) -> Result<()> {
    let dir = match &args.dir {
        Some(dir) => context.cwd.join(dir),
        None => context.cwd.clone(),
    };
    // Written as absolute paths: the settings must mean the same from
    // wherever mvault runs.
    let mirrors: Vec<String> = args
        .mirror
        .iter()
        .map(|mirror| {
            let is_path = !mirror.contains("://");
            if is_path && !mirror.is_empty() && !std::path::Path::new(mirror).has_root() {
                context.cwd.join(mirror).to_string_lossy().into_owned()
            } else {
                mirror.clone()
            }
        })
        .collect();
    let created = workspace::init(&dir, &mirrors, args.allow_official)?;
    for path in &created {
        say(context, &format!("created {}", path.display()))?;
    }
    if created.is_empty() {
        say(
            context,
            &format!("{} is already a workspace", dir.display()),
        )?;
    }
    Ok(())
}

pub(super) fn add(args: &AddArgs, context: &mut Context<'_>) -> Result<()> {
    let workspace = open(context)?;
    let reference = Reference::parse(&args.source, &workspace.defaults())
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let request = add::Request {
        reference: &reference,
        dest: &args.dest,
        paths: &args.path,
        globs: &args.glob,
        file_ids: &args.file_id,
        revision: args.revision.as_deref(),
        file: &args.file,
    };
    let added = with_access(&workspace, context, |access, _| {
        add::add(&workspace, access, &request)
    })?;
    match added {
        Added::Appended { entry, resolved } => say(
            context,
            &format!(
                "added to {} (resolved to {resolved}):\n\n{entry}",
                args.file
            ),
        ),
        Added::AlreadyThere => say(context, "the manifest already has this entry"),
    }
}

pub(super) fn sync(args: &SyncArgs, context: &mut Context<'_>) -> Result<()> {
    let workspace = open(context)?;
    let options = Options {
        dry_run: args.dry_run,
        locked: args.locked,
        prune: args.prune,
        force: args.force,
    };
    let report = with_access(&workspace, context, |access, reporter| {
        sync::sync(&workspace, access, options, &mut |event| {
            // Progress output is best effort.
            let _ = match event {
                Event::Resolved { source, resolved } => {
                    writeln!(reporter.err, "resolved {source} to {resolved}")
                }
                Event::Fetching { index, count, dest } => {
                    reporter.begin(format!("[{index}/{count}] {dest}"))
                }
                Event::Progress(progress) => reporter.progress(progress),
                Event::Fetched => reporter.end("fetched"),
                Event::Linked(_) => Ok(()),
                Event::Conflict(dest) => writeln!(
                    reporter.err,
                    "conflict: {dest} exists and is not managed by mvault (--force replaces it)"
                ),
                Event::Damaged(dest) => writeln!(
                    reporter.err,
                    "damaged: the blob of {dest} was written to and no longer has its digest \
                     (--force fetches it again)"
                ),
                Event::Pruned(dest) => writeln!(reporter.err, "pruned {dest}"),
                Event::Kept(dest) => writeln!(
                    reporter.err,
                    "kept {dest}: its blob was written to (--force lets it go as an ordinary file)"
                ),
            };
        })
    })?;
    let would = if args.dry_run { "would be " } else { "" };
    say(
        context,
        &format!(
            "{} resolved, {} fetched, {} linked, {} unchanged, {} pruned ({would}done)",
            report.resolved, report.fetched, report.linked, report.unchanged, report.pruned
        ),
    )?;
    if !report.dropped.is_empty() {
        say(
            context,
            &format!(
                "{} placed file(s) are no longer in the manifest (--prune removes them)",
                report.dropped.len()
            ),
        )?;
    }
    let mut problems = Vec::new();
    if !report.conflicts.is_empty() {
        problems.push(format!(
            "{} destination(s) are taken by files mvault does not manage",
            report.conflicts.len()
        ));
    }
    if !report.damaged.is_empty() {
        problems.push(format!(
            "{} file(s) have a blob that was written to",
            report.damaged.len()
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::Invalid(problems.join("; ")))
    }
}

fn open(context: &Context<'_>) -> Result<Workspace> {
    let root = Workspace::find(&context.cwd)?;
    Workspace::open(root, global_path(context.env).as_deref())
}

fn say(context: &mut Context<'_>, text: &str) -> Result<()> {
    writeln!(context.out, "{text}")
        .map_err(|e| Error::Invalid(format!("cannot write the result: {e}")))
}

/// Runs `run` with access to the mirrors of the workspace and to the
/// upstreams, then prints the warnings that came up.
fn with_access<T>(
    workspace: &Workspace,
    context: &mut Context<'_>,
    run: impl FnOnce(&Access<'_>, &mut Reporter<'_>) -> Result<T>,
) -> Result<T> {
    let remotes = workspace.remotes()?;
    // Each token is for one host only; see `mvault mirror pull`.
    let hf_token = auth::hf_token(context.env)?;
    let civitai_token = auth::civitai_token(context.env)?;
    let civitai_host = workspace.config.civitai_host.clone();
    let base_url = context.base_url;
    let access = Access {
        http: &context.http,
        remotes: &remotes,
        hub: &|host| {
            let token = hf_token.as_deref().filter(|_| host == DEFAULT_HF_HOST);
            Hub::new(&base_url(host), token)
        },
        site: &|host| {
            let token = civitai_token
                .as_deref()
                .filter(|_| civitai_host.as_deref() == Some(host));
            Site::new(&base_url(host), token)
        },
        warnings: RefCell::default(),
        down: RefCell::default(),
    };
    let mut reporter = Reporter {
        err: &mut *context.err,
        live: context.live,
        label: String::new(),
        drawn: None,
    };
    let result = run(&access, &mut reporter);
    for warning in access.warnings.borrow().iter() {
        let _ = writeln!(reporter.err, "warning: {warning}");
    }
    result
}
