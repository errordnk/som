//! A stand-in for `ssh`, for the tests that run a remote tab without a remote
//! machine: it drops everything up to and including `~/.local/bin/somsrv` (the
//! options, the host, the path of the far side's binary) and runs the `somsrv`
//! named by `FAKESSH_SOMSRV` with the rest, as the far side's shell would, with
//! the same stdio.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(at) = args.iter().position(|arg| arg == "~/.local/bin/somsrv") else {
        eprintln!("fakessh: no ~/.local/bin/somsrv in {args:?}");
        std::process::exit(2);
    };
    let Ok(somsrv) = std::env::var("FAKESSH_SOMSRV") else {
        eprintln!("fakessh: FAKESSH_SOMSRV is not set");
        std::process::exit(2);
    };
    match std::process::Command::new(&somsrv).args(&args[at + 1..]).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("fakessh: cannot run {somsrv}: {err}");
            std::process::exit(2);
        },
    }
}
