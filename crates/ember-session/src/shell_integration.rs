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
#[derive(Default)]
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
    std::fs::create_dir_all(dir)?;
    // The user's real ZDOTDIR (where their .zshrc lives).
    let orig = std::env::var("ZDOTDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_default();

    // .zshenv: restore ZDOTDIR + chain the user's .zshenv (runs for every zsh).
    let zshenv = format!(
        "export ZDOTDIR=\"${{EMBER_ZDOTDIR_ORIG:-{orig}}}\"\n\
         [ -f \"$ZDOTDIR/.zshenv\" ] && source \"$ZDOTDIR/.zshenv\"\n"
    );
    std::fs::write(dir.join(".zshenv"), zshenv)?;

    // .zshrc: chain the user's .zshrc, then install the hooks.
    let zshrc = format!(
        "[ -f \"$ZDOTDIR/.zshrc\" ] && source \"$ZDOTDIR/.zshrc\"\n{HOOKS_ZSH}"
    );
    std::fs::write(dir.join(".zshrc"), zshrc)?;

    Ok(Injection {
        env: vec![
            ("EMBER_ZDOTDIR_ORIG".into(), orig),
            ("ZDOTDIR".into(), dir.to_string_lossy().into_owned()),
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
    std::fs::create_dir_all(dir)?;
    let rc = dir.join("ember-bash-rc");
    std::fs::write(&rc, RCFILE_BASH_HEAD)?;
    Ok(Injection {
        env: Vec::new(),
        args: vec!["--rcfile".into(), rc.to_string_lossy().into_owned()],
    })
}

/// A per-run integration dir under the system temp dir.
pub fn integration_dir() -> PathBuf {
    std::env::temp_dir().join("ember-shell-integration")
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
        assert!(inj.env.iter().any(|(k, v)| k == "ZDOTDIR" && v == &dir.to_string_lossy()));
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
}
