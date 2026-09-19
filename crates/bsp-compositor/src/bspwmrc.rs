//! Runs `bspwmrc` at startup, matching bspwm's own behavior exactly:
//! executed directly (not through `sh -c` — `bspwmrc` is expected to be
//! its own executable script, shebang line and all), given one
//! argument, and not waited on — it typically issues `bspc` commands
//! against the control socket as it runs, so nothing here blocks on it.
//!
//! bspwm: `src/bspwm.c` `main()`'s config-path resolution and
//! `run_config(run_level)` call site (right after the control socket
//! starts listening, before the main loop begins), `src/settings.c`
//! `run_config()`.

use std::path::PathBuf;

/// Resolves the `bspwmrc` path bspwm itself would use, given
/// `XDG_CONFIG_HOME` and `HOME` (`None` for an unset variable):
/// `$XDG_CONFIG_HOME/bspwm/bspwmrc` if set, else `$HOME/.config/bspwm/
/// bspwmrc`. Deliberately the *same* path bspwm itself reads (not e.g.
/// `bspwm-rs/bspwmrc`) — `docs/migrating.md`: an existing `bspwmrc` is
/// meant to keep working unchanged, the same way `bsp-hotkeys` reads
/// the same `sxhkdrc` sxhkd would.
///
/// bspwm: `src/bspwm.c` `main()`'s config-path block, `src/bspwm.h`
/// `CONFIG_HOME_ENV`/`WM_NAME`/`CONFIG_NAME`. Takes the two variables
/// as parameters rather than reading the environment itself, so it can
/// be tested without touching real process state (mirrors
/// `bsp_ipc::wire::resolve_socket_path` and
/// `bsp_hotkeys::config::resolve_path`).
#[must_use]
fn resolve_path(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(config_home) = xdg_config_home {
        return Some(PathBuf::from(config_home).join("bspwm/bspwmrc"));
    }
    home.map(|h| PathBuf::from(h).join(".config/bspwm/bspwmrc"))
}

/// Runs `bspwmrc`, if one is found. Not an error if it isn't — matches
/// bspwm itself, which does not treat a missing config specially either
/// (`execl` simply fails and is logged, `docs/design.md`'s Reliability
/// section: a missing config must never lock the user out, and this
/// compositor starts up perfectly well with no `bspwmrc` at all).
///
/// bspwm: `src/settings.c` `run_config()`. `run_level` is always `"0"`
/// here (bspwm's own default): this build has neither `--load-state`
/// nor an externally-provided socket fd (`docs/bsp-ipc.md`'s The IPC
/// progress) to set its two bits.
pub fn run() {
    let xdg_config_home = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    let Some(path) = resolve_path(xdg_config_home.as_deref(), home.as_deref()) else {
        tracing::warn!("no XDG_CONFIG_HOME/HOME: bspwmrc disabled");
        return;
    };
    let mut rc = std::process::Command::new(&path);
    rc.arg("0");
    match crate::spawn::clean_signal_mask(&mut rc).spawn() {
        Ok(child) => {
            crate::hotkeys::reap(child);
            tracing::info!(path = %path.display(), "ran bspwmrc");
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no bspwmrc found");
        }
        Err(err) => tracing::warn!(path = %path.display(), "failed to run bspwmrc: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_prefers_xdg_config_home() {
        assert_eq!(
            resolve_path(Some("/x"), Some("/home/u")),
            Some(PathBuf::from("/x/bspwm/bspwmrc"))
        );
    }

    #[test]
    fn resolve_path_falls_back_to_home_dot_config() {
        assert_eq!(
            resolve_path(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/bspwm/bspwmrc"))
        );
    }

    #[test]
    fn resolve_path_is_none_with_neither_variable() {
        assert_eq!(resolve_path(None, None), None);
    }
}
