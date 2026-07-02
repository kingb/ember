//! Auto-injected shell integration (OSC 133) — design §8.1.
//!
//! Ember can make a spawned shell emit OSC 133 marks *without* the user editing
//! their rc, so the exit-status gutter + jump-to-prompt "just work" (Ghostty's
//! model). We write a tiny integration dir and point the shell at it via env, and
//! that dir **chains** the user's real config first (never replaces it).
//!
//! - **zsh:** set `ZDOTDIR` to our dir; our `.zshenv`/`.zshrc` restore the user's
//!   `ZDOTDIR` and source their files, then install `precmd`/`preexec` hooks.
//! - **bash:** run with `--rcfile <ours>`; ours sources the user's `~/.bashrc`
//!   then adds a `PROMPT_COMMAND` + `DEBUG` trap.
//!
//! Shells emitting OSC 133 already (many zsh setups) will simply mark twice at the
//! same line — cosmetically one bar. Fish/others are a documented follow-up.

use std::path::{Path, PathBuf};

/// The env vars + extra args to apply to a shell command so it emits OSC 133.
#[derive(Default, Debug)]
pub struct Injection {
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
}

/// Prepare shell integration for `program` (a path or name). Writes the
/// integration files under `dir` and returns the env/args to apply. Returns an
/// empty `Injection` for unsupported shells (or on any IO error — never fatal).
pub fn prepare(program: &str, dir: &Path) -> Injection {
    let shell = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    match shell {
        "zsh" => prepare_zsh(dir).unwrap_or_default(),
        "bash" => prepare_bash(dir).unwrap_or_default(),
        _ => Injection::default(),
    }
}

const HOOKS_ZSH: &str = r#"
# Ember shell integration (OSC 133). Marks prompts + command exit status.
_ember_precmd() {
  local ret=$?
  print -n "\e]133;D;${ret}\e\\"
  print -n "\e]133;A\e\\"
}
_ember_preexec() { print -n "\e]133;C\e\\" }
autoload -Uz add-zsh-hook 2>/dev/null
if whence add-zsh-hook >/dev/null 2>&1; then
  add-zsh-hook precmd _ember_precmd
  add-zsh-hook preexec _ember_preexec
fi
"#;

fn prepare_zsh(dir: &Path) -> std::io::Result<Injection> {
    create_private_dir(dir)?;
    // The user's real ZDOTDIR (where their .zshrc lives).
    let orig = std::env::var("ZDOTDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_default();
    let ours = dir.to_string_lossy().into_owned();

    // zsh re-evaluates $ZDOTDIR before EACH startup file, so ZDOTDIR must keep
    // pointing at our dir until the LAST file we need (.zshrc) has been read —
    // restoring it any earlier makes zsh read the user's files directly and
    // skip ours (the kitty/ghostty pattern). Each of our files chains the
    // user's counterpart with ZDOTDIR temporarily restored.

    // .zshenv (every zsh): chain the user's, then re-point ZDOTDIR at us for
    // interactive shells only — scripts keep the user's env untouched.
    let zshenv = format!(
        "export ZDOTDIR=\"${{EMBER_ZDOTDIR_ORIG:-{orig}}}\"\n\
         [ -f \"$ZDOTDIR/.zshenv\" ] && source \"$ZDOTDIR/.zshenv\"\n\
         export EMBER_ZDOTDIR_ORIG=\"$ZDOTDIR\"\n\
         [[ -o interactive ]] && export ZDOTDIR={ours:?}\n\
         true\n"
    );
    std::fs::write(dir.join(".zshenv"), zshenv)?;

    // .zprofile (login shells, e.g. launched from Finder): chain the user's —
    // this is where Homebrew PATH etc. comes from — then point back at us.
    let zprofile = format!(
        "export ZDOTDIR=\"$EMBER_ZDOTDIR_ORIG\"\n\
         [ -f \"$ZDOTDIR/.zprofile\" ] && source \"$ZDOTDIR/.zprofile\"\n\
         export ZDOTDIR={ours:?}\n\
         true\n"
    );
    std::fs::write(dir.join(".zprofile"), zprofile)?;

    // .zshrc: final restore (so .zlogin + subshells see the user's ZDOTDIR),
    // chain the user's .zshrc, then install the hooks.
    let zshrc = format!(
        "export ZDOTDIR=\"$EMBER_ZDOTDIR_ORIG\"\n\
         unset EMBER_ZDOTDIR_ORIG\n\
         [ -f \"$ZDOTDIR/.zshrc\" ] && source \"$ZDOTDIR/.zshrc\"\n{HOOKS_ZSH}"
    );
    std::fs::write(dir.join(".zshrc"), zshrc)?;

    Ok(Injection {
        env: vec![
            ("EMBER_ZDOTDIR_ORIG".into(), orig),
            ("ZDOTDIR".into(), ours),
        ],
        args: Vec::new(),
    })
}

const RCFILE_BASH_HEAD: &str = r#"[ -f "$HOME/.bashrc" ] && source "$HOME/.bashrc"
_ember_precmd() {
  local ret=$?
  printf '\e]133;D;%s\e\\' "$ret"
  printf '\e]133;A\e\\'
}
case "$PROMPT_COMMAND" in
  *_ember_precmd*) ;;
  *) PROMPT_COMMAND="_ember_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}" ;;
esac
trap 'printf "\e]133;C\e\\"' DEBUG
"#;

fn prepare_bash(dir: &Path) -> std::io::Result<Injection> {
    create_private_dir(dir)?;
    let rc = dir.join("ember-bash-rc");
    std::fs::write(&rc, RCFILE_BASH_HEAD)?;
    Ok(Injection {
        env: Vec::new(),
        args: vec!["--rcfile".into(), rc.to_string_lossy().into_owned()],
    })
}

/// The per-user integration dir. The shell SOURCES files from here, so on a
/// shared /tmp (Linux; macOS's $TMPDIR is already per-user) a fixed name is a
/// pre-squat target: another local user creates it first and their rc runs in
/// your shell. Prefer $XDG_RUNTIME_DIR (per-user, 0700 by contract); else suffix
/// the temp path with the uid. `prepare` additionally verifies ownership.
pub fn integration_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("ember-shell-integration-{}", process_uid()))
}

/// This process's uid, for dir names + ownership checks.
#[cfg(unix)]
#[allow(unsafe_code)] // getuid is unconditionally safe; std exposes no wrapper
fn process_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn process_uid() -> u32 {
    0
}

/// Create `dir` owner-only and confirm it is actually OURS — `create_dir_all`
/// happily accepts a pre-existing attacker-owned dir on shared /tmp.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::metadata(dir)?;
    if meta.uid() != process_uid() {
        return Err(std::io::Error::other(format!(
            "{} is owned by uid {}, not us — refusing shell integration",
            dir.display(),
            meta.uid()
        )));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_writes_chaining_files_and_env() {
        let dir = std::env::temp_dir().join(format!("ember-si-test-{}", std::process::id()));
        let inj = prepare("/bin/zsh", &dir);
        assert!(dir.join(".zshrc").exists());
        assert!(dir.join(".zshenv").exists());
        // ZDOTDIR points at our dir; the original is preserved for chaining.
        assert!(
            inj.env
                .iter()
                .any(|(k, v)| k == "ZDOTDIR" && v == &dir.to_string_lossy())
        );
        assert!(inj.env.iter().any(|(k, _)| k == "EMBER_ZDOTDIR_ORIG"));
        let rc = std::fs::read_to_string(dir.join(".zshrc")).unwrap();
        assert!(rc.contains("source \"$ZDOTDIR/.zshrc\"")); // chains user config
        assert!(rc.contains("133;A")); // installs the marks
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_uses_rcfile_that_sources_user_bashrc() {
        let dir = std::env::temp_dir().join(format!("ember-si-bash-{}", std::process::id()));
        let inj = prepare("bash", &dir);
        assert_eq!(inj.args.first().map(String::as_str), Some("--rcfile"));
        let rc = std::fs::read_to_string(dir.join("ember-bash-rc")).unwrap();
        assert!(rc.contains("source \"$HOME/.bashrc\"")); // chains user config
        assert!(rc.contains("133;A"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_shell_is_noop() {
        let inj = prepare("fish", &std::env::temp_dir());
        assert!(inj.env.is_empty() && inj.args.is_empty());
    }

    /// Drive a REAL zsh through the injection and prove (a) ember's hooks
    /// install, (b) the user's own .zshrc still runs, (c) ZDOTDIR is restored —
    /// i.e. the "zsh re-evaluates ZDOTDIR per startup file" trap is handled.
    fn run_real_zsh(login: bool) -> Option<String> {
        if !std::path::Path::new("/bin/zsh").exists() {
            return None; // no zsh on this runner — skip
        }
        let base =
            std::env::temp_dir().join(format!("ember-si-e2e-{}-{login}", std::process::id()));
        let user = base.join("user");
        let ember = base.join("ember");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(".zshrc"), "echo USER-ZSHRC-RAN\n").unwrap();
        std::fs::write(user.join(".zprofile"), "echo USER-ZPROFILE-RAN\n").unwrap();

        // Compute the injection as if the user's ZDOTDIR were `user`.
        // (prepare() reads the env; emulate its output for a hermetic test.)
        let inj = {
            let _ = prepare_zsh(&ember).unwrap();
            Injection {
                env: vec![
                    ("EMBER_ZDOTDIR_ORIG".into(), user.to_string_lossy().into()),
                    ("ZDOTDIR".into(), ember.to_string_lossy().into()),
                ],
                args: Vec::new(),
            }
        };

        let mut cmd = std::process::Command::new("/bin/zsh");
        if login {
            cmd.arg("-l");
        }
        cmd.args([
            "-ic",
            "whence _ember_precmd >/dev/null && echo HOOKS-OK; echo ZD=$ZDOTDIR",
        ]);
        for (k, v) in &inj.env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let _ = std::fs::remove_dir_all(&base);
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    #[test]
    fn real_zsh_installs_hooks_and_chains_user_rc() {
        let Some(out) = run_real_zsh(false) else {
            return;
        };
        assert!(out.contains("USER-ZSHRC-RAN"), "user rc skipped: {out:?}");
        assert!(
            out.contains("HOOKS-OK"),
            "ember hooks not installed: {out:?}"
        );
        let zd = out
            .lines()
            .find_map(|l| l.strip_prefix("ZD="))
            .unwrap_or_default();
        assert!(
            zd.ends_with("/user"),
            "ZDOTDIR not restored to the user's dir: {out:?}"
        );
    }

    #[test]
    fn real_login_zsh_chains_zprofile_too() {
        let Some(out) = run_real_zsh(true) else {
            return;
        };
        assert!(
            out.contains("USER-ZPROFILE-RAN"),
            "user .zprofile skipped: {out:?}"
        );
        assert!(out.contains("USER-ZSHRC-RAN"), "user rc skipped: {out:?}");
        assert!(
            out.contains("HOOKS-OK"),
            "ember hooks not installed: {out:?}"
        );
    }
}
