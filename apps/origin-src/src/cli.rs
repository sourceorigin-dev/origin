//! The `origin` command line's argument parsing. The commands themselves
//! are methods of [`Workspace`] and [`ServerConfig`], which print their
//! output.

use std::convert::Infallible;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};
use origin_shared::access::Capability;
use origin_shared::policy::KindPattern;
use origin_shared::{
    ChangeId, Digest, Inputs, Kind, Period, Principal, RepoPath, TargetName, Verdict,
};

use crate::config::UserConfig;
use crate::oplog::Actor;
use crate::plugin::Plugin;
use crate::server::ServerConfig;
use crate::workspace::{Annotated, Payload, Workspace, WorkspaceError};

/// A modern, git-compatible source control system.
#[derive(Debug, Parser)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a repository, with the targets `policy` and `main`.
    Init {
        /// The directory to create it in.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Clone a remote repository. Copies the chains of `policy` and of a
    /// target, and the target's tip, then starts a new change on that tip.
    /// Other snapshots are fetched when something reads them.
    Clone {
        /// The repository: `http://host[:port]/<repository>`, or a path.
        url: String,
        /// Where to create the clone: by default, a directory named after the
        /// repository.
        path: Option<PathBuf>,
        /// The target to follow and start the new change on.
        #[arg(short, long, default_value = "main")]
        target: TargetName,
    },
    /// Serve repositories over HTTP, acting as their authority.
    Serve {
        /// The server configuration file.
        #[arg(short, long, default_value = "origin-server.toml")]
        config: PathBuf,
    },
    /// Manage the repositories a server hosts.
    #[command(subcommand)]
    Repo(RepoCommand),
    /// Manage tokens.
    #[command(subcommand)]
    Token(TokenCommand),
    #[command(flatten)]
    Workspace(WorkspaceCommand),
    /// Any other command runs the plugin `origin-<command>` from `PATH`, such
    /// as `origin-ci` for `origin ci`.
    #[command(external_subcommand)]
    Plugin(Vec<OsString>),
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Create a repository with no working copy, for a server to host. The
    /// server's node becomes the authority of `policy` and `main`.
    Create {
        /// The directory to create it in.
        path: PathBuf,
        /// The server configuration file.
        #[arg(short, long, default_value = "origin-server.toml")]
        config: PathBuf,
        /// Who may change the policy over the API, such as `user:alice`. By
        /// default, only the server's operator may, through the repository's path.
        #[arg(long)]
        admin: Option<Principal>,
    },
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Issue a short-lived token with a narrower scope, for an agent or a CI
    /// runner. By default the remote signs a token that acts for you. With
    /// `--config`, a server signs one for any principal.
    Issue {
        /// A capability the token keeps: `read <path>`, `write <path>`,
        /// `annotate <kind>`, `land <target>`, `land *` or `admin`. Repeat it for
        /// several.
        #[arg(long, required = true)]
        scope: Vec<Capability>,
        /// How long the token lasts, such as `1h`. At most `7d`.
        #[arg(long, default_value = "1h")]
        ttl: Period,
        /// Sign the token with the key of the server this configuration file
        /// describes, instead of asking the remote.
        #[arg(short, long, requires = "principal")]
        config: Option<PathBuf>,
        /// The principal the token acts for, such as `user:agent-1`. Only with
        /// `--config`.
        #[arg(long = "as", requires = "config")]
        principal: Option<Principal>,
    },
}

/// Commands that run in the repository containing the current directory.
/// Commands that read the working copy snapshot it first. A change is
/// named by a unique prefix of its id, or by `@` for the working copy's.
#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Show the working copy's change and the files it changes.
    Status,
    /// Show the working copy's changes, line by line.
    Diff,
    /// Show a change and its diff (the working copy's change by default).
    Show {
        /// The change.
        change: Option<String>,
    },
    /// Finish the current change, and start a new one on top of it.
    New {
        /// What the new change is for.
        #[arg(default_value = "")]
        description: String,
        /// Start the new change on the tip of this target instead.
        #[arg(long)]
        on: Option<TargetName>,
    },
    /// Set the description of the current change.
    Describe {
        /// The description.
        description: String,
    },
    /// Switch the working copy to a change.
    Edit {
        /// The change.
        change: String,
    },
    /// Abandon a change, recording why. The change stays visible to others.
    Abandon {
        /// The change (the working copy's by default). Abandoning the working
        /// copy's change starts a new change on the same base.
        change: Option<String>,
        /// Why the change was abandoned.
        #[arg(long)]
        reason: String,
    },
    /// End a change's divergence with a new revision that evolves from all
    /// its heads.
    Converge {
        /// The change.
        change: String,
    },
    /// Mark conflicted files as resolved, keeping their current content.
    Resolve {
        /// The files, as paths from the root of the repository.
        #[arg(required = true)]
        paths: Vec<RepoPath>,
    },
    /// Show the working copy's stack of changes and the history of the
    /// target it's on.
    Log {
        /// Show this target's history instead.
        target: Option<TargetName>,
        /// Show at most this many acceptances.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Show every revision of a change.
    Evolog {
        /// The change.
        #[arg(default_value = "@")]
        change: String,
    },
    /// Manage the record index.
    #[command(subcommand)]
    Index(IndexCommand),
    /// Issue a statement about a revision, such as a check result or a
    /// review, or with `--on`, about another statement, such as a `flaky`
    /// mark on a failed check. It supersedes your previous statement of the
    /// same kind about the same subject. `--withdraw` takes that one back
    /// instead.
    #[command(
        override_usage = "origin annotate [OPTIONS] <--pass|--fail|--neutral|--withdraw> \
                                <REV> <KIND>\n       origin annotate [OPTIONS] \
                                <--pass|--fail|--neutral|--withdraw> --on <STATEMENT> <KIND>"
    )]
    Annotate(AnnotateArgs),
    /// Show the statements about a revision, each with the statements
    /// about it indented under it.
    Statements {
        /// The revision: `@`, a change for its head, or a revision.
        #[arg(default_value = "@")]
        rev: String,
    },
    /// Copy results to a revision from records on the same target with the
    /// same input digest.
    Promote {
        /// The revision: `@`, a change for its head, or a revision.
        #[arg(default_value = "@")]
        rev: String,
    },
    /// Drop the payloads that are past the retention set in `policy.toml`,
    /// pack small objects, and remove what crashed writes left behind.
    Gc,
    /// Land a change on a target, along with the changes below it that the
    /// target doesn't have yet. The queue rebases them onto the target's tip
    /// and checks for the evidence `policy.toml` requires. If the evidence is
    /// there, it appends a signed acceptance. A landing that conflicts or lacks
    /// evidence waits in the queue.
    Land(LandArgs),
    /// Manage targets.
    #[command(subcommand)]
    Target(TargetCommand),
    /// List the targets with their tips, authorities and requirements.
    Targets,
    /// Manage workspaces: working copies that share this repository.
    #[command(subcommand)]
    Workspace(WorkspacesCommand),
    /// Show the op log: the operations this node ran, in every workspace.
    #[command(subcommand)]
    Op(OpCommand),
    /// Move this workspace's working copy back to where it was before its
    /// latest operation. Records and targets are not changed.
    Undo,
    /// Show the remote, or set it.
    Remote {
        /// The new remote: `http://host[:port]/<repository>`, or a path.
        url: Option<String>,
    },
    /// Fetch the remote's new acceptances on the followed targets, and the
    /// records of the changes this repository knows. Changes no files.
    Fetch {
        /// Also follow this target.
        #[arg(long)]
        target: Vec<TargetName>,
        /// Also fetch this change, given by its full id.
        #[arg(long)]
        change: Vec<ChangeId>,
    },
    /// Fetch from the remote, if there is one, then move the working copy to
    /// where its change went, or onto its target's new tip, keeping the edits
    /// made since.
    Pull,
    /// Show who the remote thinks you are.
    Whoami,
    /// Show what the remote lets you do: the paths you read and write, the
    /// statements you issue, and the targets you land on.
    Permissions,
    /// Send changes to the remote, with the changes below them: their
    /// revisions, files and statements. Lands nothing.
    Push {
        /// The changes to push: the working copy's by default.
        changes: Vec<String>,
    },
    /// Print events as they happen, one per line. The first line is the cursor
    /// to resume from. Watches the remote's events if the repository has a
    /// remote. Runs until interrupted.
    Watch {
        /// Only show events with this name, such as `check.requested`, or events
        /// about statements of this kind, such as `core.conflict` or `ci.*`.
        #[arg(long)]
        kind: Vec<KindPattern>,
        /// Only show events about this subject: a revision or change id prefix, or
        /// a target name.
        #[arg(long)]
        subject: Vec<String>,
        /// Start after this cursor instead of now. `0` starts from the oldest
        /// event kept.
        #[arg(long)]
        since: Option<u64>,
        /// Watch this node's events instead of the remote's.
        #[arg(long)]
        local: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OpCommand {
    /// List the latest operations, newest first.
    Log {
        /// Show at most this many operations.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
}

#[derive(Debug, Args)]
struct LandArgs {
    /// The change to land: the working copy's by default.
    #[arg(conflicts_with_all = ["from", "status"])]
    change: Option<String>,
    /// The target to land on: by default, the one the change is on.
    #[arg(long)]
    onto: Option<TargetName>,
    /// Land the tip of this target instead of a change.
    #[arg(long, requires = "onto")]
    from: Option<TargetName>,
    /// List the landings that are waiting, and why each waits.
    #[arg(long, conflicts_with_all = ["onto", "from"])]
    status: bool,
}

#[derive(Debug, Subcommand)]
enum TargetCommand {
    /// Create a target at the tip of another.
    New {
        /// Its name, such as `release/v3`.
        name: TargetName,
        /// The target whose tip the new target starts at.
        #[arg(long)]
        from: TargetName,
    },
}

#[derive(Debug, Args)]
struct AnnotateArgs {
    /// The revision (`@`, a change for its head, or a revision), or with
    /// `--on`, the kind.
    #[arg(value_name = "REV|KIND")]
    first: String,
    /// The kind of statement, such as `ci.test` or `review`, when the first
    /// argument is a revision.
    #[arg(value_name = "KIND")]
    kind: Option<String>,
    /// The statement to annotate instead of a revision, by a unique prefix
    /// of its id. A statement about a statement is never promoted, so it
    /// takes no inputs.
    #[arg(
        long,
        value_name = "STATEMENT",
        conflicts_with_all = ["inputs", "definition", "toolchain", "external"]
    )]
    on: Option<String>,
    #[command(flatten)]
    verdict: VerdictArgs,
    /// Take back your current statement of this kind about the subject
    /// instead of giving a verdict. The details may say why.
    #[arg(
        long,
        group = "VerdictArgs",
        conflicts_with_all = ["inputs", "definition", "toolchain", "external"]
    )]
    withdraw: bool,
    /// The files and directories the claim depended on. The claim is promoted
    /// to revisions on the same target where they are the same.
    #[arg(long, num_args = 1..)]
    inputs: Vec<RepoPath>,
    /// The check's command and configuration, as text or as its hex digest.
    #[arg(long, requires = "inputs")]
    definition: Option<DigestArg>,
    /// The tools that ran the check, such as the output of `rustc -V`, as
    /// text or as its hex digest.
    #[arg(long, requires = "inputs")]
    toolchain: Option<DigestArg>,
    /// The result also depended on things outside the inputs, such as the
    /// network, so it is never promoted.
    #[arg(long, requires = "inputs")]
    external: bool,
    #[command(flatten)]
    payload: PayloadArgs,
}

/// The statement's details, from at most one source.
#[derive(Debug, Args)]
#[group(multiple = false)]
struct PayloadArgs {
    /// Details as text, such as a review's message.
    #[arg(short, long)]
    message: Option<String>,
    /// A file holding the details, such as a log, or `-` to read them from
    /// standard input.
    #[arg(long)]
    payload: Option<PathBuf>,
}

impl PayloadArgs {
    fn payload(&self) -> Option<Payload> {
        let message = self.message.clone().map(Payload::Text);
        message.or_else(|| self.payload.clone().map(Payload::File))
    }
}

/// The statement's verdict: exactly one is required.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct VerdictArgs {
    /// The check passed, or you approve.
    #[arg(long)]
    pass: bool,
    /// The check failed, or you object.
    #[arg(long)]
    fail: bool,
    /// Neither: the statement is informational.
    #[arg(long)]
    neutral: bool,
}

impl VerdictArgs {
    fn verdict(&self) -> Verdict {
        match (self.pass, self.fail) {
            (true, _) => Verdict::Pass,
            (_, true) => Verdict::Fail,
            _ => Verdict::Neutral,
        }
    }
}

impl AnnotateArgs {
    /// Returns what the statement is about and its kind. Without `--on`, the
    /// arguments are the revision, then the kind; with `--on`, just the kind.
    ///
    /// # Errors
    ///
    /// Returns a clap error if the arguments match neither form, or the kind
    /// is invalid.
    fn subject(&self) -> Result<(Annotated<'_>, Kind), clap::Error> {
        let (annotated, kind) = match (&self.on, &self.kind) {
            (Some(on), None) => (Annotated::Statement(on), &self.first),
            (None, Some(kind)) => (Annotated::Revision(&self.first), kind),
            (Some(_), Some(_)) => {
                let message = "with --on, give the kind alone, without a revision";
                return Err(clap::Error::raw(ErrorKind::TooManyValues, message));
            }
            (None, None) => {
                let message = "give the revision, then the kind, or --on and the kind";
                return Err(clap::Error::raw(
                    ErrorKind::MissingRequiredArgument,
                    message,
                ));
            }
        };
        let kind = kind.parse().map_err(|error| {
            let message = format!("invalid kind `{kind}`: {error}");
            clap::Error::raw(ErrorKind::InvalidValue, message)
        })?;
        Ok((annotated, kind))
    }

    /// The verdict, or `None` for `--withdraw`.
    fn verdict(&self) -> Option<Verdict> {
        (!self.withdraw).then(|| self.verdict.verdict())
    }

    /// The declared inputs, or `None` without `--inputs`. A missing
    /// definition or toolchain is the digest of empty input.
    fn inputs(&self) -> Option<Inputs> {
        let digest = |arg: &Option<DigestArg>| arg.map_or_else(|| Digest::of(b""), |arg| arg.0);
        (!self.inputs.is_empty()).then(|| Inputs {
            paths: self.inputs.clone(),
            target_paths: Vec::new(),
            definition: digest(&self.definition),
            toolchain: digest(&self.toolchain),
            external: self.external,
        })
    }
}

/// A digest argument: valid hex is parsed as a digest, and any other
/// text is hashed.
#[derive(Clone, Copy, Debug)]
struct DigestArg(Digest);

impl FromStr for DigestArg {
    type Err = Infallible;

    fn from_str(arg: &str) -> Result<Self, Self::Err> {
        Ok(Self(
            arg.parse().unwrap_or_else(|_| Digest::of(arg.as_bytes())),
        ))
    }
}

#[derive(Debug, Subcommand)]
enum IndexCommand {
    /// Rebuild the index from the store.
    Rebuild,
}

#[derive(Debug, Subcommand)]
enum WorkspacesCommand {
    /// Create a workspace, with a new change on the same base as the working
    /// copy's change.
    New {
        /// Check out this change's head instead of starting a new change.
        #[arg(long)]
        change: Option<String>,
        /// Where to create the workspace: by default, beside the main working
        /// copy, named after the change.
        path: Option<PathBuf>,
    },
    /// List the workspaces and their changes.
    List,
    /// Remove a workspace, keeping its work as a revision of its change.
    Rm {
        /// The workspace's path.
        path: PathBuf,
    },
}

impl Cli {
    /// Parses the command line, then checks the `annotate` arguments, which
    /// clap can't check because their meaning depends on `--on`. On an
    /// error, prints it with the usage and exits, as clap does.
    ///
    /// # Panics
    ///
    /// Never: `annotate` is always a subcommand.
    #[must_use]
    #[expect(clippy::expect_used, reason = "`annotate` is a subcommand.")]
    pub fn parse_checked() -> Self {
        let cli = Self::parse();
        if let Command::Workspace(WorkspaceCommand::Annotate(args)) = &cli.command
            && let Err(error) = args.subject()
        {
            let mut command = Self::command();
            let annotate = command
                .find_subcommand_mut("annotate")
                .expect("`annotate` is a subcommand");
            error.format(annotate).exit();
        }
        cli
    }

    /// Runs the command, printing its output.
    ///
    /// # Errors
    ///
    /// Fails if the configuration cannot be loaded or the command fails. The
    /// error is meant for the user.
    pub fn run(self) -> Result<()> {
        match self.command {
            Command::Serve { config } => return ServerConfig::load(&config)?.serve(),
            Command::Repo(RepoCommand::Create {
                path,
                config,
                admin,
            }) => {
                return ServerConfig::load(&config)?.create_repo(&path, admin.as_ref());
            }
            Command::Token(TokenCommand::Issue {
                scope,
                ttl,
                config: Some(config),
                principal: Some(principal),
            }) => {
                println!(
                    "{}",
                    ServerConfig::load(&config)?.issue_token(principal, scope, ttl.duration())?
                );
                return Ok(());
            }
            Command::Init { .. }
            | Command::Clone { .. }
            | Command::Token(_)
            | Command::Workspace(_)
            | Command::Plugin(_) => {}
        }
        let config = UserConfig::load()?;
        let args: Vec<_> = env::args_os()
            .skip(1)
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let actor = Actor::new(args.join(" "));
        match self.command {
            Command::Init { path } => {
                Workspace::init(&path, config, &actor)?;
            }
            Command::Clone { url, path, target } => {
                Workspace::clone(&url, path.as_deref(), &target, config, &actor)?;
            }
            Command::Workspace(command) => command.run(config, &actor)?,
            Command::Plugin(args) => {
                let cwd = env::current_dir().context("cannot read the current directory")?;
                let context = match Workspace::discover(&cwd, config, &actor) {
                    Ok(mut workspace) => Some(workspace.plugin_context()?),
                    Err(WorkspaceError::NotFound(_)) => None,
                    Err(error) => return Err(error.into()),
                };
                match Plugin(args).exec(context.as_ref())? {}
            }
            Command::Token(TokenCommand::Issue { scope, ttl, .. }) => {
                let cwd = env::current_dir().context("cannot read the current directory")?;
                Workspace::discover(&cwd, config, &actor)?.issue_token(scope, ttl)?;
            }
            Command::Serve { .. } | Command::Repo(_) => unreachable!("run above"),
        }
        Ok(())
    }
}

impl WorkspaceCommand {
    fn run(self, config: UserConfig, actor: &Actor) -> Result<()> {
        let cwd = env::current_dir().context("cannot read the current directory")?;
        let mut workspace = Workspace::discover(&cwd, config, actor)?;
        match self {
            Self::Status => workspace.status()?,
            Self::Diff => workspace.diff()?,
            Self::Show { change } => workspace.show(change.as_deref())?,
            Self::New { description, on } => match on {
                Some(target) => workspace.new_on(&target, &description)?,
                None => workspace.new_change(&description)?,
            },
            Self::Describe { description } => workspace.describe(&description)?,
            Self::Resolve { paths } => workspace.resolve(&paths)?,
            Self::Edit { change } => workspace.edit(&change)?,
            Self::Abandon { change, reason } => workspace.abandon(change.as_deref(), &reason)?,
            Self::Converge { change } => workspace.converge(&change)?,
            Self::Log { target, limit } => workspace.log(target, limit)?,
            Self::Evolog { change } => workspace.evolog(&change)?,
            Self::Index(IndexCommand::Rebuild) => workspace.rebuild_index()?,
            Self::Annotate(args) => {
                let (annotated, kind) = args.subject()?;
                workspace.annotate(
                    annotated,
                    kind,
                    args.verdict(),
                    args.inputs(),
                    args.payload.payload().as_ref(),
                )?;
            }
            Self::Statements { rev } => workspace.statements(&rev)?,
            Self::Promote { rev } => workspace.promote(&rev)?,
            Self::Gc => workspace.gc()?,
            Self::Land(args) => match (args.from, args.onto) {
                _ if args.status => workspace.land_status()?,
                (Some(from), Some(onto)) => workspace.land_target(&from, &onto)?,
                (_, onto) => workspace.land(args.change.as_deref(), onto)?,
            },
            Self::Target(TargetCommand::New { name, from }) => {
                workspace.target_new(&name, &from)?;
            }
            Self::Targets => workspace.targets()?,
            Self::Workspace(WorkspacesCommand::New { change, path }) => {
                workspace.add_workspace(change.as_deref(), path.as_deref())?;
            }
            Self::Workspace(WorkspacesCommand::List) => workspace.list_workspaces()?,
            Self::Workspace(WorkspacesCommand::Rm { path }) => workspace.remove_workspace(&path)?,
            Self::Op(OpCommand::Log { limit }) => workspace.op_log(limit)?,
            Self::Undo => workspace.undo()?,
            Self::Remote { url } => workspace.set_remote(url.as_deref())?,
            Self::Fetch { target, change } => workspace.fetch(&target, &change)?,
            Self::Pull => workspace.pull()?,
            Self::Whoami => workspace.whoami()?,
            Self::Permissions => workspace.permissions()?,
            Self::Push { changes } => workspace.push(&changes)?,
            Self::Watch {
                kind,
                subject,
                since,
                local,
            } => workspace.watch(kind, subject, since, local)?,
        }
        Ok(())
    }
}
