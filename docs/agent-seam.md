# Agent seam (disabled in v0)

Filecraft reserves an `agent` command so a future opt-in assistant has a
stable place to live. v0 ships **no agent**.

> **Not the summarizer.** `:summarize` / `S` (`src/summarize.rs`) is a
> separate, shipped feature: the user marks a finite list of documents,
> picks a provider from a fixed table, and Filecraft spawns that CLI to
> write one new Markdown file. It has no autonomy - it does not choose
> files, does not act again after it finishes, and cannot be started
> without both explicit steps. The `agent` seam below stays disabled,
> and the summarizer does not enable it.

Typing `agent` (with or without extra words) prints a "not configured"
explanation and returns to the navigator. Nothing is scanned, indexed,
sent off the machine, or changed.

## What v0 guarantees

- The only implementation is `DisabledAgent` in `src/agent.rs`.
- `Agent::is_enabled` is always `false`. There is no environment
  variable, flag, or config file that turns an agent on.
- `agent` never produces a filesystem or process effect. It does not
  invoke an LLM, spawn a helper, or walk the tree "for context".
- Arguments after `agent` are echoed back as unused. They are not
  prompts and are not transmitted.

If you are reviewing a change that adds another `Agent` implementation,
treat it as a security-boundary change.

## Future contract

Any later, **explicitly opt-in** agent must honor all four of these
rules. Shipping a default-on agent, or one that acts without them, is a
breaking change of this contract.

### 1. Explicit file scope

The agent may see only files the user names: the current selection
and/or paths typed after `agent`. It must not walk the tree, read
dotfiles, or index the disk to "find context". Hidden files stay hidden
from the agent unless the user names them.

### 2. Preview / dry run

Every proposed mutation is shown first as a dry-run preview: the path,
the kind of change (edit, move, rename), and the resulting content or
destination. No write happens at the preview step.

### 3. User approval

Each action (or an explicitly listed batch) is applied only after the
user confirms it, using the same explicit `y/n` style as `move` and
`rename`. Cancel leaves the filesystem unchanged. There is no
autonomous loop that keeps acting after a single approval.

### 4. Auditable actions

Every applied action is recorded in a user-visible log: timestamp,
scope, operation, target path, and result (applied / rejected / failed).
The log is local. v0 still has no network, telemetry, or background
daemon, and a future agent does not add those by default.

## Opt-in (not implemented)

When an agent exists, enabling it will be an affirmative step (a
documented flag or config value, default off). Absence of that step
must keep today's disabled behavior.
