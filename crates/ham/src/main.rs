//! `ham`: see the crate docs and `crates/ham/AGENTS.md`.

fn main() {
    // nosemgrep: args -- the first element, the program name, only decides whether the notice of the earlier name is printed and the rest is the subcommand and options of the ham command line, parsed as data and never used to decide who may do what
    let all: Vec<String> = std::env::args().collect();
    // COMPAT-CAH: `setup.sh` makes a link named cah to this program for the window of the earlier name; the name it was started under only adds
    // one notice on standard error, and the program then behaves as ham does.
    let started_under_earlier_name =
        ham::cli::started_under_earlier_name(all.first().map(String::as_str));
    let args: Vec<String> = all.into_iter().skip(1).collect();
    ham::cli::announce_earlier_names(started_under_earlier_name, &args);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ham: cannot start the async runtime: {e}");
            std::process::exit(ham_core::exit::ERROR);
        }
    };
    let code = runtime.block_on(ham::cli::main_with(&args));
    // Leave the event-reader thread and pooled connections behind without waiting on them.
    runtime.shutdown_background();
    std::process::exit(code);
}
