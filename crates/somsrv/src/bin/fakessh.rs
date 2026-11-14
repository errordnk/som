//! A stand-in for `ssh`, for the tests that run a remote tab without a remote
//! machine: it drops everything up to and including `/tmp/som-somsrv/somsrv` (the
//! options, the host, the path of the far side's binary) and runs the `somsrv`
//! named by `FAKESSH_SOMSRV` with the rest, as the far side's shell would, with
//! the same stdio.
//!
//! `$SSH_CLIENT` is set for the child, as sshd does: a Windows RELAY without
//! it counts as local and asks its console for win32-input-mode (see
//! `somsrv::relay`'s `WIN32_INPUT_MODE_ON`), which recodes the link frames'
//! GS markers into `CSI Vk;...;_` key sequences on their way in — Som's
//! channel to that machine's daemon dies with nothing ever listening. The
//! value's shape (`<ip> <port> <port>`) is all `protocol::ssh_client_id`
//! reads; `<user>@<that ip>` is the client id the RELAY registers its
//! sessions under, which a test that cleans up after itself by
//! `admin::kill_session` must use.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(at) = args.iter().position(|arg| arg == "/tmp/som-somsrv/somsrv") else {
        eprintln!("fakessh: no /tmp/som-somsrv/somsrv in {args:?}");
        std::process::exit(2);
    };
    let Ok(somsrv) = std::env::var("FAKESSH_SOMSRV") else {
        eprintln!("fakessh: FAKESSH_SOMSRV is not set");
        std::process::exit(2);
    };
    match std::process::Command::new(&somsrv)
        .args(&args[at + 1..])
        .env("SSH_CLIENT", "127.0.0.1 62222 22")
        .status()
    {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("fakessh: cannot run {somsrv}: {err}");
            std::process::exit(2);
        },
    }
}
