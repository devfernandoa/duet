//! `duetctl`: the local control CLI, as a plain binary rather than
//! `duet agent ...` buried behind the GTK app's own argv — see
//! `.claude/steps.md` Milestone 3 ("duetctl: agents list, agents inspect,
//! send, connections list, workspace inspect"). This binary is intentionally
//! thin: all parsing and behavior live in `duet::control`, shared with the
//! GTK binary's own pre-GTK CLI dispatch, so neither ever reimplements the
//! other.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ok = duet::control::run_cli(&args);
    std::process::exit(if ok { 0 } else { 1 });
}
