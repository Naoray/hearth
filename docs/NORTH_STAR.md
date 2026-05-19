# Hearth — North Star

## Mission
Build the best free, MIT-licensed, daemon-driven Laravel development environment for solo Mac developers and AI coding agents — one binary, no friction, no telemetry.

## Target users / adopters
- **Primary:** Solo Laravel devs on macOS who want a scriptable, agent-friendly local stack with sane defaults and zero ongoing cost.
- **Secondary:** Teams who want the same dev stack reproducible across machines via Homebrew + `hearth install`.
- **Disqualified:** Linux/Windows-only devs (macOS-first), non-Laravel PHP devs.

## Non-goals
- **No GUI in v0.3.x.** Tauri GUI (PR #4) slides to Phase 5.
- **No XDebug toggle, no PHP profiler UI, no per-site Log Viewer in v0.3.x.** Out of scope until the CLI surface is rock-solid.
- **No paid tiers, no telemetry, no account system.** Ever.
- **No support for non-macOS hosts** until core is rock-solid on macOS.
- **No re-implementing Valet.** Hearth wraps Valet CLI; Valet stays a managed dependency.
- **No persistent privileged helper.** `sudo` only during `hearth install`.

## Hard constraints
- **v0.3.0 ship deadline:** 2026-05-26. A real ship date forces decisions and lets dogfooding inform v0.3.1.
- **License:** MIT, free forever, including databases.
- **Architecture invariants:** Process groups for all children (no orphans). Per-field `Arc<Mutex<T>>` in `DaemonState`. Lock order `config → php_manager → site_manager → supervisor`. Circuit breaker 3-in-60s.
- **Herd coexistence non-negotiable:** detect Herd and skip services it already owns. Hearth respects the user's existing setup.
- **CLI-first:** every feature must work without a GUI before any GUI work resumes.

## Decision principles (priority order)
1. **Ship the maintainer's daily workflow first.** Features the maintainer (and similar solo Laravel devs) use day-to-day land before edge-case features. *Why:* dogfooding is the only review that matters; if the maintainer doesn't use it, it isn't ready to ship.
2. **Ship beats polish, this sprint.** With a fixed v0.3.0 date, working CLI > pretty CLI > new CLI. *Why:* a real ship date is a forcing function; perfect features ship in v0.3.1.
3. **Daemon owns processes; CLI is thin.** No long-running work in `hearth-cli`. *Why:* single point of supervision is the architectural promise; breaking it invites orphan-process bugs.
4. **Wrap Valet, don't fork it.** When functionality exists in Valet/Herd, wrap their CLI/config; only re-implement when it blocks supervision or coexistence. *Why:* Valet's site/SSL plumbing is mature and free.
5. **Docs are part of the product.** README, CLAUDE.md, ROADMAP.md must agree on phase numbering at every release. *Why:* contradictory docs cost orientation and sprints.
6. **Agent-readable CLI is a first-class feature.** Stable flags, machine-parseable output where reasonable, and a Scribe skill that mirrors the human CLI. *Why:* agent integration is a primary value prop, not an add-on.

## Success signals
- v0.3.0 tagged on or before 2026-05-26 with `hearth add` (horizon/telescope/pulse/reverb) + `hearth db` (mysql/postgres/redis supervised).
- 0 orphan processes across 24h of dogfooding.
- README/CLAUDE.md/ROADMAP phase numbers identical.
- `cargo test` green, `./scripts/smoke-test.sh` green.
- Maintainer uses Hearth as their primary local Laravel stack for the full sprint without falling back to other tooling.

## Anti-signals (going wrong)
- Sprint scope creeps to include Tauri GUI or Log Viewer.
- A delegate edits a doc to disagree with another doc.
- Orchestrator starts coding instead of dispatching.
- We add a feature just to match another tool's checklist instead of because the maintainer needs it.
- `hearth install` regresses (the single hardest path to fix remotely).
