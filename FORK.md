# Fork state (vimeh/opensessions)

Context for future-us: why this fork exists, what diverges from
`Ataraxy-Labs/opensessions`, and when it is safe to re-point the nixos-config
pin at upstream. Maintained on `feat/highlight-switch`; this file never goes
upstream (PR branches are separate cherry-picks off `main@origin`).

## How this fork is consumed

`nixos-config/packages/opensessions/package.nix` pins `rev` + `hash` to a
commit on `feat/highlight-switch`. Bump workflow: commit here, push to
`fork` (this repo's remote for vimeh/opensessions), `nix flake prefetch` for
the hash, re-pin, `nix build .#opensessions`, and re-run the switching E2E
trio against the store binaries before deploying (`just switch` + restart
opensessions).

## Divergence inventory (upstream `main`..`feat/highlight-switch`)

Grouped oldest-first; single commits listed where they stand alone.

1. **Remote tmux provider** — `39627017`, `089c5df2`
   Surfaces sessions from ssh-forwarded sockets (`ssh-tmux-nav`, see
   `nixos-config/modules/home/tmux.nix`) with a process-group deadline on
   remote commands. The fork's original reason. Not yet proposed upstream:
   needs a setup/docs story to be reviewable.

2. **Reap closed agents** — `54a09345` → upstream **PR #57** (open).

3. **Highlight-driven switching** — `c93ab146`, `6b98bb79`, `5ca9d108`,
   `2ae0de1d`, `aeb8f80e`, `c795aadd`, `69720e96`, `731456b0`
   The product-behavior fork: keyboard highlight switches the tmux client
   immediately (no Enter), coalesced by a 150 ms server-side gate
   (latest-wins + waker); navigation wraps at list edges; duplicate-sidebar
   spawns are deduped per window with a respawn guard; focus stays on the
   destination sidebar while browsing and `Enter` commits into the main pane
   (`SwitchSession.focusMain`, `#[serde(default)]` so protocol-compatible).
   Contract documented in `docs/explanation/sidebar-behavior.md` (§Session
   Switching Rules). NOT proposed upstream: it inverts upstream's
   browse-with-arrows/switch-on-Enter model, so it needs maintainer buy-in
   (likely config-gated) via an issue first.

4. **Pre-size destination window before switch-client** — `54a2c164`
   → upstream **PR #58** (open). Kills the attach-time sidebar
   balloon/repair double reflow under `window-size latest`.

5. **Shared capped debug log** — `6e5f71d5` → upstream **PR #59** (open;
   standalone version without the fork-only per-stage switch timings and
   presize instrumentation).

## When is it safe to follow upstream again?

Re-point the pin at upstream only when every row is satisfied (or its loss is
explicitly accepted):

| Fork surface | Safe when |
|---|---|
| Remote sessions (hermes etc. in the sidebar) | Remote provider series merged upstream, or we consciously drop remote sessions from the sidebar |
| Agents tab hygiene | PR #57 merged (or equivalent) |
| Highlight-driven switching + gate + sidebar-stay/Enter | Upstream ships switch-on-highlight semantics (or a config that reproduces them). Without this, upstream reverts us to Enter-driven switching — the single biggest UX regression risk |
| No switch-time layout flash | PR #58 merged (or equivalent presize/pre-layout) |
| Debug log usable under load | PR #59 merged (nice-to-have; absence only hurts diagnosability) |

Check PR state with: `gh pr list -R Ataraxy-Labs/opensessions --author vimeh --state all`.
Check residual divergence with: `jj git fetch --all-remotes && jj log -r 'main@origin..feat/highlight-switch'`.

## Re-pointing checklist (when the table above is green)

1. `jj git fetch --all-remotes`; confirm each remaining fork commit is
   upstream (merged PR or equivalent rework) — `jj log -r
   'main@origin..feat/highlight-switch'` should be empty or only
   accepted-loss commits plus this file.
2. Pin `packages/opensessions/package.nix` to the upstream rev (drop the
   fork comment), `nix build .#opensessions`.
3. Run the tmux E2E suite against the store binaries
   (`CARGO_BIN_EXE_opensessions-sidebar=… OPENSESSIONS_E2E_SERVER_BIN=…`),
   expecting the usual environmental failures only.
4. Live smoke after `just switch` + restart: `j`/`k` scan switches without
   Enter and without layout flash (`grep -a 'presize-switch\|width-repair'
   /tmp/opensessions-debug.log` stays quiet during a scan); `Enter` lands in
   the session's main pane; hermes remote sessions still listed; debug log
   stays under its cap.

## Rebase maintenance while the fork lives

As upstream merges PRs, rebase to shrink the stack:
`jj rebase -b feat/highlight-switch -d main@origin` after a fetch; merged
commits become empty and can be `jj abandon`ed. Keep this file's inventory in
sync when commits land or new divergence is added — future-us will trust it.
