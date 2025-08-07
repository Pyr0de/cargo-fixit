use std::{
    collections::HashSet,
    env,
    io::{BufRead, BufReader, Cursor},
    path::Path,
    process::Stdio,
};

use cargo_util::paths;
use clap::Parser;
use indexmap::{IndexMap, IndexSet};
use rustfix::{collect_suggestions, CodeFix, Suggestion};
use tracing::{trace, warn};

use crate::{
    core::{shell, sysroot::get_sysroot},
    ops::check::{BuildUnit, CheckOutput, Message},
    util::{cli::CheckFlags, package::format_package_id, vcs::VcsOpts},
    CargoResult,
};

#[derive(Debug, Parser)]
pub struct FixitArgs {
    /// Run `clippy` instead of `check`
    #[arg(long)]
    clippy: bool,

    #[command(flatten)]
    vcs_opts: VcsOpts,

    #[command(flatten)]
    check_flags: CheckFlags,
}

impl FixitArgs {
    pub fn exec(self) -> CargoResult<()> {
        exec(self)
    }
}

#[derive(Debug, Default)]
struct File {
    fixes: u32,
}

#[tracing::instrument(skip_all)]
fn exec(args: FixitArgs) -> CargoResult<()> {
    args.vcs_opts.valid_vcs()?;

    let mut files: IndexMap<String, File> = IndexMap::new();

    let max_iterations: usize = env::var("CARGO_FIX_MAX_RETRIES")
        .ok()
        .and_then(|i| i.parse().ok())
        .unwrap_or(4);
    let mut iteration = 0;

    let mut last_errors = IndexMap::new();
    let mut current_target: Option<BuildUnit> = None;
    let mut seen = HashSet::new();

    loop {
        trace!("iteration={iteration}");
        trace!("current_target={current_target:?}");
        let (messages, _exit_code) = check(&args)?;

        let (mut errors, build_unit_map) = collect_errors(messages, &seen);

        if iteration >= max_iterations {
            if let Some(target) = current_target {
                if seen.iter().all(|b| b.package_id != target.package_id) {
                    shell::status("Checking", format_package_id(&target.package_id)?)?;
                }

                for (name, file) in files {
                    shell::fixed(name, file.fixes)?;
                }
                files = IndexMap::new();

                let mut errors = errors.shift_remove(&target).unwrap_or_else(IndexSet::new);

                if let Some(e) = build_unit_map.get(&target) {
                    for (_, e) in e.iter().flat_map(|(_, s)| s) {
                        let Some(e) = e else {
                            continue;
                        };
                        errors.insert(e.to_owned());
                    }
                }
                for e in errors {
                    shell::print_ansi_stderr(format!("{}\n\n", e.trim_end()).as_bytes())?;
                }

                seen.insert(target);
                current_target = None;
                iteration = 0;
            } else {
                break;
            }
        }

        let mut made_changes = false;

        for (build_unit, file_map) in build_unit_map {
            if seen.contains(&build_unit) {
                continue;
            }

            let build_unit_errors = errors
                .entry(build_unit.clone())
                .or_insert_with(IndexSet::new);

            if current_target.is_none() && file_map.is_empty() {
                if seen.iter().all(|b| b.package_id != build_unit.package_id) {
                    shell::status("Checking", format_package_id(&build_unit.package_id)?)?;
                }
                for e in build_unit_errors.iter() {
                    shell::print_ansi_stderr(format!("{}\n\n", e.trim_end()).as_bytes())?;
                }
                errors.shift_remove(&build_unit);

                seen.insert(build_unit);
            } else if !file_map.is_empty()
                && current_target.get_or_insert(build_unit.clone()) == &build_unit
                && fix_errors(&mut files, file_map, build_unit_errors)?
            {
                made_changes = true;
                break;
            }
        }

        trace!("made_changes={made_changes:?}");
        trace!("current_target={current_target:?}");

        last_errors = errors;
        iteration += 1;

        if !made_changes {
            if let Some(pkg) = current_target {
                if seen.iter().all(|b| b.package_id != pkg.package_id) {
                    shell::status("Checking", format_package_id(&pkg.package_id)?)?;
                }

                for (name, file) in files {
                    shell::fixed(name, file.fixes)?;
                }
                files = IndexMap::new();

                let errors = last_errors.shift_remove(&pkg).unwrap_or_else(IndexSet::new);
                for e in errors {
                    shell::print_ansi_stderr(format!("{}\n\n", e.trim_end()).as_bytes())?;
                }

                seen.insert(pkg);
                current_target = None;
                iteration = 0;
            } else {
                break;
            }
        }
    }

    for (name, file) in files {
        shell::fixed(name, file.fixes)?;
    }

    for e in last_errors.iter().flat_map(|(_, e)| e) {
        shell::print_ansi_stderr(format!("{}\n\n", e.trim_end()).as_bytes())?;
    }

    Ok(())
}

fn check(args: &FixitArgs) -> CargoResult<(impl Iterator<Item = CheckOutput>, Option<i32>)> {
    let cmd = if args.clippy { "clippy" } else { "check" };
    let command = std::process::Command::new(env!("CARGO"))
        .args([cmd, "--message-format", "json-diagnostic-rendered-ansi"])
        .args(args.check_flags.to_flags())
        // This allows `cargo fix` to work even if the crate has #[deny(warnings)].
        .env("RUSTFLAGS", "--cap-lints=warn")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()?;

    let buf = BufReader::new(Cursor::new(command.stdout));

    Ok((
        buf.lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str(&l).ok()),
        command.status.code(),
    ))
}

#[tracing::instrument(skip_all)]
#[allow(clippy::type_complexity)]
fn collect_errors(
    messages: impl Iterator<Item = CheckOutput>,
    seen: &HashSet<BuildUnit>,
) -> (
    IndexMap<BuildUnit, IndexSet<String>>,
    IndexMap<BuildUnit, IndexMap<String, IndexSet<(Suggestion, Option<String>)>>>,
) {
    let only = HashSet::new();
    let mut build_unit_map = IndexMap::new();

    let mut errors = IndexMap::new();

    for message in messages {
        let Message {
            build_unit,
            message: diagnostic,
        } = match message {
            CheckOutput::Message(m) => m,
            CheckOutput::Artifact(a) => {
                if !seen.contains(&a.build_unit) && !a.fresh {
                    build_unit_map
                        .entry(a.build_unit.clone())
                        .or_insert(IndexMap::new());
                }
                continue;
            }
        };

        let errors = errors
            .entry(build_unit.clone())
            .or_insert_with(IndexSet::new);

        if seen.contains(&build_unit) {
            trace!("rejecting build unit `{:?}` already seen", build_unit);
            continue;
        }

        let file_map = build_unit_map
            .entry(build_unit.clone())
            .or_insert(IndexMap::new());

        let filter = if env::var("__CARGO_FIX_YOLO").is_ok() {
            rustfix::Filter::Everything
        } else {
            rustfix::Filter::MachineApplicableOnly
        };

        let Some(suggestion) = collect_suggestions(&diagnostic, &only, filter) else {
            trace!("rejecting as not a MachineApplicable diagnosis: {diagnostic:?}");
            if let Some(rendered) = diagnostic.rendered {
                errors.insert(rendered);
            }
            continue;
        };

        let mut file_names = suggestion
            .solutions
            .iter()
            .flat_map(|s| s.replacements.iter())
            .map(|r| &r.snippet.file_name);

        let Some(file_name) = file_names.next() else {
            trace!("rejecting as it has no solutions {:?}", suggestion);
            if let Some(rendered) = diagnostic.rendered {
                errors.insert(rendered);
            }
            continue;
        };

        if !file_names.all(|f| f == file_name) {
            trace!("rejecting as it changes multiple files: {:?}", suggestion);
            if let Some(rendered) = diagnostic.rendered {
                errors.insert(rendered);
            }
            continue;
        }

        let file_path = Path::new(&file_name);
        // Do not write into registry cache. See rust-lang/cargo#9857.
        if let Ok(home) = env::var("CARGO_HOME") {
            if file_path.starts_with(home) {
                continue;
            }
        }

        if let Some(sysroot) = get_sysroot() {
            if file_path.starts_with(sysroot) {
                continue;
            }
        }

        file_map
            .entry(file_name.to_owned())
            .or_insert_with(IndexSet::new)
            .insert((suggestion, diagnostic.rendered));
    }

    (errors, build_unit_map)
}

#[tracing::instrument(skip_all)]
fn fix_errors(
    files: &mut IndexMap<String, File>,
    file_map: IndexMap<String, IndexSet<(Suggestion, Option<String>)>>,
    errors: &mut IndexSet<String>,
) -> CargoResult<bool> {
    let mut made_changes = false;
    for (file, suggestions) in file_map {
        let code = match paths::read(file.as_ref()) {
            Ok(s) => s,
            Err(e) => {
                warn!("failed to read `{}`: {}", file, e);
                errors.extend(suggestions.iter().filter_map(|(_, e)| e.clone()));
                continue;
            }
        };

        let mut fixed = CodeFix::new(&code);
        let mut num_fixes = 0;

        for (suggestion, rendered) in suggestions.iter().rev() {
            match fixed.apply(suggestion) {
                Ok(()) => num_fixes += 1,
                Err(rustfix::Error::AlreadyReplaced {
                    is_identical: true, ..
                }) => {}
                Err(e) => {
                    if let Some(rendered) = rendered {
                        errors.insert(rendered.to_owned());
                    }
                    warn!("{e:?}");
                }
            }
        }
        if fixed.modified() {
            let new_code = fixed.finish()?;
            paths::write(&file, new_code)?;
            made_changes = true;
            files.entry(file).or_default().fixes += num_fixes;
        }
    }

    Ok(made_changes)
}
