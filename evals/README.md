# Skill evals

Eval-driven iteration for the `codesearch-cli` and `codesearch-mcp` skills. The
goal is to measure whether an agent, given a skill, actually follows the runbook
— **recall memory first, then choose the right query** — instead of guessing
whether an edit to the SKILL.md helped.

This is the pragmatic alternative to RL for a prompt-only skill: a
measure → refine → re-measure loop with a real reward signal (a blind judge),
minus the gradient descent. See Anthropic's
[skill authoring best practices](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices)
and [improving skill-creator](https://claude.com/blog/improving-skill-creator-test-measure-and-refine-agent-skills).

## The cases

`skill-evals.jsonl` — one case per line. Each has an `id`, an `entry_mode`, the
`skills` under test, a `query` (the user's opening message), and an
`expected_behavior` rubric the judge scores against.

The set is deliberately diverse in **how a session begins**, because the runbook
must hold whether or not the user opens with a question:

| `entry_mode` | What it tests | Cases |
|---|---|---|
| `open-question` | Classic "where/what/how" — locate then understand | `open-question-locate`, `understand-symbol-explain`, `cross-service-question` |
| `goal-task` | Session opens with a task, not a question. Must still recall memory first, *then* decide what to query (broad `overview` vs. a specific `search`/`context`) | `goal-task-first-change`, `goal-task-known-symbol`, `orient-new-repo-overview`, `mcp-goal-task`, `negative-known-symbol-no-search` |
| `resume` | "What did recent sessions do / where do we pick up?" — recall recent agent-loop context from memory before touching code | `resume-recent-sessions`, `resume-specific-thread`, `mcp-resume-overview` |
| `unrelated` | The skill must **not** over-trigger on an off-topic prompt (description tuning) | `trigger-should-not-fire` |

Two behaviours recur across the rubrics and are the crux of the runbook:

1. **Memory-first, unconditionally.** A concrete task or goal is *not* an excuse
   to skip Phase 1. `goal-task-*` cases exist specifically to catch a skill that
   only recalls memory for open questions.
2. **Then choose the query deliberately.** With no known symbol → `search` (or
   `overview` to orient). With a known symbol → straight to `context`/`impact`
   (or MCP `get_symbol_context`/`analyze_impact`), *not* a broad search and *not*
   grep. `goal-task-known-symbol` and `negative-known-symbol-no-search` are the
   negative controls for this.

## How to run the loop

There is no built-in eval runner (per Anthropic's docs, you bring your own).
The loop is the observe-refine-test cycle:

1. **Baseline.** For each case, run a fresh agent on the `query` **without** the
   skill loaded. Record what it did.
2. **With skill.** Run a fresh agent on the same `query` **with** the skill
   loaded (the matching one in `skills`).
3. **Judge, blind.** Have a separate judge agent score each run against
   `expected_behavior` (met / partially / not met) without knowing which run had
   the skill. Diff the two.
4. **Refine.** Where the skill run fails a rubric item, edit the SKILL.md (make a
   rule more prominent, sharpen the description to fix triggering, reorder a
   phase) — one change at a time.
5. **Re-measure.** Re-run the affected cases and confirm the score moved the
   right way before keeping the edit.

Track pass rate, tokens, and turns per run so a "fix" that doubles token cost is
visible.

### Judge prompt (starting point)

> You are grading whether an agent followed the codesearch skill's runbook.
> Given the user query, the agent's transcript, and a rubric of expected
> behaviours, mark each rubric item **met / partial / not-met** with a one-line
> reason. Judge only what the transcript shows; do not assume unstated steps.
> Pay special attention to: (a) whether memory was recalled *before* any code
> query, and (b) whether the agent chose the right query for what it knew —
> broad orientation vs. a specific symbol lookup vs. not searching at all.

## Adding cases

Keep new cases realistic (mirror actual requests), give each a clear
`entry_mode`, and write rubric items as **observable actions** ("calls
`read_memory` before searching"), not vibes ("understands the codebase"). Add at
least one negative control per behaviour you care about.
