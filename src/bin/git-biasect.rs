use argh::FromArgs;
use git_biasect::alloc::{init, step, BasicAllocator};
use git_biasect::shell::{
    bisect_report, get_commit_files, get_commits, reproducer_shell_commands, run_script,
    worktree_prune,
};
use git_biasect::visualize::print_commits;
use git_biasect::{CommitState, Status};
use rand::seq::IteratorRandom;
use rand::{rngs::StdRng, SeedableRng};
use std::collections::HashSet;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::str;
use std::thread::sleep;
use std::time::{Duration, Instant};

/**
Git Biasect
*/
#[derive(FromArgs)]
struct Args {
    #[argh(subcommand)]
    subcommand: SubCommands,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum SubCommands {
    Run(RunOptions),
    Next(NextOptions),
}

#[derive(FromArgs, Debug)]
/// git bisect run
#[argh(subcommand, name = "run")]
struct RunOptions {
    /// runners to run concurrently
    #[argh(option, short = 'j')]
    jobs: usize,

    /// reckless mode. Don't check that the bounds of the bisection (good/bad) can be replicated using the given script
    #[argh(switch, short = 'r')]
    reckless: bool,

    /// set the current working directory
    #[argh(option, short = 'C', default = "PathBuf::from(\".\")")]
    repo_path: PathBuf,

    /// script to run, encapsulated in quotes. Eg. "make build"
    #[argh(positional)]
    script: String,
}

#[derive(FromArgs)]
/// git bisect next
#[argh(subcommand, name = "next")]
struct NextOptions {
    /// check that the bounds of the bisection (good/bad) can be replicated using the given script
    #[argh(switch, short = 'c')]
    check_bounds: bool,

    /// set the current working directory
    #[argh(option, short = 'C', default = "PathBuf::from(\".\")")]
    repo_path: PathBuf,
}

fn start_runners(
    runner_commits: &[usize],
    commits: &[String],
    repo_path: &PathBuf,
    script_path: &str,
) -> Vec<(usize, Child)> {
    runner_commits
        .iter()
        .map(|commit_idx| {
            (
                *commit_idx,
                run_script(
                    &fs::canonicalize(repo_path).unwrap(),
                    script_path,
                    commits.get(*commit_idx).unwrap(),
                ),
            )
        })
        .collect()
}

fn bounds_validated(commits: &Vec<CommitState>, reckless_mode: bool) -> bool {
    if reckless_mode || commits.is_empty() {
        return true;
    }

    let commit_states = commits.iter().map(|x| x.status).collect::<HashSet<_>>();

    commit_states.contains(&Status::Good) && commit_states.contains(&Status::Bad)
}

fn bisect_report_all(commits: &Vec<CommitState>, repo_path: &Path) {
    for commit in commits {
        if commit.status != Status::Unknown {
            bisect_report(repo_path, &commit.status, &commit.hash);
        }
    }
}

fn main() -> Result<(), String> {
    let args: Args = argh::from_env();

    match args.subcommand {
        SubCommands::Run(run_opts) => {
            let commits = get_commits(&run_opts.repo_path)?;
            let _files_per_commit = commits
                .iter()
                .map(|hash| get_commit_files(&run_opts.repo_path, hash).unwrap())
                .collect::<Vec<_>>();

            let mut state = init(&commits, run_opts.jobs, !run_opts.reckless);
            let mut runners;

            // Kick off runners
            let start = Instant::now();
            runners = start_runners(
                &state.runners.commits,
                &commits,
                &run_opts.repo_path,
                &run_opts.script,
            );

            let mut loop_iter = 0;
            loop {
                loop_iter += 1;
                let mut rng = StdRng::seed_from_u64(loop_iter);

                print_commits(
                    state
                        .commits
                        .iter()
                        .map(|x| x.status)
                        .collect::<Vec<_>>()
                        .as_slice(),
                    &state.runners.commits,
                );

                // Wait for the first completed child
                let mut first_completed = None;

                // Runner count doesn't update
                let runners_count = runners.len();
                while first_completed.is_none() {
                    for child in runners.iter_mut().choose_multiple(&mut rng, runners_count) {
                        let res = child.1.try_wait();
                        let res = res.unwrap();
                        if let Some(exit_status) = res {
                            first_completed = Some((child.0, exit_status));
                        }
                    }
                    // TODO: Replace with condvar or learn from the bisection script runtime to reduce compute burden
                    sleep(Duration::from_secs(1));
                }

                let commit_index_exit_code = first_completed.unwrap();
                let exit_code = commit_index_exit_code
                    .1
                    .code()
                    .or_else(|| commit_index_exit_code.1.signal())
                    .unwrap();
                let exit_status = if exit_code == 0 {
                    Status::Good
                } else if exit_code == 124 {
                    Status::Skip
                } else {
                    Status::Bad
                };

                // Check if result is invalid
                // TODO: Nicer error messages that allow users to reproduce the failure with example commands
                if commit_index_exit_code.0 == 0 && exit_status == Status::Bad {
                    // The first commit must be good - that's what the user told us when setting up the bisection!
                    eprintln!(
                        "Initial bisection bounds invalid.\n\
                        Commit: `{}` evaluated to bad with exit code {}.\n\
                        The oldest commit must not be bad.\n\
                        \n\
                        Reproduce this failure with these commands:\n\
                        {}",
                        commits.get(commit_index_exit_code.0).unwrap(),
                        exit_code,
                        reproducer_shell_commands(
                            &run_opts.repo_path,
                            &run_opts.script,
                            &state.commits.get(commit_index_exit_code.0).unwrap().hash
                        )
                    );
                    return Ok(());
                } else if commit_index_exit_code.0 == commits.len() - 1
                    && exit_status == Status::Good
                {
                    // The last commit must be bad - that's what the user told us when setting up the bisection!
                    eprintln!(
                        "Initial bisection bounds invalid.\n\
                        Commit: `{}` evaluated to good with exit code {}.\n\
                        The newest commit must not be good.\n\
                        \n\
                        Reproduce this failure with these commands:\n\
                        {}",
                        commits.get(commit_index_exit_code.0).unwrap(),
                        exit_code,
                        reproducer_shell_commands(
                            &run_opts.repo_path,
                            &run_opts.script,
                            &state.commits.get(commit_index_exit_code.0).unwrap().hash
                        )
                    );
                    return Ok(());
                }

                let old_state = state;

                // Perform step
                let invalidated_runners;
                let new_runners;
                let current_runtime = start.elapsed().as_secs_f64();

                let commit_runtime = current_runtime
                    - *old_state
                        .runners
                        .start_times
                        .get(
                            old_state
                                .runners
                                .commits
                                .iter()
                                .enumerate()
                                .filter(|(_, commit_idx)| commit_idx == &&commit_index_exit_code.0)
                                .map(|(runner_idx, _)| runner_idx)
                                .next()
                                .unwrap(),
                        )
                        .unwrap();

                (state, invalidated_runners, new_runners) = step::<BasicAllocator>(
                    &old_state,
                    exit_status,
                    commit_index_exit_code.0,
                    commit_runtime,
                    current_runtime,
                );

                // Report status to git after ensuring bounds are valid
                if bounds_validated(&state.commits, run_opts.reckless)
                    && (commit_index_exit_code.0 == 0
                        || commit_index_exit_code.0 == state.commits.len() - 1)
                {
                    // Report all bisection steps that have completed while validating the bounds
                    println!(
                        "Bounds newly validated, reporting commits {:?}",
                        state.commits
                    );
                    bisect_report_all(&state.commits, &run_opts.repo_path);
                } else if bounds_validated(&state.commits, run_opts.reckless) {
                    // Report all bisection steps right away when bounds are validated
                    bisect_report(
                        &run_opts.repo_path,
                        &exit_status,
                        commits.get(commit_index_exit_code.0).unwrap(),
                    );
                }

                // Cancel invalidated tasks
                // TODO: Clean up temp folders
                let _ = old_state
                    .runners
                    .commits
                    .iter()
                    .filter(|commit_idx| invalidated_runners.contains(commit_idx))
                    .map(|commit_idx| {
                        let mut invalidated_runners = runners
                            .iter_mut()
                            .filter(|x| x.0 == *commit_idx)
                            .collect::<Vec<_>>();

                        for invalidated_runners in invalidated_runners.iter_mut() {
                            // println!("Killing {}", invalidated_runners.0);
                            let killed = invalidated_runners.1.kill();
                            if killed.is_ok() {
                                // println!("Successfully cancelled {}", invalidated_runners.0);
                            } else {
                                panic!("Failed to kill invalidated runner: {:?}", killed.err());
                            }
                        }
                    })
                    .collect::<Vec<_>>();

                let e_runners = runners
                    .into_iter()
                    .filter(|commit| {
                        !(invalidated_runners.contains(&commit.0)
                            || commit_index_exit_code.0 == commit.0)
                    })
                    .collect::<Vec<_>>();

                let n_runners = start_runners(
                    &new_runners,
                    &commits,
                    &run_opts.repo_path,
                    &run_opts.script,
                );

                runners = e_runners.into_iter().chain(n_runners).collect();

                if runners.is_empty() {
                    break;
                }
            }

            print_commits(
                state
                    .commits
                    .iter()
                    .map(|x| x.status)
                    .collect::<Vec<_>>()
                    .as_slice(),
                &state.runners.commits,
            );

            let _ = worktree_prune(&run_opts.repo_path).wait();
        }
        SubCommands::Next(next_opts) => {
            let commits = get_commits(&next_opts.repo_path)?;

            let state = init(&commits, 1, next_opts.check_bounds);

            print_commits(
                state
                    .commits
                    .iter()
                    .map(|x| x.status)
                    .collect::<Vec<_>>()
                    .as_slice(),
                &state.runners.commits,
            );
        }
    }

    Ok(())
}
