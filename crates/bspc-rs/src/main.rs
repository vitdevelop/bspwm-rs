//! `bspc-rs`: command-line client for `bspwm-rs`, matching `bspc`'s
//! arguments, socket protocol and exit codes.
//!
//! bspwm: `src/bspc.c` `main()`. Named `bspc-rs` rather than `bspc`
//! (`docs/design.md`, Project basics) so both can be installed side by
//! side; a script that calls the stock `bspc` still works unchanged by
//! pointing `BSPWM_SOCKET` at this program's socket.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use bsp_ipc::wire::{self, Reply};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("No arguments given.");
        return ExitCode::FAILURE;
    }

    let Some(socket_path) = wire::socket_path() else {
        eprintln!(
            "Failed to determine the socket path: set {} or XDG_RUNTIME_DIR.",
            wire::SOCKET_ENV_VAR
        );
        return ExitCode::FAILURE;
    };

    if args[0] == "--print-socket-path" {
        println!("{}", socket_path.display());
        return ExitCode::SUCCESS;
    }

    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to connect to the socket: {e}");
            return ExitCode::FAILURE;
        }
    };

    let request = wire::encode_request(args.iter().map(String::as_str));
    if let Err(e) = stream.write_all(&request) {
        eprintln!("Failed to send the data: {e}");
        return ExitCode::FAILURE;
    }

    // A blocking read loop rather than bspwm's `poll()`: one iteration per
    // reply chunk works for both an ordinary command (one chunk, then the
    // server closes the connection) and `subscribe` (the connection stays
    // open and each event arrives as its own chunk, forwarded to stdout
    // as it comes rather than buffered). bspc.c additionally polls
    // standard output for `POLLHUP` so a `subscribe | head -1` pipeline
    // exits promptly when the reader goes away; that refinement is not
    // implemented here (`docs/bsp-ipc.md`, scope) — this client
    // instead exits once its own write to a closed pipe fails.
    let mut had_failure = false;
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => match Reply::parse(&buf[..n]) {
                Reply::Ok(s) => {
                    let mut out = std::io::stdout();
                    if out.write_all(s.as_bytes()).is_err() || out.flush().is_err() {
                        break;
                    }
                }
                Reply::Fail(s) => {
                    had_failure = true;
                    let mut err = std::io::stderr();
                    let _ = err.write_all(s.as_bytes());
                    let _ = err.flush();
                }
            },
            Err(e) => {
                eprintln!("Failed to read the reply: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if had_failure {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
