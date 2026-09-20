# Plan

Where things stand as of 2026-09-20.

## Built

Works end to end and is tested live, against scratch projects and a fake
source:

- `anna chat` — a terminal conversation. A thread is `claude -p`, resumed per
  conversation.
- The broker — MCP over a unix socket per session, hand-rolled (no tokio, no
  rmcp), reached through socat. Which socket a call arrives on is who is
  calling.
- Hands — bubblewrap, an allowlist CONNECT proxy as the only way out, a
  profile holding the access token and no refresh token. `.git/config`,
  `.git/hooks` and `.git/modules` are read-only inside, and `.git` is its own
  mount, because the brain runs git in that folder later.
- The reviewer — the same sandbox with the project read-only. The thread gets
  its verdict and never the hand's own report. Three rejections tell the
  thread to change approach.
- Grants — `start_hand` can pass on named tools; tools that speak to people
  can't be granted.
- MCP servers added from outside (`anna mcp add/list/remove/prose`), their
  tool listing fingerprinted, results screened by the judge.
- The judge — Jev with a key, haiku without. No answer counts as a yes.
- The editor — scores against the style, haiku rewrites, two fidelity
  questions (does the rewrite invent anything, does it drop anything) because
  one question couldn't tell a good rewrite from one that lost a fact.
- `anna run` — sources as config, one generic poller, a dispatcher: backlog
  ignored on first look, unknown senders ignored, injections refused with a
  reply, one turn at a time per conversation.
- `anna setup`, `anna log -f`, cleanup of profiles left by dead processes,
  threads that die with Anna.

Not built, or not working yet:

- **Memory.** Threads launch through `katami claude` when katami is installed,
  but katami only supervises when it has a terminal, so headless threads get
  no memory today. katami needs to supervise over pipes. After that: `origin`
  and speaker in katami, then the hand-transcript pipeline with the Jev risk
  score.
- **Accounts.** `anna run` starts `ax auto-switch` next to itself, which
  covers threads and every new hand. Not run live, because it moves the real
  default login. Still missing: moving a long-running hand mid-session.
- **One binary.** katami and ax are used as installed commands for now.
- **The config as a ceiling.** `people` decides who is heard. Who may ask for
  what, the widest grant, and the per-task budget don't exist yet.
- **Real sources.** Anna has no account of her own anywhere, so no source has
  been configured against a real server.
- **Dependencies and Rust in the sandbox.** mise's tools are there now: its
  installs folder is bound read-only and on the hand's PATH, listed from the
  home directory so a hand's own `mise.toml` is never read on the host. What
  is still missing is anything that needs a registry — `bundle install`,
  `npm install`, `cargo build` — since a hand has no network, and Rust itself,
  which lives under cargo's and rustup's folders next to credentials. Likely
  answer: registries as something the proxy can be told to allow per hand.
- The in-flight table, the classifier-driven dispatcher (routing, model
  choice), and transcripts of hands being kept for memory.

## Decided

- Anna is her own project, built on top of katami (memory) and ax (Claude
  subscriptions). She ships as one binary: `anna memory …` and
  `anna claude account …` are katami's and ax's logic pulled in as libraries.
- Brain and hands. The brain is a dispatcher plus threads. A thread is one
  Claude Code session per conversation, with its own reviewer. A thread can
  run several hands.
- Hands are sandboxed, isolated, and throw-away. Their only way out is an MCP
  request to the broker, which the brain controls per hand.
- Hands never read memory. The thread writes a brief with enough context for
  the hand to do a good job.
- The thread's reviewer checks the hand's work and sends it back to clean up.
- What a hand learned reaches memory only through katami's reviewer.
- Threads share memory.
- Fully autonomous. No human input past `anna setup`. Nothing may wait on a
  person to approve, unblock, or decide.
- Prefer classifiers (Jev) over LLMs wherever the answer is a pick from a
  list. They are faster and cost almost nothing.
- Sources: Basecamp, Fizzy, HEY, GitHub.
- Anna is an agent, not a tool. She can fail, but she never answers with the
  equivalent of "tool call failed". When she hits a limit she tries another
  way, offers what she can do instead, or says what she tried, like a
  colleague would.
- The brain is not sandboxed. It creates and clones projects and connects to
  servers, and needs real access to do that.
- A log you can follow: `anna log -f`. No `anna stop` — ctrl+c on `anna run`
  is the way to stop her.
- Rust.

## Proposed, not yet confirmed

- **Setup config.** What `anna setup` writes: who Anna listens to, which
  projects and servers exist, who may ask for what, the widest grant a hand
  can get, and a per-task budget. It turns "is this person allowed to ask for
  this" into a lookup in code, so a thread never decides it from the message
  it just read. Anna can read the config and cannot rewrite it; the broker
  refuses any grant above it.
- **What protects an unsandboxed brain.** The config decides who can ask for
  server-level work; who sent a message comes from the source, never the text;
  the reviewer stays read-only; Jev scores each inbound message before a
  thread reads it. Past that, a convincing message from someone allowed to ask
  still gets acted on. That risk is accepted.
- **Limits change the approach, they don't end the task.** Three failed
  reviews means the thread rethinks: a different plan, a fresh hand, a bigger
  model, or a smaller task. A request the config rules out is handled in
  conversation. The per-task budget is the only hard stop, and running into it
  is reported as what she tried and where she got stuck.
- **Flagged memories are dropped and logged.** Nobody is on the other end of
  that one.
- **Stopping cleanly.** `anna run` kills every thread and hand on SIGINT and
  SIGTERM. Hands live in their own sandboxes and would otherwise outlive her.
  Covers `systemctl stop` on the machine she runs on too.
- **A table of what's in flight.** katami memory only updates after a review,
  which is too slow for two threads working on the same project at once. Anna
  keeps threads, tasks and grants in her own SQLite, and threads can ask what
  else is going on.

## The pieces

### Dispatcher

Plain code plus a classifier, no LLM. Polls the sources (polling beats
webhooks behind Tailscale), works out which conversation a message belongs to,
and wakes the thread that owns it with `claude --resume`, or starts a new one.
One thread per conversation, with a lock so it never runs twice at once;
different conversations run in parallel. Who sent a message comes from the
source's own data, never from the message text.

Anna gets her own account on each source, so assigning her work is a normal
assignment and the source's permissions limit what she sees.

### Thread

Plans the work, picks the project, writes the brief and the grant, starts
hands, and replies in the conversation. Reads and writes memory through
katami. People can answer its questions in the conversation like they would a
coworker's — that's task input, not an approval.

The reviewer is a separate sub-session with read-only access to the hand's
worktree. The thread only sees its verdict. A hand's diff and report are the
most likely place for an injection to come from.

### Hand

`claude -p` inside bubblewrap: an empty root with the project folder and a
throw-away Claude profile bound in, `--unshare-net`, `--strict-mcp-config`.
The broker's unix socket is bound in and `socat` exposes it on localhost, so
the hand can reach the Claude API and the broker's MCP endpoint and nothing
else. `systemd-run --scope` for CPU and memory limits. Clean-up rounds resume
the same session in the same sandbox; the sandbox is deleted when the thread
accepts the work or gives up. The transcript is copied out first.

A task too big for one hand is a sequence of hands. What carries over lives in
the repo, the project's status in memory, and the task table.

### Broker

One grant per hand, one MCP endpoint per hand, and the egress proxy that only
lets Claude API traffic out. It has no memory actions: I floated a
project-scoped memory search for hands, and the decision was to keep memory
out of them entirely.

Known gap: the hand's sandbox holds a real subscription token, and a granted
action (a comment, a commit message) could leak it. A proxy that adds the auth
header itself would fix that and allow switching accounts mid-session, but
whether Claude Code's subscription login works through one is unverified.

### Memory

```
hand transcript > haiku extracts > class rules > Jev risk score > store
```

- Class rules: a hand's observations may only be history, reference or
  decision, about its own project. No preferences, constraints, identity,
  people, status, skills, supersedes or retracts. Stored with origin `worker`
  and shown to threads as unverified.
- Jev scores each surviving entry: "if a more powerful agent believed this and
  acted on it, could it weaken security, destroy data, move credentials, or
  run code from somewhere the note names?" Above ~0.2 it is dropped.
- A transcript Jev flags as an attack contributes nothing to memory.

See `spikes/injection-detection/RESULTS.md`. Short version: class rules alone
let 7 of 30 poisoning attempts through, all fake "decisions"; the per-entry
risk score caught 15 of 15 with no false positives on 16 benign entries.
haiku as a detector gets steered by the text it's scoring; Jev can't be.

### Setup

`anna setup` walks through: Claude accounts, the sources and who may ask for
what, MCP servers, and three optional things —

- a Jev API key (see Judge)
- a prose style, stored in the config. The editor scores and rewrites outgoing
  text against it. Without one there is no style pass.
- a personality, which becomes the CLAUDE.md in Anna's own Claude profile, so
  every thread loads it. Hands don't get it; they keep the project's own
  CLAUDE.md.

### MCP servers

Integrations are not built in. `anna mcp add basecamp -- basecamp mcp`, same
shape as `claude mcp add`; `basecamp mcp` and `hey mcp` already exist, and
GitHub has an official server. Any other MCP server can be added the same way.

The broker is the only thing built in. It connects to every added server as a
client and re-exposes their tools to threads and hands, filtered by grant, so
no session ever talks to a server directly.

- The config marks which tools and arguments carry prose for people. Those go
  through the editor. `reply(text)` is Anna's own tool and maps to the right
  server tool for the conversation.
- Hands never post to people. Their words go to the thread; the thread speaks.
- Tool results are untrusted and get the same injection score as inbound
  messages. Tool descriptions are pinned when the server is added.
- Inbound stays built in: MCP has no usable "something new arrived", so each
  source has a small poller. A server without a poller means Anna can act
  there but nobody can talk to her there.

### Editor

Every outgoing text marked as prose, inside the broker: skip code blocks,
links and quotes > judge scores it against the style > if it fails, haiku
rewrites it with one rule, change no facts > judge checks the rewrite says the
same thing > post. If the meaning check fails, the text goes back to the
thread with specific notes. Rewrite before reject: a rejection loop spends the
thread's expensive context on restyling.

Untested: whether Jev or haiku can judge style at all.

### Judge

Everything that asks a classifier-shaped question — routing, injection score,
memory risk, outcome of a hand, style — goes through one judge with two
backends. Jev when `TYPESAFE_AI_API_KEY` is set, haiku through the Claude
subscription when it isn't. Anna works without Jev; she is slower and spends
quota on questions Jev answers for fractions of a cent.

haiku as a judge has one known weakness: on long inputs it sometimes answers
the text instead of scoring it. So the haiku backend wraps the input as data,
accepts only the exact JSON it asked for, and treats anything else as flagged.
On the memory risk score it caught 13 of 15 poisoned entries against Jev's 15,
with no false positives.

### Accounts

ax today only rotates the default `~/.claude` login; `ax run` profiles are
pinned to one account. Needed:

- pick the account with the most headroom when a session starts — covers hands
- move running profiles before they hit a limit — covers long-lived threads

### Where classifiers go

- Dispatcher: which conversation, which project, task or question or chatter,
  is this addressed to Anna. A "thanks" never wakes a thread.
- Model choice: haiku, sonnet or opus per hand by how hard the task is.
- Hand outcome: done, blocked, needs input, failed — from the free-text report.
- Broker, outbound: is this comment on topic for the task. Secrets get a regex
  first.
- Memory: the per-entry risk score above.
- Never: who is allowed to ask for what. That's a lookup in the setup config.

## Changes to katami and ax

- Both become library crates with a thin binary, so Anna can pull them in and
  they keep working as standalone tools.
- katami re-spawns itself as `current_exe() review …` (`src/reviewer.rs`), and
  probably does the same for hooks. Inside Anna `current_exe()` is Anna, so she
  has to expose the same hidden subcommands.
- katami: an `origin` on memories (owner, person, worker) with the class rules
  per origin, and a speaker on transcript turns — today "user turn" means
  the person she works for, and in Anna it can be a coworker whose words would otherwise become
  the person she works for's preferences.
- Two binaries on one memory store: pin Anna to the installed katami version,
  or let the schema-version check refuse the older one.
- ax: the two account changes above.

## Order

Done: the pre-build checks (`spikes/sandbox/RESULTS.md`), the core, hands, the
review loop, the broker, external MCP servers, the judge, the editor, the
poller and dispatcher, setup.

Next:

1. katami: supervise headless sessions, so threads get memory.
2. Anna's own accounts on Basecamp and HEY; configure them as sources and see
   what their watch tools really answer with.
3. The config as a ceiling: who may ask for what, the widest grant, a per-task
   budget.
4. Registries through the proxy, per hand; Rust inside the sandbox.
5. katami `origin` and speaker; hand transcripts into memory through the Jev
   risk score.
6. katami and ax as libraries, one binary; moving a running hand to another
   account.
7. Move to the machine she runs on. Then Fizzy and GitHub.

## Open questions

- What does the setup config look like, and who besides the person she works for can ask for
  what?
- How big is the per-task budget, and is it tokens, time, or both?
- Header-injecting proxy: does Claude Code's subscription login work through
  one?
- Jev thresholds were read off a small corpus written by Claude models, with
  one adaptive round. Worth redoing with attacks written by someone who can
  query the scorer.
