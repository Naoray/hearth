# Hearth — North Star

## Mission
Be the free, MIT-licensed, daemon-driven Laravel dev environment that lets a solo Mac developer cancel their Herd Pro subscription without losing the workflow they actually use day-to-day.

## Target users / adopters
- **Primary:** Solo Laravel devs on macOS who currently pay for Herd Pro (or are about to) and want a free, scriptable, agent-friendly alternative.
- **Secondary:** Teams who want the same dev stack reproducible across machines via Homebrew + `hearth install`.
- **Disqualified:** Linux/Windows-only devs (macOS-first), non-Laravel PHP devs, anyone needing a polished consumer GUI in 2026Q2.

## Non-goals
- **No GUI in the renewal sprint.** Tauri GUI (PR #4) slides to Phase 5 (post-2026-05-26).
- **No XDebug toggle, no PHP profiler UI, no per-site Log Viewer in v0.3.x.** Herd Pro features the maintainer does not personally use daily are out of scope.
- **No paid tiers, no telemetry, no account system.** Ever.
- **No support for non-macOS hosts** until core is rock-solid on macOS.
- **No re-implementing Valet.** Hearth wraps Valet CLI; Valet stays a managed dependency.
- **No persistent privileged helper.** `sudo` only during `hearth install`.

## Hard constraints
- **Deadline:** Renewal hits 2026-05-26 07:50 UTC. v0.3.0 must ship and be dogfooded before then.
- **License:** MIT, free forever, including databases.
- **Architecture invariants:** Process groups for all children (no orphans). Per-field `Arc<Mutex<T>>` in `DaemonState`. Lock order `config → php_manager → site_manager → supervisor`. Circuit breaker 3-in-60s.
- **Herd coexistence non-negotiable:** detect Herd, skip nginx/php-fpm/dnsmasq when it owns them.
- **CLI-first:** every feature must work without a GUI before any GUI work resumes.

## Decision principles (priority order)
1. **Cancel-renewal beats everything else this week.** Anything that doesn't move the maintainer toward not pressing "Renew" on 2026-05-26 is deferred. *Why:* the deadline is real money + a forcing function for shipping vs. polishing.
2. **Ship the maintainer's daily workflow first, the catalog second.** Features the maintainer uses daily on Herd Pro ship before features other users might want. *Why:* dogfooding is the only review that matters in a 7-day window.
3. **Daemon owns processes; CLI is thin.** No long-running work in `hearth-cli`. *Why:* single point of supervision is the architectural promise; breaking it invites the orphan-process bugs we already solved.
4. **Wrap Valet, don't fork it.** When functionality exists in Valet/Herd, wrap their CLI/config; only re-implement when it blocks supervision or coexistence. *Why:* Valet's site/SSL plumbing is mature and free; rewriting it burns weeks.
5. **Docs are part of the product.** README, CLAUDE.md, ROADMAP.md must agree on phase numbering at every release. *Why:* contradictory docs have already produced an orphaned Phase 3 PR (#4); confused docs cost a sprint.
6. **Agent-readable CLI is a feature, not an afterthought.** Stable flags, machine-parseable output where reasonable, and a Scribe skill that mirrors the human CLI. *Why:* "agent integration" is a primary differentiator vs. Herd Pro.

## Success signals
- Maintainer cancels Herd Pro renewal on or before 2026-05-26.
- v0.3.0 tagged with: `hearth add` (horizon/telescope/pulse/reverb) + `hearth db` (mysql/postgres/redis supervised).
- 0 orphan processes across 24h of dogfooding.
- README/CLAUDE.md/ROADMAP phase numbers identical.
- `cargo test` green, smoke-test.sh green.

## Anti-signals (going wrong)
- Sprint scope creeps to include Tauri GUI or Log Viewer.
- A delegate edits a doc to disagree with another doc.
- Orchestrator (Claude Opus) starts coding instead of dispatching.
- We add a feature the maintainer doesn't use just because Herd Pro has it.
- `hearth install` regresses (it's the single hardest path to fix remotely).
