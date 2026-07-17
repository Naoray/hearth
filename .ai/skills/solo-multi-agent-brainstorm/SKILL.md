---
name: solo-multi-agent-brainstorm
description: Run a moderated panel discussion across multiple Solo MCP projects and AI agents. Spawns 3 panelists (personas, cross-project lenses, or hybrid), runs N rounds with synthesis between rounds, delivers final verdict. TRIGGER when user says "brainstorm with panel", "get multiple perspectives", "debate this", "/panel", or wants cross-project / multi-agent input on a decision. SKIP for quick questions, single-perspective tasks, time-critical work, or when superpowers:brainstorming alone is sufficient.
---

# Solo Multi-Agent Brainstorm — Moderated Panel Across Projects and Agents

Turn a 1:1 brainstorm into a small panel discussion. Each panelist runs as its own Solo MCP agent, optionally rooted in a different project (own codebase context) or running on a different AI runtime (Claude / Codex / Gemini). The orchestrator moderates: dispatches rounds, synthesizes, decides convergence, delivers verdict.

Built on `solo-orchestration` (mechanism layer). Read that first if unfamiliar — this skill assumes its push patterns and cross-project scoping rules.

## When To Use

| Situation | This skill | superpowers:brainstorming |
|-----------|-----------|---------------------------|
| Quick question with one likely answer | — | use this |
| Solo design exploration | — | use this |
| Decision spanning multiple repos / teams | **use this** | — |
| High-stakes architecture call with non-obvious tradeoffs | **use this** | — |
| Want different AI vendors weighing in | **use this** | — |
| Time-critical (<10 min) | — | use this |

Cost reality: 3 panelists × 3 rounds ≈ 9-12 LLM calls + orchestrator synthesis. Roughly 50k tokens panel-side for a real cross-project brainstorm. Reserve for decisions that justify that.

## Modes

| Mode | Panel composition | When |
|------|-------------------|------|
| `personas` | All panelists in orchestrator's project, distinct lens each | Single-repo design |
| `cross-project` | One panelist per relevant project, cross-codebase grounding | Decisions spanning system boundaries |
| `hybrid` (default) | Mix of personas + cross-project lenses | Most real decisions |

Mode is runtime decision based on topic. Default to `hybrid`.

## v1.2 Scope

In:
- All three modes
- Project selection: `list_projects()` → suggest → user confirm
- **Multi-vendor panel by default**: panelists can run on Claude, Codex, Gemini, Amp, OpenCode — whichever the host has wired. Vendor selection is delegated to `solo-orchestration` Multi-Vendor Preflight (no probing logic in this skill).
- Per-role runtime assignment via the matrix below (architect → strongest reasoning, contrarian → minority vendor for divergence, pragmatist → fast vendor)
- Echo-chamber warning when preflight returns <2 OK vendors
- Round protocol with synthesis + convergence detection
- Final verdict archived to orchestrator-project scratchpad
- Read-only enforcement + secret-leak guard in panelist seed
- Cross-project scoping per `solo-orchestration` rules
- Per-panelist idle-timer registration (one timer per panelist, scoped to its own project — see Cross-Project Timer Caveat below)

Deferred:
- `NEED_PANELIST` mid-flight requests (v2 Feature B — issue #2)
- Orchestrator context handoff (v3 — issue #3)
- Recursive sub-panelists (forbidden permanently)

## Vendor Assignment Matrix

The `solo-orchestration` Multi-Vendor Preflight returns a list of OK vendors. This skill maps roles to vendors using the matrix below. If a desired vendor is not in the OK list, fall back to the next entry in the row.

| Role | Preference (left → right fallback) | Reason |
|------|------------------------------------|--------|
| architect / long-term reasoning | Claude (Opus) → Claude (Sonnet) → Codex (gpt-5.5) → first OK | Strongest cross-file reasoning, lowest hallucination on architecture |
| pragmatist / ship-now | Claude (Haiku) → Codex → Gemini → first OK | Fast turnaround, cheap |
| contrarian | Force a vendor **different** from the majority. Pick first OK vendor whose `tool_type` ≠ majority. If no diversity available → assign Claude with persona "contrarian" + emit echo-chamber warning. | Cross-vendor disagreement is the main hedge against correlated bias |
| codebase-grounded (cross-project) | Claude → Codex → first OK | Strongest local-repo tooling integration |
| user-advocate / skeptic | Any OK vendor | Persona drives output more than vendor here |

Rules:
- Resolve the assignment after preflight, not before. Preflight is the source of truth.
- Skip any vendor with `status != OK`. Use the hint to inform the user once, then move on.
- If ≥2 distinct `tool_type` available → vendor mix is "real". If only one → mark panel as **single-vendor** in the verdict diversity assessment.
- Format-drift mitigation: enforce strict `POSITION / TOP_RISK / DELTA` template across all vendors. Validate parsing on every callback. Log + retry once if parse fails.

## Echo-Chamber Warning

Surfaced **upfront** (before spawn) and **archived** (in verdict Provenance & Usage block) when:
- Preflight returns `<2` OK vendors, OR
- All assigned panelists end up on the same `tool_type` after fallback resolution.

Wording (verbatim, do not paraphrase):

> **Echo chamber risk:** all panelists run on `{vendor}`. Convergence may overstate consensus. Recommended: run preflight again after wiring a second vendor, or accept and proceed with persona-only diversity.

User confirms before spawn.

## Defaults

| Knob | Default | Reason |
|------|---------|--------|
| Panel size | 3 | Enough for disagreement, small enough to quote each verbatim |
| MAX_ROUNDS | 3 | Real-world brainstorms converge in 3 rounds. Allow 4 only if convergence not detected after round 3 |
| Convergence | DELTA="no change" 2 rounds OR POSITION cluster | Stop when stable |
| Per-round response cap | 300 words | Token discipline |
| Synthesis cap | 500 words | Orchestrator memory bound |
| Round timeout | 10 min | Generous safety net |

## Persona Library (starting set)

```
PERSONAS = {
  "skeptic":     "find weakest assumption, demand evidence",
  "pragmatist":  "what ships this week, cost vs value",
  "architect":   "long-term shape, coupling, blast radius",
  "contrarian":  "argue opposite of consensus",
  "user-advocate": "what does the end user actually feel",
}
```

Pick 3 distinct lenses per brainstorm. Add domain personas as needed.

## Cross-Project Lens Selection

```
projects = list_projects()
# Score each project for topic relevance:
#   - Keyword match on project name/description
#   - User-provided context
# Present top 3-5 to user, mark suggested with [x]:
#
# Topic: "rename /users endpoint to /accounts"
# Suggested panel:
#   [x] artistfy-api      (backend-eye)
#   [x] artistfy-web      (frontend-eye)
#   [x] artistfy-docs     (docs-eye)
#   [ ] artistfy-infra    (low relevance)
# Confirm? [y/edit/cancel]
```

Spawn each confirmed panelist with `project_id=<that project>`. The agent boots inside that project's repo with full local tooling.

## Panelist Seed Prompt Template

Prepend to first `send_input` after `spawn_agent(agent_tool_id=..., project_id=PANEL_PROJECT_ID)` (preferred; `spawn_process(kind="agent", ...)` still works). Also prepend `agent_instructions` returned by spawn.

```
You are panelist {PANEL_NAME} (process {SPAWN_PID}) in a moderated brainstorm.
Orchestrator pid: {ORCH_PID}. Orchestrator project_id: {ORCH_PROJECT_ID}.
You represent: {PROJECT_OR_PERSONA_LABEL}
Your lens: {LENS_DESCRIPTION}

PROJECT CONTEXT HINTS:
- GitHub repo for this project: {GH_REPO_HINT_OR_"discover via `git remote -v`"}
- Default branch: {DEFAULT_BRANCH_OR_"discover via `git symbolic-ref refs/remotes/origin/HEAD`"}

CONSTRAINTS:
- READ-ONLY. You are a consultant, not implementer. Do NOT modify files in your project.
- Do NOT echo file contents verbatim in callbacks. Summarize patterns, not values. Never quote .env, secrets, credentials, tokens.
- You may NOT call spawn_agent or spawn_process. No sub-panelists. Reason within your own session only.
- Stay in character every round. Re-read your lens before responding.
- When making a factual claim about your project (file exists, function does X, dep is pinned to Y), CITE the file path or function name. Verify against actual code before asserting. If unsure, say "I believe..." not assert.
- Use ONLY scratchpad_write + send_input for the reporting contract below. IGNORE other Solo coordination tools (kv_set, lock_acquire, timer_set, todos) — they are not part of this protocol.

REPORTING CONTRACT (follow exactly):

Step 1 — Write your response to scratchpad in orchestrator's project. The `name` parameter MUST be the literal string below — do NOT substitute your own title or H1; the orchestrator looks you up by author name + round number, but consistent naming helps debugging:
  scratchpad_write(
    name="panel-{SPAWN_PID}-round-{N}",
    project_id={ORCH_PROJECT_ID},
    content=<full response body, ≤300 words, ending with the three required lines below>
  )

Required ending lines in your scratchpad content (exact format, exact labels):
  POSITION: <one line — your current stance>
  TOP_RISK: <one line — biggest risk you see>
  DELTA: <what changed your view since last round, or "initial">

Step 2 — Signal completion via send_input:
  send_input(
    process_id={ORCH_PID},
    project_id={ORCH_PROJECT_ID},
    input="PANEL {SPAWN_PID} ROUND {N} DONE"
  )

ALWAYS pass project_id on cross-project send_input and scratchpad calls — your default scope ({YOUR_PROJECT_ID}) differs from orchestrator's ({ORCH_PROJECT_ID}).
Do NOT send_input for intermediate progress within a round.
```

## Round Prompt Template

Sent to each panelist at the start of round N (after seed):

```
TOPIC: {TOPIC}
ROUND: {N} of {MAX_ROUNDS}

PRIOR ROUNDS SYNTHESIS:
{ORCHESTRATOR_SYNTHESIS_OR_"initial round"}

YOUR LENS (re-stated): {LENS_DESCRIPTION}

Respond per the reporting contract from your seed prompt. Stay ≤300 words.
End with POSITION / TOP_RISK / DELTA lines.
```

Re-stating lens every round prevents persona drift.

## Orchestrator Recipe

```
# 1. Pre-flight
me = whoami()
ORCH_PID = me.process_id
ORCH_PROJECT_ID = me.project_id

# 1a. Prior-verdict housekeeping (default = ASK, never auto-archive)
prior = [p for p in scratchpad_list(project_id=ORCH_PROJECT_ID)
         if "brainstorm-verdict" in p.tags and not p.archived]
if prior:
    # Prompt verbatim:
    #   "{N} prior verdict scratchpads found ({pad ids + slugified names}).
    #    Archive them? [y/n/list]"
    ans = ask_user(prompt)
    if ans == "list":
        # show name + date per pad, then re-prompt y/n
        show_pads_with_dates(prior); ans = ask_user(prompt)
    if ans == "y":
        for p in prior:
            scratchpad_archive(scratchpad_id=p.id, project_id=ORCH_PROJECT_ID)
    # ans == "n" → leave alone. Do NOT ask again this run (no nag).

# Delegate vendor discovery + health check to solo-orchestration Multi-Vendor Preflight.
# Returns: [(tool_id, name, tool_type, status, hint)] — only status=="OK" is usable.
preflight = run_multi_vendor_preflight()  # see solo-orchestration SKILL.md
ok_vendors = [v for v in preflight if v.status == "OK"]
report_skipped_vendors_to_user([v for v in preflight if v.status != "OK"])

if len(ok_vendors) == 0:
    abort("No usable agent runtime. See preflight hints.")
if len({v.tool_type for v in ok_vendors}) < 2:
    confirm_echo_chamber_warning_with_user(ok_vendors)

# 2. Resolve panel composition
projects = list_projects()
panel_plan = build_panel(topic, mode, projects)  # see Cross-Project Lens Selection

# Assign vendor per role using the Vendor Assignment Matrix.
# entry.role drives the lookup; matrix returns the first OK vendor for that role.
for entry in panel_plan:
    entry.agent_tool_id = pick_vendor_for_role(entry.role, ok_vendors)
confirm_with_user(panel_plan)

# 3. Spawn panelists
panel = []  # list of (pid, project_id, label, lens, gh_repo_hint)
for entry in panel_plan:
    r = spawn_agent(
        agent_tool_id=entry.agent_tool_id,  # set by Vendor Assignment Matrix
        name=f"panelist-{entry.label}",
        project_id=entry.project_id,
    )
    # spawn_process(kind="agent", ...) is the equivalent older API; either works.
    seed = SEED_TEMPLATE.format(
        SPAWN_PID=r.process_id, ORCH_PID=ORCH_PID, ORCH_PROJECT_ID=ORCH_PROJECT_ID,
        YOUR_PROJECT_ID=entry.project_id,
        PANEL_NAME=entry.label, PROJECT_OR_PERSONA_LABEL=entry.label,
        LENS_DESCRIPTION=entry.lens,
        GH_REPO_HINT_OR_DISCOVER=entry.gh_repo_hint or "discover via `git remote -v`",
        DEFAULT_BRANCH_OR_DISCOVER=entry.default_branch or "discover via `git symbolic-ref refs/remotes/origin/HEAD`",
    )
    send_input(r.process_id, r.agent_instructions + "\n\n" + seed)
    panel.append((r.process_id, entry.project_id, entry.label, entry.lens, entry.gh_repo_hint))

# 3b. VERIFY seed delivery (send_input bytes-sent != delivered to fresh agent — see QA finding #6)
# Wait briefly, then check each panelist's terminal shows the seed prompt rendered.
sleep(2)
for pid, panel_project_id, label, _, _ in panel:
    out = get_process_output(pid, project_id=panel_project_id, lines=20)
    if "REPORTING CONTRACT" not in out:
        # Seed didn't land. Re-send with same content.
        re_send_seed(pid, panel_project_id, label)

# 4. Round loop
synthesis = "initial round"
synthesis_pads = []  # [(round_n, scratchpad_id, name)] — one per round, see v1.4
for round_n in range(1, MAX_ROUNDS + 1):
    # Dispatch round prompt to each panelist (in their project scope)
    for pid, panel_project_id, label, lens, _ in panel:
        send_input(
            pid,
            project_id=panel_project_id,
            input=ROUND_TEMPLATE.format(
                TOPIC=topic, N=round_n, MAX_ROUNDS=MAX_ROUNDS,
                ORCHESTRATOR_SYNTHESIS_OR_INITIAL=synthesis,
                LENS_DESCRIPTION=lens,
            ),
        )

    # Wait fan-in (Pattern C: send_input + idle-timer safety net).
    # CROSS-PROJECT CAVEAT: timer_fire_when_idle_all CANNOT watch pids across
    # multiple projects in a single call — it errors with "Process ID not found
    # in the requested project scope". Register ONE timer per panelist scoped
    # to its own project_id.
    for pid, panel_project_id, label, _, _ in panel:
        timer_fire_when_idle_all(
            processes=[pid],
            project_id=panel_project_id,
            max_wait_ms=10 * 60 * 1000,
            body=f"Panel {pid} ({label}) round {round_n} idle.",
        )
    # Then orchestrator goes quiet. Wakes on either:
    #   (a) panelist send_input callback (Pattern B — fast path), OR
    #   (b) idle-timer fire (Pattern A safety net — may be stale if callback
    #       already arrived; just verify scratchpad and continue).
    #
    # OPTIONAL Pattern D rescue: if a panelist commonly stalls past its expected
    # round duration (e.g., Codex swallows submit on cold runs), schedule a
    # one-shot timer with delivery_process_id=PANEL_PID and a plain-English nudge
    # body. The nudge is injected INTO the panelist as a fresh user turn,
    # asking it to either finish + callback, or write STALLED to its scratchpad.
    # Do NOT use Pattern D as the primary wake — Pattern B+A still happy path.

    # On wake: discover scratchpads (panelists may name them via their own H1
    # rather than the literal `name=` parameter — see QA finding #5; lookup by
    # author actor name + round number is the reliable path).
    pads = scratchpad_list(project_id=ORCH_PROJECT_ID)
    responses = []
    for pid, _, label, _, _ in panel:
        pad = find_pad_for(pads, author_actor=f"panelist-{label}", round_n=round_n)
        content = scratchpad_read(scratchpad_id=pad.id, project_id=ORCH_PROJECT_ID, content_only=True)
        responses.append((label, content, parse_position_lines(content)))

    # Synthesize — quote POSITION/TOP_RISK lines verbatim, do NOT paraphrase away dissent
    synthesis = synthesize(topic, round_n, responses)  # ≤500 words

    # Persist synthesis AFTER EVERY round (including the final round whose
    # synthesis becomes the verdict). Deterministic name + tag = handoff/recall.
    # Cost: ~1-2k tokens per write × N rounds = N writes. Bounded, intentional.
    synth_pad_name = f"synthesis-round-{round_n}-{slugify(topic)}"
    sp = scratchpad_write(
        name=synth_pad_name,
        project_id=ORCH_PROJECT_ID,
        content=synthesis,
        tags=["brainstorm-synthesis"],
    )
    synthesis_pads.append((round_n, sp.scratchpad_id, synth_pad_name))

    # Convergence check
    if converged(responses, prior_synthesis):
        break

# 5. Deliver verdict
verdict = build_verdict(topic, panel_plan, synthesis, rounds_run=round_n)
present_to_user(verdict)

# 6. Archive verdict (lookup by id from list — name ≠ URL slug after Solo derives it from H1)
# tags=["brainstorm-verdict"] is REQUIRED: it is what step 1a discovers next run.
# The final round's synthesis pad already exists (written in the loop). The
# verdict is written separately even so — slight content duplication, but it
# gives clean artifact boundaries: synthesis-round-N = raw round output,
# brainstorm-verdict = the locked, formatted decision record.
scratchpad_write(
    name=f"brainstorm-{slugify(topic)}-{today}",
    project_id=ORCH_PROJECT_ID,
    content=verdict,
    tags=["brainstorm-verdict"],
)

# 7. Cleanup — archive ALL produced pads: panel round pads AND synthesis pads
for pid, panel_project_id, label, _, _ in panel:
    for pad in find_round_pads_for(pads, author_actor=f"panelist-{label}"):
        scratchpad_archive(scratchpad_id=pad.id, project_id=ORCH_PROJECT_ID)
    close_process(pid, project_id=panel_project_id)
for _, sp_id, _ in synthesis_pads:
    scratchpad_archive(scratchpad_id=sp_id, project_id=ORCH_PROJECT_ID)
```

## Cross-Project Timer Caveat

Confirmed in v1 live test: `timer_fire_when_idle_all([pid1, pid2, pid3])` errors with `"Process ID '<pid>' not found in the requested project scope"` whenever any pid lives outside the timer's project. There is no single-call multi-project fan-in primitive in Solo MCP today.

Workaround (used in the recipe above): register **one timer per panelist**, scoped to that panelist's own `project_id`. Each fires independently when its panelist idles. The orchestrator wakes once per panelist. Pair with Pattern B `send_input` callbacks for the fast path; ignore stale timer fires if scratchpad was already consumed.

## Stale Timer Fires Are Normal

When Pattern C is in effect (combo of `send_input` callback + idle-timer safety net), the orchestrator typically wakes via the `send_input` first, processes the result, and then receives the timer-fire wake-up afterwards. This is a **stale fire** — harmless, just check whether the scratchpad has already been consumed and move on. Do not re-process.

## Convergence Detection

Stop when ANY of:
- All panelists' DELTA = "initial" or "no change" for two consecutive rounds
- POSITION lines cluster (semantic overlap on outcome — orchestrator judgment)
- `round_n == MAX_ROUNDS`
- User interrupts with explicit verdict

Do not loop past convergence. Diminishing returns are real.

## Synthesis Discipline

Orchestrator is the bias bottleneck. To fight it:
- Quote each panelist's POSITION + TOP_RISK lines **verbatim** in the synthesis section
- Paraphrase only in the meta-summary, never in position attribution
- Note where synthesis is the orchestrator's own framing vs panelist words
- If a panelist disagrees strongly, surface that disagreement — do not smooth it away
- If a panelist named a failure mode no one else addressed, treat it as a finding, not noise — even if it's a 1-of-3 minority. Minority risk-spotting is the main reason to run a panel; the synthesizer must not average it out.
- Final verdict must show the spread, not just the consensus

The synthesizer is itself a single LLM. POSITION/TOP_RISK lines are quoted verbatim and are trustworthy. But the cross-panelist judgment layer — what converged, which dissent matters, what's "minor" — is one model's framing. Honest compression is the goal: compress the prose, never the disagreement. If unsure whether a dissent is signal, keep it.

## What User Sees

User does NOT watch panelist chatter live. User sees:
1. Setup confirmation (panel plan)
2. Per-round summary (one paragraph + POSITION lines per panelist)
3. Final verdict + decision rationale
4. Archive scratchpad name for later recall

Like meeting minutes, not the meeting itself.

## Verdict Template — Required Sections

Every archived verdict MUST include the following sections, in this order:

1. **TL;DR** — 3-7 bullets capturing the locked decisions
2. **Per-project / per-panelist breakdown** — concrete deliverables, ownership, sequence
3. **Cross-cutting agreements** (architecture decisions, policies, rules locked across panel)
4. **Open items for out-of-band resolution** — anything the panel could not lock
5. **Provenance** — pointers to round scratchpad ids, synthesis pads, QA log
6. **Provenance & Usage** — see template below

### Provenance & Usage block (mandatory)

```
## Provenance & Usage

**Brainstorm runtime:** solo-multi-agent-brainstorm v{SKILL_VERSION}
**Date:** {DATE}
**Rounds:** {N} (converged before MAX_ROUNDS={MAX_ROUNDS}) | (hit MAX_ROUNDS={MAX_ROUNDS}, no convergence)
**Total wall time:** ~{MINUTES} min

**Panel composition & runtime:**

| Panelist | pid | Project | Lens | Agent runtime | Model |
|----------|-----|---------|------|---------------|-------|
| {label}  | {pid} | {project_name} ({project_id}) | {lens} | {agent_tool_name} (id={agent_tool_id}) | {model_name_if_known} |
| ...      | ...   | ...                            | ...    | ...                                    | ...                  |

**Diversity assessment (panel only — does NOT cover the synthesizer):**
- Single-vendor panel → "**Echo chamber risk:** all N panelists ran on {vendor}. Convergence may overstate consensus."
- Multi-vendor panel → "**Vendor mix:** {list}. Diversity reduces correlated bias."

**Orchestrator / synthesizer:** {orchestrator_vendor} {model_name_if_known} (pid {ORCH_PID}, project {ORCH_PROJECT_NAME})

> Synthesizer is a single LLM. POSITION/TOP_RISK lines above are quoted verbatim; the convergence and dissent judgments are this one vendor's framing. The diversity assessment applies to the panel only — it does not extend to the synthesis layer. Re-runs wanting a true cross-check should swap the orchestrator vendor too.

**Token usage (estimated, order-of-magnitude only):**

| Component | Tokens (in+out, rough) |
|-----------|------------------------|
| Panelist {pid} ({rounds_run} rounds + seed{retry_note}) | ~{N}k |
| ... | ... |
| Orchestrator — synthesis + verdict | ~{N}k |
| **Total panel-side** | **~{N}k** |
| **Total session including orchestrator** | **~{N}k** |

Estimation method: bytes sent/received via Solo `send_input` + scratchpad write sizes ÷ ~4 chars/token. Solo MCP does not expose per-spawn token counts today; treat as order-of-magnitude. Record raw counts + model name + date — re-price from current rates if cost calculation needed.

**Available agent runtimes (not used this run):** {list_other_agents_from_list_agent_tools}

**Convergence trigger:** {one of: POSITION-line clustering | DELTA="no change" 2 rounds | MAX_ROUNDS hit | user interrupt}

**Synthesis pads (one per round, tag `brainstorm-synthesis`):**

| Round | Scratchpad name | Scratchpad id |
|-------|-----------------|---------------|
| 1     | synthesis-round-1-{slug} | {id} |
| ...   | ...             | ...   |
| {N}   | synthesis-round-{N}-{slug} (final → basis for this verdict) | {id} |
```

Why this is mandatory:
- **Trust calibration**: same verdict from 3 Claude vs 3 mixed-vendor panelists means very different things.
- **Reproducibility**: future re-runs need to know what model + version was used.
- **Cost accountability**: invisible cost = cost shock when budget hits.
- **Skill QA**: cross-run comparison ("multi-vendor panel converged in 2 rounds vs 3 for single-vendor") only possible with this data.
- **PII guard**: scan actor names + project labels before archive — they may include client/customer hints.

## Anti-Patterns

Do NOT:
- Spawn panelist without `project_id` — Solo will guess wrong
- Forget READ-ONLY constraint in seed — panelist may modify its project
- Forget secret-leak guard — panelist may quote .env contents in callback
- Allow panelist to call `spawn_agent` or `spawn_process` — recursion explosion
- Use `timer_set(loop=true)` for round dispatch — wastes cache
- Call `timer_fire_when_idle_all` with cross-project pids in one call — errors out; register one timer per panelist in its own project instead
- Trust `send_input`'s "bytes sent" response as proof of delivery — fresh agents may not consume the input; verify with `get_process_output` before relying on the seed
- Look up panelist scratchpads by `name=` parameter — Solo derives URL slug from the response's H1; lookup reliably by author actor name + round number via `scratchpad_list`
- Send round prompt before all panelists from prior round have completed — race
- Paraphrase POSITION lines in synthesis — bias drift
- Skip convergence check and loop to MAX_ROUNDS by default — wasted tokens
- Forget `close_process` per panelist + scratchpad archive on exit — pid + storage leak
- Mix orchestrator-project scratchpad writes from panelists without `project_id=ORCH_PROJECT_ID` — silently lost
- Spawn a panelist on a vendor that the preflight did not return as `OK` — Solo MCP tools may be missing in that runtime; panelist cannot follow the reporting contract
- Build your own vendor probe inside this skill — duplicates `solo-orchestration` Multi-Vendor Preflight; drift will silently diverge
- Trust the synthesis layer's neutrality. The synthesizer is a single LLM. POSITION lines are quoted verbatim, but cross-panelist judgments (what's the convergence? what dissent matters?) are still one model's framing. The panel's vendor diversity does NOT cover the synthesizer — name it in the verdict and read its convergence claims as one vendor's read.

## When To Skip This Skill

- Quick technical question with one right answer
- Solo design exploration where one perspective is enough → `superpowers:brainstorming`
- Time-critical decision (<10 min available)
- No Solo MCP available locally
- Only one project or one agent runtime registered AND topic doesn't benefit from persona panel
- User wants to think alone

## Related Skills

- `solo-orchestration` — push patterns, cross-project scoping. Read first.
- `superpowers:brainstorming` — solo brainstorm fallback
- `superpowers:writing-plans` — natural follow-up after panel reaches consensus

## Quick Reference

| Need | Action |
|------|--------|
| Discover panelist projects | `list_projects()` |
| Discover usable agent runtimes | Multi-Vendor Preflight in `solo-orchestration` (returns OK vendors only) |
| Pick vendor for a role | Vendor Assignment Matrix (this skill, above) |
| Spawn panelist in another project | `spawn_agent(agent_tool_id=entry.agent_tool_id, project_id=X)` (or `spawn_process(kind="agent", ...)`) |
| Verify seed delivered | `get_process_output(pid, project_id=panel_project_id, lines=20)`, look for "REPORTING CONTRACT" |
| Dispatch round | `send_input(pid, project_id=panel_project_id, input=ROUND_TEMPLATE)` |
| Wait for round complete | One `timer_fire_when_idle_all(processes=[pid], project_id=panel_project_id, ...)` per panelist |
| Discover round scratchpads | `scratchpad_list(project_id=ORCH_PROJECT_ID)` then filter by `updated_by_actor_name == f"panelist-{label}"` |
| Read panelist response | `scratchpad_read(scratchpad_id=pad.id, project_id=ORCH_PROJECT_ID, content_only=True)` |
| Persist round synthesis | `scratchpad_write(name="synthesis-round-{N}-{slug}", project_id=ORCH_PROJECT_ID, tags=["brainstorm-synthesis"])` every round |
| Archive verdict | `scratchpad_write(name="brainstorm-{slug}-{date}", project_id=ORCH_PROJECT_ID, tags=["brainstorm-verdict"])` |
| Housekeep prior verdicts | Pre-flight: list `brainstorm-verdict` pads, ASK user `[y/n/list]`, archive only on `y` |
| Tear down panelist | `scratchpad_archive(...)` per round pad **and** per synthesis pad + `close_process(pid, project_id=panel_project_id)` |

## Changelog

### v1.5 (2026-05-27, Solo API surface update)

- **spawn API**: switched call sites to `spawn_agent(agent_tool_id=..., project_id=...)`. `spawn_process(kind="agent", ...)` still works (generic equivalent) and remains documented for back-compat.
- **Panelist ban broadened**: seed prompt + anti-pattern now ban both `spawn_agent` AND `spawn_process` for sub-panelists.
- **Pattern D referenced**: `timer_set(delivery_process_id=PANEL_PID, ...)` available as optional stalled-panelist rescue in the round loop. Not primary wake — Pattern B+A still happy path. See `solo-orchestration` Pattern D for mechanism.
- **Explicit non-changes** (audited Solo API surface, judged not worth adopting):
  - `scratchpad_transfer` — skill writes directly to ORCH_PROJECT_ID; transfer adds steps without removing failure modes.
  - `scratchpad_append_section` — v1.4 explicitly chose multi-pad-per-round for clean artifact boundaries; reverting would lose that.
  - `scratchpad_find` / `scratchpad_tail` / `scratchpad_read(mode=...)` — current `list + filter by author_actor` + full read works; rename without semantic win.
  - `scratchpad_save_to_file` — tag + archive already supports verdict discovery.
  - Todos for panel tracking — overkill for N×N rounds; scratchpad model fits.
  - Prompt templates — moving inline `.format()` strings to Solo storage is storage-shape change without behavior change.
  - `setup_agent_integration` replacing Multi-Vendor Preflight config patcher — Preflight idempotent with .bak files; swap would add a Solo-side dependency without removing failure modes.
  - `agent_channels` — not user-facing tool; "channels" in coordination help refers to slim-ack contract on internal write responses, not a push primitive.

### v1.4 (2026-05-15, persist per-round synthesis pads — issue #7)

- **Synthesis written after every round**, not just held in orchestrator memory. Includes the final round (whose synthesis becomes the verdict).
- **Deterministic naming**: `synthesis-round-{N}-{topic-slug}`. Tag `brainstorm-synthesis` applied at write time.
- **Verdict Provenance** now enumerates all synthesis pad ids per round (new table).
- **Cleanup** archives synthesis pads too, not only panel round pads.
- **Cost note**: each synthesis write ~1-2k tokens; N rounds = N writes. Bounded and intentional — buys consistency + handoff/recall support.
- **Final-round behavior clarified**: write both the synthesis pad AND the verdict. Slight content duplication, but clean artifact boundaries (raw round synthesis vs locked decision record).
- See `versions/rev-5.md`.

### v1.3 (2026-05-15, optional prior-verdict housekeeping — issue #6)

- **Pre-flight step 1a** added: list scratchpads tagged `brainstorm-verdict` in orchestrator project.
- If any found, **ASK** the user: `"{N} prior verdict scratchpads found ({pad ids + slugified names}). Archive them? [y/n/list]"`. `list` shows name + date then re-prompts.
- **Default = ASK, never auto-archive.** Archive happens only on explicit `y`. `n` leaves them untouched.
- **No nag**: if the user declines, the skill does not ask again the same run.
- **Documented explicitly** that the verdict-write step uses `tags=["brainstorm-verdict"]` — that tag is what the pre-flight discovery depends on.
- See `versions/rev-4.md`.

### v1.2.1 (2026-05-15, synthesizer-bias risk documented — issue #10)

- **New anti-pattern**: "Trusting the synthesis layer's neutrality." The synthesizer is a single LLM; POSITION lines are verbatim but cross-panelist judgments are one model's framing.
- **Synthesis Discipline** now requires surfacing minority failure modes: a risk named by 1-of-3 is a finding, not noise. Goal is honest compression, not no compression.
- **Provenance & Usage** names the orchestrator/synthesizer vendor explicitly and states the diversity assessment covers the panel only, not the synthesis layer.
- See `versions/rev-3.1.md`.

### v1.2 (2026-05-05, multi-vendor enabled)

- **Multi-vendor brainstorm is now default**, not deferred. Panelists can run on Claude, Codex, Gemini, Amp, OpenCode — whichever the host has wired and probed OK.
- **Vendor discovery delegated** to `solo-orchestration` Multi-Vendor Preflight. This skill no longer probes runtimes itself.
- **Vendor Assignment Matrix** added: maps role (architect / pragmatist / contrarian / codebase-grounded / user-advocate) to a preference list of vendors with fallback.
- **Echo-chamber warning** surfaced upfront (before spawn) and archived in verdict, when preflight returns <2 OK vendors or all assigned panelists end up on the same `tool_type`.
- **Removed v1.1 hardcode** "prefer Claude as only runtime" — replaced with assignment matrix. Single-vendor still possible (echo-chamber warning will fire) but no longer the default.
- **Footgun moved upstream**: the "non-Claude runtimes lack `--dangerously-skip-permissions` so scratchpad writes stall" assumption was a symptom of missing Solo MCP wiring in those runtimes' configs, not a permissions issue. Preflight now patches the configs idempotently. See `solo-orchestration` Multi-Vendor Preflight + Vendor config registry.
- **Anti-Patterns**: added "spawn on non-OK vendor" + "build your own probe" entries.

### v1.1 (2026-04-25, post first live test)

Patches based on real brainstorm execution against gaze / gaze-laravel / Dashboard:

- **Cross-project timer fix**: `timer_fire_when_idle_all` cannot watch pids across projects in one call. Recipe now registers one timer per panelist in its own project. Added Cross-Project Timer Caveat section.
- **Seed-delivery verification**: `send_input` to fresh agent may silently fail (bytes-sent ≠ delivered). Recipe now adds `get_process_output` check after seed and re-sends on miss.
- **Scratchpad lookup model**: panelists override `name=` parameter via their own H1; URL slug derives from the H1 not the name. Recipe now looks up by `updated_by_actor_name` + round number via `scratchpad_list`. Stronger directive in seed prompt to use exact `name=` (best-effort) but recipe doesn't depend on it.
- **MAX_ROUNDS default**: lowered from 4 to 3. Real brainstorms converge in 3 rounds; 4 is padding. Override allowed when convergence not detected after 3.
- **Agent runtime preference**: prefer Claude (`tool_type=="claude"`) over first-available — only Claude reliably has `--dangerously-skip-permissions` so scratchpad writes don't stall.
- **Project context hints**: seed prompt now includes GH repo + default branch hints (with discovery fallback via `git remote -v` / `git symbolic-ref`).
- **Factual-claim discipline**: seed prompt now requires panelists to cite file path / function name when asserting facts about their project.
- **Tool ignore-list**: seed prompt now explicitly tells panelists to use only `scratchpad_write` + `send_input`, ignore other Solo coordination tools (`kv_set`, `lock_acquire`, `timer_set`, todos).
- **Stale timer fires documented**: Pattern C combo produces benign stale fires; orchestrator recipe + new section explain to ignore.
- **Anti-Patterns + Quick Reference**: updated to reflect all of the above.
- **Verdict Template — Provenance & Usage block**: every verdict now MUST document panel composition (per-panelist agent runtime + model), diversity assessment (echo-chamber warning for single-vendor), token usage estimate, available-but-unused runtimes, and convergence trigger. Trust calibration and reproducibility require it.

### v1.0 (2026-04-25)

Initial release. See `versions/rev-1.md`.
