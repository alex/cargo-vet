use std::collections::HashMap;
use std::ffi::OsString;
use std::mem;
use std::ops::Deref;
use std::panic::panic_any;
use std::path::Path;
use std::process::Command;
use std::{fs::File, io::Write, panic, path::PathBuf};

use cargo_metadata::{Metadata, Package};
use clap::{CommandFactory, Parser};
use console::{style, Term};
use eyre::{eyre, WrapErr};
use serde::de::Deserialize;
use tracing::{error, info, trace, warn};

use crate::cli::*;
use crate::format::{
    AuditEntry, AuditKind, AuditsFile, ConfigFile, CriteriaEntry, CriteriaStr, Delta,
    DependencyCriteria, DiffStat, FetchCommand, ImportsFile, MetaConfig, MetaConfigInstance,
    PackageStr, SortedMap, StoreInfo, UnauditedDependency,
};
use crate::resolver::{Conclusion, CriteriaMapper, DepGraph, SuggestItem};
use crate::storage::{Cache, Store};

mod cli;
mod flock;
pub mod format;
pub mod resolver;
mod serialization;
pub mod storage;
#[cfg(test)]
mod tests;

pub type VetError = eyre::Report;

/// Absolutely All The Global Configurations
pub struct Config {
    /// Cargo.toml `metadata.vet`
    metacfg: MetaConfig,
    /// `cargo metadata`
    metadata: Metadata,
    /// Freestanding configuration values
    _rest: PartialConfig,
}

/// Configuration vars that are available in a free-standing situation
/// (no actual cargo-vet instance to load/query).
pub struct PartialConfig {
    /// Details of the CLI invocation (args)
    cli: Cli,
    /// Path to the `cargo` binary that invoked us
    cargo: OsString,
    /// Path to the cargo's home, whose registry/cache, we opportunistically use for inspect/diff
    cargo_home: Option<PathBuf>,
    /// Path to the global tmp we're using
    tmp: PathBuf,
}

// Makes it a bit easier to have both a "partial" and "full" config
impl Deref for Config {
    type Target = PartialConfig;
    fn deref(&self) -> &Self::Target {
        &self._rest
    }
}

pub trait PackageExt {
    fn is_third_party(&self) -> bool;
}

impl PackageExt for Package {
    fn is_third_party(&self) -> bool {
        self.source
            .as_ref()
            .map(|s| s.is_crates_io())
            .unwrap_or(false)
    }
}

static TEMP_DIR_SUFFIX: &str = "cargo-vet-checkout";
static CARGO_ENV: &str = "CARGO";
// package.metadata.vet
static PACKAGE_VET_CONFIG: &str = "vet";
// workspace.metadata.vet
static WORKSPACE_VET_CONFIG: &str = "vet";

/// Trick to let us std::process::exit while still cleaning up
/// by panicking with this type instead of a string.
struct ExitPanic(i32);

/// Similar to the above, but allows us to exec a new command
/// as our final act.
struct ExecPanic(std::process::Command);

fn main() -> Result<(), VetError> {
    // Wrap main up in a catch_panic so that we can use it to implement std::process::exit with
    // unwinding, allowing us to silently exit the program while still cleaning up.
    let result = std::panic::catch_unwind(real_main);
    match result {
        Ok(main_result) => main_result,
        Err(mut e) => {
            if let Some(ExitPanic(code)) = e.downcast_ref::<ExitPanic>() {
                // Exit panic, just silently exit with this status
                std::process::exit(*code);
            } else if let Some(ExecPanic(_command)) = e.downcast_mut::<ExecPanic>() {
                // Exit with an exec.
                #[cfg(target_family = "unix")]
                {
                    use std::os::unix::process::CommandExt;
                    _command.exec();
                }
                unreachable!("we only use ExecPanic for unix");
            } else {
                // Normal panic, let it ride
                std::panic::resume_unwind(e);
            }
        }
    }
}

fn real_main() -> Result<(), VetError> {
    use cli::Commands::*;

    let fake_cli = cli::FakeCli::parse();
    let cli::FakeCli::Vet(cli) = fake_cli;

    //////////////////////////////////////////////////////
    // Setup logging / output
    //////////////////////////////////////////////////////

    // Init the logger (and make trace logging less noisy)
    if let Some(log_path) = &cli.log_file {
        let log_file = File::create(log_path).unwrap();
        tracing_subscriber::fmt::fmt()
            .with_max_level(cli.verbose)
            .with_target(false)
            .without_time()
            .with_ansi(false)
            .with_writer(log_file)
            .init();
    } else {
        tracing_subscriber::fmt::fmt()
            .with_max_level(cli.verbose)
            .with_target(false)
            .without_time()
            .with_writer(std::io::stderr)
            .init();
    }

    // Set a panic hook to redirect to the logger
    panic::set_hook(Box::new(|panic_info| {
        if panic_info.payload().is::<ExitPanic>() || panic_info.payload().is::<ExecPanic>() {
            // Be silent, we're just trying to std::process::exit
            return;
        }
        let (filename, line) = panic_info
            .location()
            .map(|loc| (loc.file(), loc.line()))
            .unwrap_or(("<unknown>", 0));

        let cause = panic_info
            .payload()
            .downcast_ref::<String>()
            .map(String::deref)
            .unwrap_or_else(|| {
                panic_info
                    .payload()
                    .downcast_ref::<&str>()
                    .copied()
                    .unwrap_or("<cause unknown>")
            });
        error!(
            "Panic - A panic occurred at {}:{}: {}",
            filename, line, cause
        );
    }));

    // Setup our output stream
    let mut stdout;
    let mut output_f;
    let out: &mut dyn Write = if let Some(output_path) = &cli.output_file {
        output_f = File::create(output_path).unwrap();
        &mut output_f
    } else {
        stdout = std::io::stdout();
        &mut stdout
    };

    ////////////////////////////////////////////////////
    // Potentially handle freestanding commands
    ////////////////////////////////////////////////////

    // TODO: make this configurable
    let cargo = std::env::var_os(CARGO_ENV).expect("Cargo failed to set $CARGO, how?");
    let tmp = std::env::temp_dir().join(TEMP_DIR_SUFFIX);
    let cargo_home = home::cargo_home().ok();
    let partial_cfg = PartialConfig {
        cli,
        cargo,
        tmp,
        cargo_home,
    };

    match &partial_cfg.cli.command {
        Some(Inspect(sub_args)) => return cmd_inspect(out, &partial_cfg, sub_args),
        Some(Diff(sub_args)) => return cmd_diff(out, &partial_cfg, sub_args),
        Some(HelpMarkdown(sub_args)) => return cmd_help_md(out, &partial_cfg, sub_args),
        _ => {
            // Not a freestanding command, time to do full parsing and setup
        }
    }

    ///////////////////////////////////////////////////
    // Fetch cargo metadata
    ///////////////////////////////////////////////////

    let cli = &partial_cfg.cli;
    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.cargo_path(&partial_cfg.cargo);
    if let Some(manifest_path) = &cli.manifest.manifest_path {
        cmd.manifest_path(manifest_path);
    }
    if !cli.features.no_all_features {
        cmd.features(cargo_metadata::CargoOpt::AllFeatures);
    }
    if cli.features.no_default_features {
        cmd.features(cargo_metadata::CargoOpt::NoDefaultFeatures);
    }
    if !cli.features.features.is_empty() {
        cmd.features(cargo_metadata::CargoOpt::SomeFeatures(
            cli.features.features.clone(),
        ));
    }
    let mut other_options = Vec::new();
    if cli.workspace.all || cli.workspace.workspace {
        other_options.push("--workspace".to_string());
    }
    for package in &cli.workspace.package {
        other_options.push("--package".to_string());
        other_options.push(package.to_string());
    }
    for package in &cli.workspace.exclude {
        other_options.push("--exclude".to_string());
        other_options.push(package.to_string());
    }
    // We never want cargo-vet to update the Cargo.lock.
    // For frozen runs we also don't want to touch the network.
    if cli.frozen {
        other_options.push("--frozen".to_string());
    } else {
        other_options.push("--locked".to_string());
    }
    cmd.other_options(other_options);

    info!("Running: {:#?}", cmd.cargo_command());

    // ERRORS: immediate fatal diagnostic
    let metadata = cmd
        .exec()
        .wrap_err("'cargo metadata' exited unsuccessfully")?;

    // trace!("Got Metadata! {:#?}", metadata);
    trace!("Got Metadata!");

    //////////////////////////////////////////////////////
    // Parse out our own configuration
    //////////////////////////////////////////////////////

    let default_config = MetaConfigInstance {
        version: Some(1),
        store: Some(StoreInfo {
            path: Some(
                metadata
                    .workspace_root
                    .join(storage::DEFAULT_STORE)
                    .into_std_path_buf(),
            ),
        }),
    };

    // FIXME: what is `store.path` relative to here?
    let workspace_metacfg = metadata
        .workspace_metadata
        .get(WORKSPACE_VET_CONFIG)
        .map(|cfg| {
            // ERRORS: immediate fatal diagnostic
            MetaConfigInstance::deserialize(cfg)
                .wrap_err("Workspace had [{WORKSPACE_VET_CONFIG}] but it was malformed")
        })
        .transpose()?;

    // FIXME: what is `store.path` relative to here?
    let package_metacfg = metadata
        .root_package()
        .and_then(|r| r.metadata.get(PACKAGE_VET_CONFIG))
        .map(|cfg| {
            // ERRORS: immediate fatal diagnostic
            MetaConfigInstance::deserialize(cfg)
                .wrap_err("Root package had [{PACKAGE_VET_CONFIG}] but it was malformed")
        })
        .transpose()?;

    if workspace_metacfg.is_some() && package_metacfg.is_some() {
        // ERRORS: immediate fatal diagnostic
        return Err(eyre!("Both a workspace and a package defined [metadata.vet]! We don't know what that means, if you do, let us know!"));
    }

    let mut metacfgs = vec![default_config];
    if let Some(metacfg) = workspace_metacfg {
        metacfgs.push(metacfg);
    }
    if let Some(metacfg) = package_metacfg {
        metacfgs.push(metacfg);
    }
    let metacfg = MetaConfig(metacfgs);

    info!("Final Metadata Config: ");
    info!("  - version: {}", metacfg.version());
    info!("  - store.path: {:#?}", metacfg.store_path());

    //////////////////////////////////////////////////////
    // Run the actual command
    //////////////////////////////////////////////////////

    let init = Store::is_init(&metacfg);
    if matches!(cli.command, Some(Commands::Init { .. })) {
        if init {
            // ERRORS: immediate fatal diagnostic
            return Err(eyre!(
                "'cargo vet' already initialized (store found at {})",
                metacfg.store_path().display()
            ));
        }
    } else if !init {
        // ERRORS: immediate fatal diagnostic
        return Err(eyre!(
            "You must run 'cargo vet init' (store not found at {})",
            metacfg.store_path().display()
        ));
    }

    let cfg = Config {
        metacfg,
        metadata,
        _rest: partial_cfg,
    };

    match &cfg.cli.command {
        None => cmd_vet(out, &cfg),
        Some(Init(sub_args)) => cmd_init(out, &cfg, sub_args),
        Some(AcceptCriteriaChange(sub_args)) => cmd_accept_criteria_change(out, &cfg, sub_args),
        Some(Certify(sub_args)) => cmd_certify(out, &cfg, sub_args),
        Some(AddUnaudited(sub_args)) => cmd_add_unaudited(out, &cfg, sub_args),
        Some(RecordViolation(sub_args)) => cmd_record_violation(out, &cfg, sub_args),
        Some(Suggest(sub_args)) => cmd_suggest(out, &cfg, sub_args),
        Some(Fmt(sub_args)) => cmd_fmt(out, &cfg, sub_args),
        Some(FetchImports(sub_args)) => cmd_fetch_imports(out, &cfg, sub_args),
        Some(RegenerateUnaudited(sub_args)) => cmd_regenerate_unaudited(out, &cfg, sub_args),
        Some(DumpGraph(sub_args)) => cmd_dump_graph(out, &cfg, sub_args),
        // Need to be non-exhaustive because freestanding commands were handled earlier
        _ => unreachable!("did you add a new command and forget to implement it?"),
    }
}

fn cmd_init(_out: &mut dyn Write, cfg: &Config, _sub_args: &InitArgs) -> Result<(), VetError> {
    // Initialize vet
    trace!("initializing...");

    let mut store = Store::create(cfg)?;

    let (config, audits, imports) = init_files(&cfg.metadata, cfg.cli.filter_graph.as_ref())?;
    store.config = config;
    store.audits = audits;
    store.imports = imports;

    store.commit()?;

    Ok(())
}

pub fn init_files(
    metadata: &Metadata,
    filter_graph: Option<&Vec<GraphFilter>>,
) -> Result<(ConfigFile, AuditsFile, ImportsFile), VetError> {
    // Default audits file is empty
    let audits = AuditsFile {
        criteria: SortedMap::new(),
        audits: SortedMap::new(),
    };

    // Default imports file is empty
    let imports = ImportsFile {
        audits: SortedMap::new(),
    };

    // This is the hard one
    let config = {
        let mut dependencies = SortedMap::new();
        let graph = DepGraph::new(metadata, filter_graph);
        for package in &graph.nodes {
            if !package.is_third_party {
                // Only care about third-party packages
                continue;
            }
            let criteria = if package.is_dev_only {
                format::DEFAULT_POLICY_DEV_CRITERIA.to_string()
            } else {
                format::DEFAULT_POLICY_CRITERIA.to_string()
            };
            // NOTE: May have multiple copies of a package!
            let item = UnauditedDependency {
                version: package.version.clone(),
                criteria,
                dependency_criteria: DependencyCriteria::new(),
                notes: None,
                suggest: true,
            };
            dependencies
                .entry(package.name.to_string())
                .or_insert(vec![])
                .push(item);
        }
        ConfigFile {
            default_criteria: format::get_default_criteria(),
            imports: SortedMap::new(),
            unaudited: dependencies,
            policy: SortedMap::new(),
        }
    };

    Ok((config, audits, imports))
}

fn cmd_inspect(
    out: &mut dyn Write,
    cfg: &PartialConfig,
    sub_args: &InspectArgs,
) -> Result<(), VetError> {
    // Download a crate's source to a temp location for review
    let mut cache = Cache::acquire(cfg)?;
    // Record this command for magic in `vet certify`
    cache.command_history.last_fetch = Some(FetchCommand::Inspect {
        package: sub_args.package.clone(),
        version: sub_args.version.clone(),
    });

    let package = &*sub_args.package;

    let to_fetch = &[(package, &sub_args.version)];
    let fetched_paths = cache.fetch_packages(to_fetch)?;
    let fetched = &fetched_paths[package][&sub_args.version];

    #[cfg(target_family = "unix")]
    {
        // Loosely borrowed from cargo crev.
        let shell = std::env::var_os("SHELL").unwrap();
        writeln!(out, "Opening nested shell in: {:#?}", fetched)?;
        writeln!(out, "Use `exit` or Ctrl-D to finish.",)?;
        let mut command = std::process::Command::new(shell);
        command.current_dir(fetched.clone()).env("PWD", fetched);
        panic_any(ExecPanic(command));
    }

    #[cfg(not(target_family = "unix"))]
    {
        writeln!(out, "  fetched to {:#?}", fetched)?;
        Ok(())
    }
}

fn cmd_certify(out: &mut dyn Write, cfg: &Config, sub_args: &CertifyArgs) -> Result<(), VetError> {
    // Certify that you have reviewed a crate's source for some version / delta
    let mut store = Store::acquire(cfg)?;
    // Grab the command history and immediately drop the cache
    let command_history = Cache::acquire(cfg)?.command_history.clone();

    let term = Term::stdout();

    // Before setting up magic, we need to agree on a package
    let last_fetch_name = command_history.last_fetch.as_ref().map(|f| f.package());
    let package = if let Some(package) = &sub_args.package {
        package.clone()
    } else if let Some(package) = last_fetch_name {
        // If we just fetched a package, assume we want to certify it
        package.to_owned()
    } else {
        // ERRORS: immediate fatal diagnostic
        writeln!(
            out,
            "error: couldn't guess what package to certify, please specify"
        )?;
        panic_any(ExitPanic(-1));
    };

    // FIXME: can/should we check if the version makes sense..?
    if !foreign_packages(&cfg.metadata).any(|pkg| pkg.name == *package) {
        // ERRORS: immediate fatal diagnostic? should we allow you to certify random packages?
        // You're definitely *allowed* to have unused audits, otherwise you'd be constantly deleting
        // useful audits whenever you update your dependencies! But this might be a useful guard
        // against typosquatting or other weird issues?
        writeln!(
            out,
            "error: '{}' isn't one of your foreign packages",
            package
        )?;
        panic_any(ExitPanic(-1));
    }

    // If the package name now matches last_fetch, make it available for further magic
    let last_fetch = if last_fetch_name == Some(&package) {
        command_history.last_fetch.as_ref()
    } else {
        None
    };

    let dependency_criteria = if sub_args.dependency_criteria.is_empty() {
        // TODO: look at the current audits to infer this? prompt?
        DependencyCriteria::new()
    } else {
        let mut dep_criteria = DependencyCriteria::new();
        for arg in &sub_args.dependency_criteria {
            dep_criteria
                .entry(arg.dependency.clone())
                .or_insert_with(Vec::new)
                .push(arg.criteria.clone());
        }
        dep_criteria
    };

    let mut criteria_guess = None;
    let kind = if let Some(v1) = &sub_args.version1 {
        // If explicit versions were provided, use those
        if let Some(v2) = &sub_args.version2 {
            // This is a delta audit
            AuditKind::Delta {
                delta: Delta {
                    from: v1.clone(),
                    to: v2.clone(),
                },
                dependency_criteria,
            }
        } else {
            // This is a full audit
            AuditKind::Full {
                version: v1.clone(),
                dependency_criteria,
            }
        }
    } else if let Some(fetch) = last_fetch {
        // Otherwise, is we just fetched this package, use the version(s) we fetched
        match fetch {
            FetchCommand::Inspect { version, .. } => AuditKind::Full {
                version: version.clone(),
                dependency_criteria,
            },
            FetchCommand::Diff {
                version1, version2, ..
            } => AuditKind::Delta {
                delta: Delta {
                    from: version1.clone(),
                    to: version2.clone(),
                },
                dependency_criteria,
            },
        }
    } else if let Some(unaudited_list) = store.config.unaudited.get(&package) {
        // Otherwise, if we have an unaudited entry for this package, use that version
        if unaudited_list.len() > 1 {
            // ERRORS: immediate fatal diagnostic
            writeln!(
                out,
                "error: couldn't guess what version to certify, you have multiple 'unaudited' entries for {}:",
                package
            )?;
            for entry in unaudited_list {
                writeln!(out, "  {}", entry.version)?;
            }
            panic_any(ExitPanic(-1));
        }
        let entry = &unaudited_list[0];
        criteria_guess = Some(vec![entry.criteria.clone()]);
        // FIXME: this should arguably use entry.dependency_criteria unless the cli specified,
        // should probably have a more coherent "strategy picking" right at the start instead
        // of individually sourcing each piece of information
        AuditKind::Full {
            version: entry.version.clone(),
            dependency_criteria,
        }
    } else if !command_history.last_suggest.is_empty() {
        // Otherwise, if we suggested a fetch for this package, use that version
        let relevant_suggestions = command_history
            .last_suggest
            .iter()
            .filter(|s| s.command.package() == package)
            .collect::<Vec<_>>();
        if relevant_suggestions.is_empty() {
            // ERRORS: immediate fatal diagnostic
            writeln!(
                out,
                "error: couldn't guess what version to certify, please specify"
            )?;
            panic_any(ExitPanic(-1));
        }
        if relevant_suggestions.len() > 1 {
            // ERRORS: immediate fatal diagnostic
            writeln!(
                out,
                "error: couldn't guess what version to certify, you have multiple suggestions for {}:",
                package
            )?;
            for entry in relevant_suggestions {
                match &entry.command {
                    FetchCommand::Inspect { version, .. } => writeln!(out, "inspect {}", version)?,
                    FetchCommand::Diff {
                        version1, version2, ..
                    } => writeln!(out, "diff {} {}", version1, version2)?,
                }
            }
            panic_any(ExitPanic(-1));
        }
        criteria_guess = Some(relevant_suggestions[0].criteria.clone());
        match &relevant_suggestions[0].command {
            FetchCommand::Inspect { version, .. } => AuditKind::Full {
                version: version.clone(),
                dependency_criteria,
            },
            FetchCommand::Diff {
                version1, version2, ..
            } => AuditKind::Delta {
                delta: Delta {
                    from: version1.clone(),
                    to: version2.clone(),
                },
                dependency_criteria,
            },
        }
    } else {
        // ERRORS: immediate fatal diagnostic
        writeln!(
            out,
            "error: couldn't guess what version to certify, please specify"
        )?;
        panic_any(ExitPanic(-1));
    };

    let (username, who) = if let Some(who) = &sub_args.who {
        (who.clone(), Some(who.clone()))
    } else {
        let user_info = get_user_info()?;
        let who = format!("{} <{}>", user_info.username, user_info.email);
        (user_info.username, Some(who))
    };

    let criteria_mapper = CriteriaMapper::new(&store.audits.criteria);

    let criteria_names = if sub_args.criteria.is_empty() {
        // If we don't have explicit cli criteria, guess the criteria
        //
        // * If any previous operation resulted in a guess, use that
        // * Otherwise check for a suggest on this exact audit
        // * Otherwise guess nothing
        //
        // Regardless of the guess, prompt the user to confirm (just needs to mash enter)
        let mut chosen_criteria = criteria_guess
            .or_else(|| {
                command_history
                    .last_suggest
                    .into_iter()
                    .filter(|s| s.command.package() == package)
                    .find(|v| match (&kind, &v.command) {
                        (
                            AuditKind::Full { version: lhs, .. },
                            FetchCommand::Inspect { version: rhs, .. },
                        ) => lhs == rhs,
                        (
                            AuditKind::Delta { delta, .. },
                            FetchCommand::Diff {
                                version1, version2, ..
                            },
                        ) => &delta.from == version1 && &delta.to == version2,
                        _ => false,
                    })
                    .map(|s| s.criteria)
            })
            .unwrap_or_default();

        // Prompt for criteria
        loop {
            term.clear_screen()?;
            write!(out, "choose criteria to certify for {}", package)?;
            match &kind {
                AuditKind::Full { version, .. } => write!(out, ":{}", version)?,
                AuditKind::Delta { delta, .. } => write!(out, ":{} -> {}", delta.from, delta.to)?,
                AuditKind::Violation { .. } => unreachable!(),
            }
            writeln!(out)?;
            writeln!(out, "  0. <clear selections>")?;
            let implied_criteria = criteria_mapper.criteria_from_list(&chosen_criteria);
            for (criteria_idx, (criteria_name, _criteria_entry)) in
                criteria_mapper.list.iter().enumerate()
            {
                if chosen_criteria.contains(criteria_name) {
                    writeln!(
                        out,
                        "  {}. {}",
                        criteria_idx + 1,
                        style(criteria_name).green()
                    )?;
                } else if implied_criteria.has_criteria(criteria_idx) {
                    writeln!(
                        out,
                        "  {}. {}",
                        criteria_idx + 1,
                        style(criteria_name).yellow()
                    )?;
                } else {
                    writeln!(out, "  {}. {}", criteria_idx + 1, criteria_name)?;
                }
            }

            writeln!(out)?;
            writeln!(
                out,
                "current selection: {:?}",
                criteria_mapper
                    .criteria_names(&implied_criteria)
                    .collect::<Vec<_>>()
            )?;
            writeln!(out, "(press ENTER to accept the current criteria)")?;
            let input = term.read_line()?;
            let input = input.trim();
            if input.is_empty() {
                if chosen_criteria.is_empty() {
                    writeln!(out, "no criteria chosen, aborting")?;
                    panic_any(ExitPanic(-1));
                }
                // User done selecting criteria
                break;
            }

            // FIXME: these errors get cleared away right away
            let answer = if let Ok(val) = input.parse::<usize>() {
                val
            } else {
                // ERRORS: immediate error print to output for feedback, non-fatal
                writeln!(out, "error: not a valid integer")?;
                continue;
            };
            if answer == 0 {
                chosen_criteria.clear();
                continue;
            }
            if answer > criteria_mapper.list.len() {
                // ERRORS: immediate error print to output for feedback, non-fatal
                writeln!(out, "error: not a valid criteria")?;
                continue;
            }
            chosen_criteria.push(criteria_mapper.list[answer - 1].0.clone());
        }
        chosen_criteria
    } else {
        sub_args.criteria.clone()
    };

    // Round-trip this through the criteria_mapper to clean up `implies` relationships
    let criteria_set = criteria_mapper.criteria_from_list(&criteria_names);
    let criteria_names = criteria_mapper
        .criteria_names(&criteria_set)
        .collect::<Vec<_>>();

    let notes = if let Some(notes) = sub_args.notes.clone() {
        Some(notes)
    } else {
        term.clear_screen()?;
        write!(out, "certifying {}", package)?;
        match &kind {
            AuditKind::Full { version, .. } => write!(out, ":{}", version)?,
            AuditKind::Delta { delta, .. } => write!(out, ":{} -> {}", delta.from, delta.to)?,
            AuditKind::Violation { .. } => unreachable!(),
        }
        writeln!(out, " for {:?}", criteria_names)?;
        writeln!(out, "do you have any notes? (press ENTER to continue)")?;
        writeln!(out)?;
        // FIXME: this should take multiline
        // FIXME: we should linebreak long inputs
        let input = term.read_line()?;
        let input = input.trim();
        if input.is_empty() {
            None
        } else {
            Some(input.to_string())
        }
    };

    for criteria in criteria_names {
        if !sub_args.accept_all {
            let eula = if let Some(eula) = eula_for_criteria(&store.audits, criteria) {
                eula
            } else {
                // ERRORS: fatal diagnostic, unclear if should be immediate or gathered?
                // Some versions of this error can arguably be a validation error in `Store`
                // so perhaps this validation doesn't need to exist at all if properly designed
                // (would require a fallback for failing to fetch url, maybe just print the url)
                writeln!(out, "error: couldn't get description of criteria")?;
                panic_any(ExitPanic(-1));
            };

            term.clear_screen()?;
            // Print out the EULA and prompt
            let what_version = match &kind {
                AuditKind::Full { version, .. } => {
                    format!("version {}", version)
                }
                AuditKind::Delta { delta, .. } => {
                    format!("the changes from version {} to {}", delta.from, delta.to)
                }
                AuditKind::Violation { .. } => unreachable!(),
            };
            let statement = format!(
                "I, {}, certify that I have audited {} of {} in accordance with the following criteria:",
                username, what_version, package,
            );

            write!(
                out,
                "\n{}\n\n",
                style(textwrap::fill(&statement, 80)).yellow().bold()
            )?;
            writeln!(out, "{}\n", style(eula).cyan())?;
            write!(out, "(type \"yes\" to certify for {}): ", criteria)?;
            out.flush()?;

            let answer = term.read_line()?.trim().to_lowercase();
            if answer != "yes" {
                // ERRORS: immediate fatal diagnostic, although arguably less of an error and more of
                // a "fine, be that way" and exit.
                writeln!(out, "rejected certification")?;
                panic_any(ExitPanic(-1));
            }
        }

        // Ok! Ready to commit the audit!
        let new_entry = AuditEntry {
            kind: kind.clone(),
            criteria: criteria.to_string(),
            who: who.clone(),
            notes: notes.clone(),
        };

        store
            .audits
            .audits
            .entry(package.clone())
            .or_insert(vec![])
            .push(new_entry);

        // If we're submitting a full audit, look for a matching unaudited entry to remove
        if let AuditKind::Full { version, .. } = &kind {
            if let Some(unaudited_list) = store.config.unaudited.get_mut(&package) {
                let cur_criteria_set = criteria_mapper.criteria_from_list([criteria]);
                // Iterate backwards so that we can delete while iterating
                // (will only affect indices that we've already visited!)
                for idx in (0..unaudited_list.len()).rev() {
                    let entry = &unaudited_list[idx];
                    let entry_criteria_set = criteria_mapper.criteria_from_list([&entry.criteria]);
                    if &entry.version == version && cur_criteria_set.contains(&entry_criteria_set) {
                        unaudited_list.remove(idx);
                    }
                }
                if unaudited_list.is_empty() {
                    store.config.unaudited.remove(&package);
                }
            }
        }
    }

    store.commit()?;

    Ok(())
}

fn cmd_record_violation(
    _out: &mut dyn Write,
    cfg: &Config,
    sub_args: &RecordViolationArgs,
) -> Result<(), VetError> {
    // Mark a package as a violation
    let mut store = Store::acquire(cfg)?;

    let kind = AuditKind::Violation {
        violation: sub_args.versions.clone(),
    };

    let (_username, who) = if let Some(who) = &sub_args.who {
        (who.clone(), Some(who.clone()))
    } else {
        let user_info = get_user_info()?;
        let who = format!("{} <{}>", user_info.username, user_info.email);
        (user_info.username, Some(who))
    };

    let notes = sub_args.notes.clone();

    let mut criteria = if sub_args.criteria.is_empty() {
        // TODO: provide an interactive prompt for this
        vec![store.config.default_criteria.clone()]
    } else {
        sub_args.criteria.clone()
    };

    // TODO: implement multi-criteria
    if criteria.len() != 1 {
        unimplemented!("multiple criteria not yet implemented");
    }
    let criteria = criteria.swap_remove(0);

    // FIXME: can/should we check if the version makes sense..?
    if !foreign_packages(&cfg.metadata).any(|pkg| pkg.name == sub_args.package) {
        // ERRORS: immediate fatal diagnostic? should we allow you to forbid random packages?
        // You're definitely *allowed* to have unused audits, otherwise you'd be constantly deleting
        // useful audits whenever you update your dependencies! But this might be a useful guard
        // against typosquatting or other weird issues?
        return Err(eyre!(
            "'{}' isn't one of your foreign packages",
            sub_args.package
        ));
    }

    // Ok! Ready to commit the audit!
    let new_entry = AuditEntry {
        kind,
        criteria,
        who,
        notes,
    };

    store
        .audits
        .audits
        .entry(sub_args.package.clone())
        .or_insert(vec![])
        .push(new_entry);

    store.commit()?;

    Ok(())
}

fn cmd_add_unaudited(
    _out: &mut dyn Write,
    cfg: &Config,
    sub_args: &AddUnauditedArgs,
) -> Result<(), VetError> {
    // Add an unaudited entry
    let mut store = Store::acquire(cfg)?;

    let dependency_criteria = if sub_args.dependency_criteria.is_empty() {
        // TODO: look at the current audits to infer this? prompt?
        DependencyCriteria::new()
    } else {
        let mut dep_criteria = DependencyCriteria::new();
        for arg in &sub_args.dependency_criteria {
            dep_criteria
                .entry(arg.dependency.clone())
                .or_insert_with(Vec::new)
                .push(arg.criteria.clone());
        }
        dep_criteria
    };

    let notes = sub_args.notes.clone();

    let mut criteria = if sub_args.criteria.is_empty() {
        // TODO: provide an interactive prompt for this
        vec![store.config.default_criteria.clone()]
    } else {
        sub_args.criteria.clone()
    };

    let suggest = !sub_args.no_suggest;

    // TODO: implement multi-criteria
    if criteria.len() != 1 {
        unimplemented!("multiple criteria not yet implemented");
    }
    let criteria = criteria.swap_remove(0);

    // FIXME: can/should we check if the version makes sense..?
    if !foreign_packages(&cfg.metadata).any(|pkg| pkg.name == sub_args.package) {
        // ERRORS: immediate fatal diagnostic? should we allow you to certify random packages?
        // You're definitely *allowed* to have unused audits, otherwise you'd be constantly deleting
        // useful audits whenever you update your dependencies! But this might be a useful guard
        // against typosquatting or other weird issues?
        return Err(eyre!(
            "'{}' isn't one of your foreign packages",
            sub_args.package
        ));
    }

    // Ok! Ready to commit the audit!
    let new_entry = UnauditedDependency {
        criteria,
        notes,
        version: sub_args.version.clone(),
        dependency_criteria,
        suggest,
    };

    store
        .config
        .unaudited
        .entry(sub_args.package.clone())
        .or_insert(vec![])
        .push(new_entry);

    store.commit()?;

    Ok(())
}

fn cmd_suggest(out: &mut dyn Write, cfg: &Config, sub_args: &SuggestArgs) -> Result<(), VetError> {
    // Run the checker to validate that the current set of deps is covered by the current cargo vet store
    trace!("suggesting...");
    let mut store = Store::acquire(cfg)?;

    // Delete all unaudited entries except those that are suggest=false
    for versions in &mut store.config.unaudited.values_mut() {
        versions.retain(|e| !e.suggest);
    }

    // DO THE THING!!!!
    let report = resolver::resolve(
        &cfg.metadata,
        cfg.cli.filter_graph.as_ref(),
        &store,
        sub_args.guess_deeper,
    );
    match cfg.cli.output_format {
        OutputFormat::Human => report.print_suggest_human(out, cfg)?,
        OutputFormat::Json => report.print_json(out, cfg)?,
    }

    // Don't commit the store, because we purged the unaudited table above.

    Ok(())
}

fn cmd_regenerate_unaudited(
    _out: &mut dyn Write,
    cfg: &Config,
    _sub_args: &RegenerateUnauditedArgs,
) -> Result<(), VetError> {
    // Run the checker to validate that the current set of deps is covered by the current cargo vet store
    trace!("regenerating unaudited...");
    let mut store = Store::acquire(cfg)?;

    minimize_unaudited(cfg, &mut store)?;

    // We were successful, commit the store
    store.commit()?;

    Ok(())
}

pub fn minimize_unaudited(cfg: &Config, store: &mut Store) -> Result<(), VetError> {
    // Set the unaudited entries to nothing
    let old_unaudited = mem::take(&mut store.config.unaudited);

    // Try to vet
    let report = resolver::resolve(&cfg.metadata, cfg.cli.filter_graph.as_ref(), store, true);

    trace!("minimizing unaudited...");
    let new_unaudited = if let Some(suggest) = report.compute_suggest(cfg, false)? {
        let mut new_unaudited = SortedMap::new();
        let mut suggest_by_package_name = SortedMap::<PackageStr, Vec<SuggestItem>>::new();
        for item in suggest.suggestions {
            let package = &report.graph.nodes[item.package];
            suggest_by_package_name
                .entry(package.name)
                .or_default()
                .push(item);
        }

        // First try to preserve as many old entries as possible
        for (package_name, old_entries) in &old_unaudited {
            let mut no_suggestions = Vec::new();
            let suggestions = suggest_by_package_name
                .get_mut(&**package_name)
                .unwrap_or(&mut no_suggestions);
            for old_entry in old_entries {
                for item_idx in (0..suggestions.len()).rev() {
                    // If there's an existing entry for these criteria, preserve it
                    let new_item = &mut suggestions[item_idx];
                    {
                        let mut new_criteria = report
                            .criteria_mapper
                            .criteria_names(&new_item.suggested_criteria);
                        if new_item.suggested_diff.to == old_entry.version
                            && new_criteria.any(|s| s == &*old_entry.criteria)
                        {
                            std::mem::drop(new_criteria);
                            report.criteria_mapper.clear_criteria(
                                &mut new_item.suggested_criteria,
                                &old_entry.criteria,
                            );
                            new_unaudited
                                .entry(package_name.clone())
                                .or_insert(Vec::new())
                                .push(old_entry.clone());
                        }
                    }
                    // If we've exhausted all the criteria for this suggestion, remove it
                    if new_item.suggested_criteria.is_empty() {
                        suggestions.swap_remove(item_idx);
                    }
                }
                // If we haven't cleared out all the suggestions for this package, make sure its entry is inserted
                // to try to preserve the original order of it.
                if !suggestions.is_empty() {
                    new_unaudited
                        .entry(package_name.clone())
                        .or_insert(Vec::new());
                }
            }
        }

        // Now insert any remaining suggestions
        for (package_name, new_items) in suggest_by_package_name {
            for item in new_items {
                for criteria in report
                    .criteria_mapper
                    .criteria_names(&item.suggested_criteria)
                {
                    new_unaudited
                        .entry(package_name.to_string())
                        .or_insert(Vec::new())
                        .push(UnauditedDependency {
                            version: item.suggested_diff.to.clone(),
                            criteria: criteria.to_string(),
                            dependency_criteria: DependencyCriteria::new(),
                            notes: None,
                            suggest: true,
                        })
                }
            }
        }

        new_unaudited
    } else if let Conclusion::Success(_) = report.conclusion {
        SortedMap::new()
    } else {
        // ERRORS: immediate fatal diagnostic
        return Err(eyre::eyre!(
            "error: regenerate-unaudited failed for unknown reason"
        ));
    };

    // Alright there's the new unaudited
    store.config.unaudited = new_unaudited;

    Ok(())
}

fn cmd_diff(out: &mut dyn Write, cfg: &PartialConfig, sub_args: &DiffArgs) -> Result<(), VetError> {
    let mut cache = Cache::acquire(cfg)?;
    cache.command_history.last_fetch = Some(FetchCommand::Diff {
        package: sub_args.package.clone(),
        version1: sub_args.version1.clone(),
        version2: sub_args.version2.clone(),
    });

    let package = &*sub_args.package;

    writeln!(
        out,
        "fetching {} {} and {} ...",
        sub_args.package, sub_args.version1, sub_args.version2,
    )?;

    let to_fetch = &[(package, &sub_args.version1), (package, &sub_args.version2)];
    let fetched_paths = cache.fetch_packages(to_fetch)?;
    let fetched1 = &fetched_paths[package][&sub_args.version1];
    let fetched2 = &fetched_paths[package][&sub_args.version2];

    writeln!(out)?;

    diff_crate(out, cfg, fetched1, fetched2)?;

    Ok(())
}

fn cmd_vet(out: &mut dyn Write, cfg: &Config) -> Result<(), VetError> {
    // Run the checker to validate that the current set of deps is covered by the current cargo vet store
    trace!("vetting...");

    let mut store = Store::acquire(cfg)?;
    if !cfg.cli.locked {
        store.fetch_foreign_audits()?;
    }

    // DO THE THING!!!!
    let report = resolver::resolve(&cfg.metadata, cfg.cli.filter_graph.as_ref(), &store, false);
    match cfg.cli.output_format {
        OutputFormat::Human => report.print_human(out, cfg)?,
        OutputFormat::Json => report.print_json(out, cfg)?,
    }

    // Only save imports if we succeeded, to avoid any modifications on error.
    if report.has_errors() {
        // ERRORS: immediate fatal diagnostic? Arguably should be silent.
        // Err(eyre!("report contains errors"))?;
        panic_any(ExitPanic(-1));
    } else {
        store.commit()?;
    }

    Ok(())
}

fn cmd_fetch_imports(
    out: &mut dyn Write,
    cfg: &Config,
    _sub_args: &FetchImportsArgs,
) -> Result<(), VetError> {
    trace!("fetching imports...");

    let mut store = Store::acquire(cfg)?;
    if !cfg.cli.locked {
        store.fetch_foreign_audits()?;
    } else {
        // ERRORS: just a warning that you're holding it wrong, unclear if immediate or buffered,
        // or if this should be a hard error, or if we should ignore the --locked flag and
        // just do it anyway
        writeln!(
            out,
            "warning: ran fetch-imports with --locked, this won't fetch!"
        )?;
    }
    store.commit()?;

    Ok(())
}

fn cmd_dump_graph(
    out: &mut dyn Write,
    cfg: &Config,
    sub_args: &DumpGraphArgs,
) -> Result<(), VetError> {
    // Dump a mermaid-js graph
    trace!("dumping...");

    let graph = resolver::DepGraph::new(&cfg.metadata, cfg.cli.filter_graph.as_ref());
    match cfg.cli.output_format {
        OutputFormat::Human => graph.print_mermaid(out, sub_args)?,
        OutputFormat::Json => serde_json::to_writer_pretty(out, &graph.nodes)?,
    }

    Ok(())
}

fn cmd_fmt(_out: &mut dyn Write, cfg: &Config, _sub_args: &FmtArgs) -> Result<(), VetError> {
    // Reformat all the files (just load and store them, formatting is implict).
    trace!("formatting...");
    let store = Store::acquire(cfg)?;
    store.commit()?;
    Ok(())
}

fn cmd_accept_criteria_change(
    _out: &mut dyn Write,
    _cfg: &Config,
    _sub_args: &AcceptCriteriaChangeArgs,
) -> Result<(), VetError> {
    // Accept changes that a foreign audits.toml made to their criteria.
    trace!("accepting...");

    unimplemented!("TODO(#68): unimplemented feature!");
}

/// Perform crimes on clap long_help to generate markdown docs
fn cmd_help_md(
    out: &mut dyn Write,
    _cfg: &PartialConfig,
    _sub_args: &HelpMarkdownArgs,
) -> Result<(), VetError> {
    let app_name = "cargo-vet";
    let pretty_app_name = "cargo vet";
    // Make a new App to get the help message this time.

    writeln!(out, "# {pretty_app_name} CLI manual")?;
    writeln!(out)?;
    writeln!(
        out,
        "> This manual can be regenerated with `{pretty_app_name} help-markdown`"
    )?;
    writeln!(out)?;

    let mut fake_cli = FakeCli::command();
    let full_command = fake_cli.get_subcommands_mut().next().unwrap();
    full_command.build();
    let mut todo = vec![full_command];
    let mut is_full_command = true;

    while let Some(command) = todo.pop() {
        let mut help_buf = Vec::new();
        command.write_long_help(&mut help_buf).unwrap();
        let help = String::from_utf8(help_buf).unwrap();

        // First line is --version
        let mut lines = help.lines();
        let version_line = lines.next().unwrap();
        let subcommand_name = command.get_name();
        let pretty_subcommand_name;

        if is_full_command {
            pretty_subcommand_name = String::new();
            writeln!(out, "Version: `{version_line}`")?;
            writeln!(out)?;
        } else {
            pretty_subcommand_name = format!("{pretty_app_name} {subcommand_name} ");
            // Give subcommands some breathing room
            writeln!(out, "<br><br><br>")?;
            writeln!(out, "## {pretty_subcommand_name}")?;
        }

        let mut in_subcommands_listing = false;
        let mut in_usage = false;
        for line in lines {
            // Use a trailing colon to indicate a heading
            if let Some(heading) = line.strip_suffix(':') {
                if !line.starts_with(' ') {
                    // SCREAMING headers are Main headings
                    if heading.to_ascii_uppercase() == heading {
                        in_subcommands_listing = heading == "SUBCOMMANDS";
                        in_usage = heading == "USAGE";

                        writeln!(out, "### {pretty_subcommand_name}{heading}")?;
                    } else {
                        writeln!(out, "### {heading}")?;
                    }
                    continue;
                }
            }

            if in_subcommands_listing && !line.starts_with("     ") {
                // subcommand names are list items
                let own_subcommand_name = line.trim();
                write!(
                    out,
                    "* [{own_subcommand_name}](#{app_name}-{own_subcommand_name}): "
                )?;
                continue;
            }
            // The rest is indented, get rid of that
            let line = line.trim();

            // Usage strings get wrapped in full code blocks
            if in_usage && line.starts_with(pretty_app_name) {
                writeln!(out, "```")?;
                writeln!(out, "{line}")?;
                writeln!(out, "```")?;
                continue;
            }

            // argument names are subheadings
            if line.starts_with('-') || line.starts_with('<') {
                writeln!(out, "#### `{line}`")?;
                continue;
            }

            // escape default/value strings
            if line.starts_with('[') {
                writeln!(out, "\\{line}  ")?;
                continue;
            }

            // Normal paragraph text
            writeln!(out, "{line}")?;
        }
        writeln!(out)?;

        todo.extend(command.get_subcommands_mut());
        is_full_command = false;
    }

    Ok(())
}

// Utils

fn diff_crate(
    _out: &mut dyn Write,
    _cfg: &PartialConfig,
    version1: &Path,
    version2: &Path,
) -> Result<(), VetError> {
    // ERRORS: arguably this is all proper fallible, but it would be fatal to
    // `cargo vet diff`, the primary consumer, to not be able to diff

    // FIXME: mask out .cargo_vcs_info.json
    // FIXME: look into libgit2 vs just calling git

    let status = Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg(version1)
        .arg(version2)
        .status()?;

    let status = status.code().unwrap();

    // 0 = empty
    // 1 = some diff
    if status != 0 && status != 1 {
        return Err(eyre::eyre!("git diff failed!\n {}", status));
    }

    Ok(())
}

fn diffstat_crate(version1: &Path, version2: &Path) -> Result<DiffStat, VetError> {
    // ERRORS: all of this is properly fallible internal workings, we can fail
    // to diffstat some packages and still produce some useful output
    trace!("diffstating {version1:#?} {version2:#?}");
    // FIXME: mask out .cargo_vcs_info.json
    // FIXME: look into libgit2 vs just calling git

    let out = Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg("--shortstat")
        .arg(version1)
        .arg(version2)
        .output()?;

    // TODO: don't unwrap this
    let status = out.status.code().unwrap();
    // 0 = empty
    // 1 = some diff
    if status != 0 && status != 1 {
        return Err(eyre::eyre!(
            "command failed!\nout:\n{}\nstderr:\n{}",
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap()
        ));
    }

    let diffstat = String::from_utf8(out.stdout)?;

    let count = if diffstat.is_empty() {
        0
    } else {
        // 3 files changed, 9 insertions(+), 3 deletions(-)
        let mut parts = diffstat.split(',');
        parts.next().unwrap(); // Discard files

        fn parse_diffnum(part: Option<&str>) -> Option<u64> {
            part?.trim().split_once(' ')?.0.parse().ok()
        }

        let added: u64 = parse_diffnum(parts.next()).unwrap_or(0);
        let removed: u64 = parse_diffnum(parts.next()).unwrap_or(0);

        // ERRORS: Arguably this should just be an error but it's more of a
        // "have I completely misunderstood this format, if so let me know"
        // panic, so the assert *is* what I want..?
        assert_eq!(
            parts.next(),
            None,
            "diffstat had more parts than expected? {}",
            diffstat
        );

        added + removed
    };

    Ok(DiffStat {
        raw: diffstat,
        count,
    })
}

struct UserInfo {
    username: String,
    email: String,
}

fn get_user_info() -> Result<UserInfo, VetError> {
    // ERRORS: this is all properly fallible internal workings
    let username = {
        let out = Command::new("git")
            .arg("config")
            .arg("--get")
            .arg("user.name")
            .output()?;

        if !out.status.success() {
            return Err(eyre::eyre!(
                "could not get user.name from git!\nout:\n{}\nstderr:\n{}",
                String::from_utf8(out.stdout).unwrap(),
                String::from_utf8(out.stderr).unwrap()
            ));
        }
        String::from_utf8(out.stdout)?
    };

    let email = {
        let out = Command::new("git")
            .arg("config")
            .arg("--get")
            .arg("user.email")
            .output()?;

        if !out.status.success() {
            return Err(eyre::eyre!(
                "could not get user.email from git!\nout:\n{}\nstderr:\n{}",
                String::from_utf8(out.stdout).unwrap(),
                String::from_utf8(out.stderr).unwrap()
            ));
        }

        String::from_utf8(out.stdout)?
    };

    Ok(UserInfo {
        username: username.trim().to_string(),
        email: email.trim().to_string(),
    })
}

fn eula_for_criteria(audits: &AuditsFile, criteria: CriteriaStr) -> Option<String> {
    // ERRORS: it's possible this should be infallible, guarded by pre-validation?
    let builtin_eulas = [
        (
            format::SAFE_TO_DEPLOY,
            include_str!("criteria/safe-to-deploy.txt"),
        ),
        (
            format::SAFE_TO_RUN,
            include_str!("criteria/safe-to-run.txt"),
        ),
    ]
    .into_iter()
    .collect::<HashMap<_, _>>();

    // Several fallbacks
    // * Try to get the builtin criteria
    // * Try to get the criteria's description
    // * Try to fetch the criteria's url
    // * Just display the url
    builtin_eulas
        .get(criteria)
        .map(|s| s.to_string())
        .or_else(|| {
            audits.criteria.get(criteria).and_then(|c| {
                c.description.clone().or_else(|| {
                    c.description_url.as_ref().map(|url| {
                        reqwest::blocking::get(url)
                            .and_then(|r| r.text())
                            .map_err(|e| {
                                // ERRORS: does the user care, if we have this recovery mode afterwards?
                                warn!("Could not fetch criteria description: {e}");
                            })
                            .ok()
                            .unwrap_or_else(|| format!("See criteria description at {url}"))
                    })
                })
            })
        })
}

fn foreign_packages(metadata: &Metadata) -> impl Iterator<Item = &Package> {
    // Only analyze things from crates.io (no source = path-dep / workspace-member)
    metadata
        .packages
        .iter()
        .filter(|package| package.is_third_party())
}
