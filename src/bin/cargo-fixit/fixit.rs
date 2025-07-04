use std::{
    collections::HashSet,
    env,
    io::{BufRead, BufReader},
    path::Path,
    process::Stdio,
};

use cargo_fixit::{shell, CargoResult, CheckFlags, CheckMessage, Target, VcsOpts};
use cargo_util::paths;
use clap::Parser;
use indexmap::{IndexMap, IndexSet};
use rustfix::{collect_suggestions, CodeFix};
use tracing::{trace, warn};

#[derive(Debug, Parser)]
pub(crate) struct FixitArgs {
    /// Run `clippy` instead of `check`
    #[arg(long)]
    clippy: bool,

    #[command(flatten)]
    vcs_opts: VcsOpts,

    #[command(flatten)]
    check_flags: CheckFlags,
}

impl FixitArgs {
    pub(crate) fn exec(self) -> CargoResult<()> {
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

    let mut files = IndexMap::new();

    let max_iterations: usize = env::var("CARGO_FIX_MAX_RETRIES")
        .ok()
        .and_then(|i| i.parse().ok())
        .unwrap_or(4);
    let mut iteration = 0;

    let mut last_errors;

    let mut current_target = None;
    let mut seen = HashSet::new();

    loop {
        trace!("iteration={iteration}");
        trace!("current_target={current_target:?}");
        let (errors, made_changes) = run_rustfix(&args, &mut files, &mut current_target, &seen)?;
        trace!("made_changes={made_changes:?}");
        trace!("current_target={current_target:?}");

        last_errors = errors;
        iteration += 1;

        if !made_changes || iteration >= max_iterations {
            if let Some(pkg) = current_target {
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

    for e in last_errors {
        eprint!("{}\n\n", e.trim_end());
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn run_rustfix(
    args: &FixitArgs,
    files: &mut IndexMap<String, File>,
    current_target: &mut Option<(Target, String)>,
    seen: &HashSet<(Target, String)>,
) -> CargoResult<(IndexSet<String>, bool)> {
    let only = HashSet::new();
    let mut file_map = IndexMap::new();

    let mut errors = IndexSet::new();

    let cmd = if args.clippy { "clippy" } else { "check" };
    let mut command = std::process::Command::new(env!("CARGO"))
        .args([cmd, "--message-format", "json"])
        .args(args.check_flags.to_flags())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let buf = BufReader::new(command.stdout.take().expect("could not capture output"));

    for line in buf.lines() {
        let Ok(CheckMessage {
            target,
            message: diagnostic,
            package_id,
        }) = serde_json::from_str(&line?)
        else {
            continue;
        };
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

        let file_names = suggestion
            .solutions
            .iter()
            .flat_map(|s| s.replacements.iter())
            .map(|r| &r.snippet.file_name);

        let file_name = if let Some(file_name) = file_names.clone().next() {
            file_name.clone()
        } else {
            trace!("rejecting as it has no solutions {:?}", suggestion);
            if let Some(rendered) = diagnostic.rendered {
                errors.insert(rendered);
            }
            continue;
        };

        if !file_names.clone().all(|f| f == &file_name) {
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

        let target = (target.clone(), package_id.clone());

        if seen.contains(&target) {
            trace!(
                "rejecting package id `{}` already seen: {:?}",
                package_id,
                suggestion,
            );
            if let Some(rendered) = diagnostic.rendered {
                errors.insert(rendered);
            }
            continue;
        }

        let current_target = current_target.get_or_insert(target.clone());

        if current_target == &target {
            file_map
                .entry(file_name)
                .or_insert_with(IndexSet::new)
                .insert((suggestion, diagnostic.rendered));
        }
    }

    let _exit_code = command.wait()?;

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

    Ok((errors, made_changes))
}
