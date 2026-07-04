#!/usr/bin/env bash
# Cross-terminal visual test for Box Drawing rendering (.x).
#
# Plain ANSI/UTF-8 output — no ember-specific tooling — so the SAME script
# can run in Ember, iTerm2, Ghostty, and Alacritty for a side-by-side look.
# It doesn't assert anything; it's for eyeballing (screenshot each terminal,
# compare seams/thickness/curves/dashes by hand).
#
#   scripts/box-drawing-visual-test.sh            # print everything at once
#   scripts/box-drawing-visual-test.sh --pause    # pause between sections
#
# What to look for in each section (see the label above it) is the same
# thing -2.7's acceptance criteria checked for automatically —
# this script just makes it visible to a human eyeball across terminals.

set -euo pipefail

PAUSE=0
for arg in "$@"; do
  case "$arg" in
    --pause) PAUSE=1 ;;
  esac
done

section() {
  printf '\n\033[1;33m== %s ==\033[0m\n' "$1"
  if [ -n "${2:-}" ]; then
    printf '\033[2m%s\033[0m\n' "$2"
  fi
  printf '\n'
}

pause() {
  if [ "$PAUSE" = 1 ]; then
    printf '\033[2m[press enter for the next section]\033[0m'
    read -r _
  fi
}

clear
printf '\033[1mBox Drawing visual test\033[0m — run this in Ember, iTerm2, Ghostty, and\n'
printf 'Alacritty and compare. Widen your window to at least 70 columns first.\n'
pause

# ---------------------------------------------------------------------------
section "1. Full range — U+2500..U+257F, 16 per row" \
        "Look for: every glyph distinct, nothing missing/blank/mojibake."
printf '─━│┃┄┅┆┇┈┉┊┋┌┍┎┏\n'
printf '┐┑┒┓└┕┖┗┘┙┚┛├┝┞┟\n'
printf '┠┡┢┣┤┥┦┧┨┩┪┫┬┭┮┯\n'
printf '┰┱┲┳┴┵┶┷┸┹┺┻┼┽┾┿\n'
printf '╀╁╂╃╄╅╆╇╈╉╊╋╌╍╎╏\n'
printf '═║╒╓╔╕╖╗╘╙╚╛╜╝╞╟\n'
printf '╠╡╢╣╤╥╦╧╨╩╪╫╬╭╮╯\n'
printf '╰╱╲╳╴╵╶╷╸╹╺╻╼╽╾╿\n'
pause

# ---------------------------------------------------------------------------
section "2. Light / heavy / double weight — corners, tees, crosses" \
        "Look for: heavy thicker than light; double = two clean rails, no gaps."
printf '┌─┬─┐   ┏━┳━┓   ╔═╦═╗\n'
printf '├─┼─┤   ┣━╋━┫   ╠═╬═╣\n'
printf '└─┴─┘   ┗━┻━┛   ╚═╩═╝\n'
pause

# ---------------------------------------------------------------------------
section "3. Dash patterns — double / triple / quadruple, light + heavy" \
        "Look for: even dash/gap rhythm, no lopsided spacing."
printf '╌╌╌╌╌╌╌╌╌╌╌╌  (double, light)\n'
printf '╍╍╍╍╍╍╍╍╍╍╍╍  (double, heavy)\n'
printf '┄┄┄┄┄┄┄┄┄┄┄┄  (triple, light)\n'
printf '┅┅┅┅┅┅┅┅┅┅┅┅  (triple, heavy)\n'
printf '┈┈┈┈┈┈┈┈┈┈┈┈  (quad, light)\n'
printf '┉┉┉┉┉┉┉┉┉┉┉┉  (quad, heavy)\n'
pause

# ---------------------------------------------------------------------------
section "4. Rounded corners" \
        "Look for: smooth curve, no kink where it meets the straight line."
printf '╭──┬──╮\n'
printf '│  │  │\n'
printf '├──┼──┤\n'
printf '│  │  │\n'
printf '╰──┴──╯\n'
pause

# ---------------------------------------------------------------------------
section "5. Diagonals" \
        "Look for: crisp AA lines meeting exactly at cell corners, no stair-stepping."
printf ' ╱╲  ╲╱  ╳\n'
printf ' ╲╱  ╱╲  ╳\n'
pause

# ---------------------------------------------------------------------------
section "6. SGR attrs — plain / bold / dim" \
        "Look for: bold visibly thicker, dim visibly darker, plain as baseline."
printf 'plain:  ──────┬──────\n'
printf 'bold:   \033[1m──────┬──────\033[0m\n'
printf 'dim:    \033[2m──────┬──────\033[0m\n'
pause

# ---------------------------------------------------------------------------
section "7. Markdown-style table (the original reported bug)" \
        "Look for: every border segment connects — no gaps at any T-junction or corner."
printf '┌────────┬───────┬────────┐\n'
printf '│ Name   │ Bead  │ Status │\n'
printf '├────────┼───────┼────────┤\n'
printf '│ an agent │ .2.7  │ ready  │\n'
printf '│ an agent │  │ open   │\n'
printf '└────────┴───────┴────────┘\n'
pause

# ---------------------------------------------------------------------------
section "8. Concealed text (SGR 8) mixed with a box character" \
        "Look for: the bracketed box char is BLANK, not a leftover artifact."
printf 'concealed: [\033[8m┌─┐\033[0m]   visible: [┌─┐]\n'
printf '\n'

printf '\033[1mDone.\033[0m Screenshot each terminal and compare sections 2 (thickness/\n'
printf 'junctions), 4 (rounded), 5 (diagonals), and 7 (seams) most closely —\n'
printf 'those are where renderers most often disagree or show gaps.\n'
