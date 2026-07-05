#!/usr/bin/env bash
# Build a sanitized demo HOME for documentation screenshots.
#
#   demo-env.sh <home-dir>
#
# The screenshots must never expose a real machine, so every shot runs against
# this throwaway environment: a neutral prompt, a small demo project with a
# fake git history, and shells that land in that project. Deterministic git
# dates keep the commit hashes (and therefore the images) stable across runs
# and across platforms, which is what makes the macOS/Linux parity check valid.
set -euo pipefail

HOMEDIR="${1:?usage: demo-env.sh <home-dir>}"
rm -rf "$HOMEDIR"
mkdir -p "$HOMEDIR/project/ember/src"

# Prompt: keep the public handle, use a branded host, in the ember palette.
# Defined for both zsh (macOS default) and bash (Linux container default) so the
# rendered prompt is identical on either platform.
cat > "$HOMEDIR/.zshrc" <<'ZRC'
cd ~/project/ember 2>/dev/null
PROMPT='%F{250}kingb%f@%F{208}ember%f:%F{74}~/project/ember%f %# '
ZRC
cat > "$HOMEDIR/.bashrc" <<'BRC'
cd ~/project/ember 2>/dev/null
PS1='\[\e[38;5;250m\]kingb\[\e[0m\]@\[\e[38;5;208m\]ember\[\e[0m\]:\[\e[38;5;74m\]~/project/ember\[\e[0m\] \$ '
BRC
cp "$HOMEDIR/.bashrc" "$HOMEDIR/.bash_profile"

# Demo project content.
cat > "$HOMEDIR/project/ember/README.md" <<'MD'
# ember
A native terminal emulator, built from scratch in Rust.
MD
cat > "$HOMEDIR/project/ember/Cargo.toml" <<'TOML'
[package]
name = "ember"
version = "0.1.0"
edition = "2021"
TOML
cat > "$HOMEDIR/project/ember/src/main.rs" <<'RS'
fn main() {
    println!("Where's iTerm2 for Linux? We wondered too.");
}
RS

# Demo git history: fake author, fixed dates => deterministic hashes.
cd "$HOMEDIR/project/ember"
export GIT_AUTHOR_NAME="Ada Lovelace" GIT_AUTHOR_EMAIL="ada@example.com"
export GIT_COMMITTER_NAME="Ada Lovelace" GIT_COMMITTER_EMAIL="ada@example.com"
git init -q -b main
_commit() {
  export GIT_AUTHOR_DATE="$1" GIT_COMMITTER_DATE="$1"
  git add -A
  git commit -q -m "$2"
}
printf 'fn main() {}\n' > src/main.rs
_commit "2026-01-02T09:00:00" "Scaffold the Cargo workspace"
printf 'pub mod render;\n\nfn main() {}\n' > src/main.rs
_commit "2026-01-03T11:20:00" "Wire up the render module"
mkdir -p src/render && printf '// gpu-backed cell renderer\n' > src/render/mod.rs
_commit "2026-01-04T14:05:00" "Add a wgpu-backed cell renderer"
printf '\n[profile.release]\nlto = true\n' >> Cargo.toml
_commit "2026-01-05T16:40:00" "Enable release LTO"

echo "demo env ready: $HOMEDIR"
