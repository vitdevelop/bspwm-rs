//! Reading and writing NUL-separated requests and replies, and finding the
//! socket path.
//!
//! bspwm: `src/common.h`, `src/bspc.c`, `src/messages.c` `handle_message()`.
//! A request is every `bspc` argument joined by NUL bytes (each argument,
//! including the last, gets a trailing NUL — see [`encode_request`]); a
//! reply is plain text, except that a failed command's reply starts with
//! [`FAILURE_MARKER`] so `bspc` knows to exit non-zero and print the rest
//! to standard error.

use std::env;
use std::path::PathBuf;

/// The first byte of a failed command's reply.
///
/// bspwm: `src/common.h` `FAILURE_MESSAGE` (`"\x07"`, the bell character).
pub const FAILURE_MARKER: u8 = 0x07;

/// Environment variable naming the socket path, checked before falling back
/// to a default under `XDG_RUNTIME_DIR`.
///
/// bspwm: `src/common.h` `SOCKET_ENV_VAR`. Kept as the same name (rather
/// than a `bspwm-rs`-specific one) so a `bspwmrc` that already exports it
/// for the stock `bspc` keeps working unchanged.
pub const SOCKET_ENV_VAR: &str = "BSPWM_SOCKET";

/// Default socket filename under `XDG_RUNTIME_DIR` when `BSPWM_SOCKET` is
/// unset.
///
/// bspwm derives its default path from the X11 display/screen number
/// (`src/common.h` `SOCKET_PATH_TPL`, `"/tmp/bspwm%s_%i_%i-socket"`), which
/// has no Wayland equivalent (`docs/bsp-ipc.md`); a Wayland session has at
/// most one compositor instance per `XDG_RUNTIME_DIR`, so a fixed name
/// there is sufficient.
pub const DEFAULT_SOCKET_NAME: &str = "bspwm-rs-socket";

/// Resolves the control socket path: `BSPWM_SOCKET` if set, otherwise
/// `$XDG_RUNTIME_DIR/bspwm-rs-socket`.
///
/// Returns `None` if `BSPWM_SOCKET` is unset and `XDG_RUNTIME_DIR` is also
/// unset (bspwm falls back to parsing an X11 display name in that case,
/// `src/bspc.c`; there is none to parse on Wayland, so the caller must
/// report this as a configuration error rather than guess a path).
///
/// bspwm: `src/bspc.c` `main()`'s socket-path resolution.
pub fn socket_path() -> Option<PathBuf> {
    resolve_socket_path(
        env::var(SOCKET_ENV_VAR).ok().as_deref(),
        env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

/// The pure decision behind [`socket_path`], taking the two environment
/// variables as plain arguments so it can be tested without touching
/// process-global environment state.
fn resolve_socket_path(
    bspwm_socket: Option<&str>,
    xdg_runtime_dir: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = bspwm_socket {
        return Some(PathBuf::from(p));
    }
    let mut path = PathBuf::from(xdg_runtime_dir?);
    path.push(DEFAULT_SOCKET_NAME);
    Some(path)
}

/// Encodes `bspc`-style arguments into one request message: every argument
/// followed by a single NUL byte.
///
/// bspwm: `src/bspc.c` `main()` (`snprintf(msg + offset, rem, "%s%c", *argv, 0)`
/// for every argument).
pub fn encode_request<'a>(args: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let mut buf = Vec::new();
    for arg in args {
        buf.extend_from_slice(arg.as_bytes());
        buf.push(0);
    }
    buf
}

/// Decodes a raw request message into its arguments: `bytes` is split at
/// every NUL byte, and any trailing bytes after the last NUL (a malformed
/// request with no terminator on its final argument) are dropped, exactly
/// as bspwm's own parser drops them.
///
/// Invalid UTF-8 within an argument is replaced (lossily) rather than
/// rejected: bspwm treats arguments as opaque byte strings, but every
/// caller in this codebase (and `bspc` itself) only ever sends UTF-8.
///
/// bspwm: `src/messages.c` `handle_message()`.
pub fn decode_request(bytes: &[u8]) -> Vec<String> {
    let mut args = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == 0 {
            args.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            start = i + 1;
        }
    }
    args
}

/// A command's reply: successful output, or a failure message.
///
/// bspwm: `src/messages.c` `fail()` prepends [`FAILURE_MARKER`]
/// (`FAILURE_MESSAGE`) to the formatted message; anything else written to
/// the response stream is a plain success reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The command succeeded; this is its output (may be empty).
    Ok(String),
    /// The command failed; this is the (possibly empty) error message.
    Fail(String),
}

impl Reply {
    /// Encodes this reply as the bytes bspwm would write to the client
    /// socket.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Reply::Ok(s) => s.into_bytes(),
            Reply::Fail(s) => {
                let mut buf = vec![FAILURE_MARKER];
                buf.extend(s.into_bytes());
                buf
            }
        }
    }

    /// Decodes a reply's raw bytes, exactly as `bspc` distinguishes success
    /// from failure: the first byte, if it is [`FAILURE_MARKER`], marks the
    /// rest as an error message.
    ///
    /// bspwm: `src/bspc.c` `main()` (`if (rsp[0] == FAILURE_MESSAGE[0])`).
    pub fn parse(bytes: &[u8]) -> Reply {
        match bytes.split_first() {
            Some((&first, rest)) if first == FAILURE_MARKER => {
                Reply::Fail(String::from_utf8_lossy(rest).into_owned())
            }
            _ => Reply::Ok(String::from_utf8_lossy(bytes).into_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_nul_terminates_every_argument() {
        let bytes = encode_request(["node", "-f", "west"]);
        assert_eq!(bytes, b"node\0-f\0west\0");
    }

    #[test]
    fn decode_request_splits_on_nul_and_drops_untererminated_trailer() {
        let args = decode_request(b"node\0-f\0west\0trailing");
        assert_eq!(args, vec!["node", "-f", "west"]);
    }

    #[test]
    fn decode_request_of_empty_message_is_empty() {
        assert!(decode_request(b"").is_empty());
    }

    #[test]
    fn reply_ok_round_trips() {
        let bytes = Reply::Ok("0x00000001\n".to_string()).into_bytes();
        assert_eq!(Reply::parse(&bytes), Reply::Ok("0x00000001\n".to_string()));
    }

    #[test]
    fn reply_fail_round_trips_and_marks_first_byte() {
        let bytes = Reply::Fail("node: Unknown command: 'x'.\n".to_string()).into_bytes();
        assert_eq!(bytes[0], FAILURE_MARKER);
        assert_eq!(
            Reply::parse(&bytes),
            Reply::Fail("node: Unknown command: 'x'.\n".to_string())
        );
    }

    #[test]
    fn resolve_socket_path_prefers_bspwm_socket_env_var() {
        assert_eq!(
            resolve_socket_path(Some("/tmp/example-socket"), Some("/run/user/1000")),
            Some(PathBuf::from("/tmp/example-socket"))
        );
    }

    #[test]
    fn resolve_socket_path_falls_back_to_xdg_runtime_dir() {
        assert_eq!(
            resolve_socket_path(None, Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/bspwm-rs-socket"))
        );
    }

    #[test]
    fn resolve_socket_path_is_none_with_neither_env_var() {
        assert_eq!(resolve_socket_path(None, None), None);
    }
}
