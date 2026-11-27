# Origin design

Origin is version control for repositories that many people, agents, CI
runners and review bots read and write at the same time. This document
describes the model the v2 prototype implements. Ideas planned for later are
listed at the end.

## Principles

- **Zero infrastructure for small projects.** A solo project needs the
  `origin` binary and a directory, nothing more: no server, no account, no
  database. The repository lives in `.origin/`. The user's identity and node
  key live in the user's config directory.
- **The same model at every scale.** A monorepo with a thousand agents adds
  machinery underneath, not different concepts.
- **Performance and reliability come first.** Every hot path has a
  benchmark, and every write survives a crash.
- **A small core of generic primitives.** Plain `origin` is the CLI and the
  server. Everything else (CI, review UIs, semantic diffs, integrations) is a
  plugin built on the same public API. The first plugin is `origin-ci`.
- **Order only what needs ordering.** The only things with an order are the
  landings on each target. Everything else is an unordered set of immutable
  records.

## Vocabulary

| Term | Meaning |
|---|---|
| **Snapshot** | An immutable tree of files, content-addressed with BLAKE3. |
| **Change** | One piece of work, such as "add retries to fetch". It keeps a stable id while it evolves. |
| **Revision** | One immutable version of a change: a snapshot, the base it was made on and a description. |
| **Statement** | An attributable claim about an immutable record: a CI result, a review, a conflict, an abandonment. Annotations are statements. |
| **Attachment** | The bytes of a revision's description or a statement's payload. It has its own hash domain, so it can never be confused with a file. |
| **Target** | A long-running, ordered line of history: `main`, `staging`, `release/v3`, a months-long overhaul, or `policy`. |
| **Acceptance** | One landing on a target: what landed, the resulting snapshot and the evidence relied on, signed by the target's authority. |
| **Authority** | The single node that orders a target's acceptances. |
| **Node** | One running copy of a repository with its own key: a laptop, an agent's machine, a CI runner or a hosted server. |
| **Principal** | Who acts: `user:<name>` or `node:<name>`. Policies also name `group:<name>`. |
| **Workspace** | A working copy. Several workspaces can share one repository's store. |

Everyday work happens in changes, not branches. A stack is a chain of changes,
each based on the one below. Targets replace branches only where a long-lived,
ordered history is wanted.

## Records

The shared state of a repository is a set of immutable, content-addressed
objects. Each encoding starts with a kind byte, and an object's id is the
BLAKE3 hash of the whole encoding, so two kinds can never share an id.
Encodings are canonical: little-endian integers, length-prefixed strings and
lists, and no trailing bytes.

| Kind | Holds |
|---|---|
| Blob | A file's content. |
| Tree | A directory: names, entry kinds and ids. |
| Revision | One version of a change. |
| Statement | A claim about another record. |
| Acceptance | One landing on a target. |
| Attachment | A description or a payload. |

Encodings in detail:

- A blob and an attachment are their content, as is. An attachment is a
  separate kind so that no record can carry a file's content as its
  attachment: someone who may read a record may not read every file.
- A tree is its entries.
- A revision, statement and acceptance are their fields in declaration order.
  Integers are little endian, ids are their raw bytes, names and paths are a
  `u32` length then UTF-8, lists are a `u32` count then the items, and options
  and enums are a tag byte then the value. An acceptance ends with a 64-byte
  ed25519 signature of everything before it.

```rust
/// One immutable version of a change.
struct Revision {
    change: ChangeId,               // random 128-bit id, stable for the change's life
    predecessors: Vec<RevisionId>,  // the revisions this one evolves from
    base: Base,                     // an acceptance on a target, or another revision
    deps: Vec<RevisionId>,          // reserved; stacks use `base` today
    snapshot: TreeId,
    description: AttachmentId,      // the intent, written for humans and agents
    author: Principal,
}

/// An attributable claim about another record.
struct Statement {
    subject: RecordId,              // a revision, an acceptance, a snapshot or a statement
    kind: Kind,                     // dot-separated: "ci.test", "review", "core.conflict", ...
    verdict: Option<Verdict>,       // Pass | Fail | Neutral: the only field core interprets
    issuer: Principal,
    inputs: Option<Inputs>,         // what the claim depended on (see promotion)
    payload: Option<AttachmentId>,  // free-form, defined by whoever owns the kind
    supersedes: Option<StatementId>,// only a statement from the same issuer
    promoted_from: Option<StatementId>, // set on a promotion: the original result
}

/// One landing on a target. The authority's ed25519 signature over the
/// encoding follows the body.
struct AcceptanceBody {
    target: TargetId,
    seq: u64,                       // 0 for a target's first acceptance
    prev: Option<AcceptanceId>,     // None exactly when seq is 0
    state: TreeId,                  // the target's snapshot after this landing
    landed: Vec<Source>,            // a revision, or another target's acceptance
    evidence: Vec<StatementId>,     // the statements the policy relied on
    policy: Option<AcceptanceId>,   // the policy in force; None only for policy's first
}
```

**Why no global log.** A single ordered log of every operation needs one
sequencer, which offline nodes can't reach. It also breaks partial
replication, because a filtered log no longer verifies. Logs per actor fork
whenever one key is used on two devices or a crashed agent restarts.
Unordered records avoid both problems. Writers never contend, offline work is
free, and a node can hold just the part of the repository it needs.

**What we accept in exchange.** No node can prove it holds *every* statement
about a revision. This doesn't matter in practice. A check that matters
eventually runs, and its result then reaches the nodes that care. An
acceptance always lists the evidence it relied on, so the reason for every
landing can be verified.

## Changes

The working copy is always on a revision of the current change. Every command
snapshots it first, so there is no `commit` and no staging area. `.origin` and
`.git` are reserved names, never stored.

A snapshot alone is a **draft**: the files as they are, which commands that
only read the working copy (`status`, `diff`, `show`, `log`) show without
recording anything. A command that shares or reshapes the change (`describe`,
`new`, `edit`, `push`, `land`, `pull`, `converge`, `resolve`, `abandon`,
`annotate`, `undo`, `workspace new`, a plugin) first records the draft as a
new revision of the change, if it changed. A change's history then holds the
points that mattered, not every command run while editing.

- **Starting and editing.** `origin new` starts a change, on the working copy
  or on a target with `--on`. `origin describe` updates the intent, and
  `origin edit <change>` moves the working copy to another change.
- **Revising.** Editing, rebasing or handing a change to another agent
  creates a new revision of the *same* change. Reviews, check results and
  history stay attached to the change across all of them.
- **Divergence.** Two revisions made from the same revision give a change two
  heads. This is a normal state, not an error. The node that notices it
  (locally after a save, the server on a push, a client on a fetch) issues a
  `core.divergent` statement, which fires an event. `origin converge <change>`
  merges every head into one revision. The landing queue never lands a
  divergent change.
- **Pushing.** When a node is online, a push first compares the change's
  heads with the remote's and refuses a stale change before uploading
  anything. The upload itself carries the heads it expects, and the server
  checks them atomically as it publishes, so two pushes can't fork a change.
- **Failed approaches** are kept, not deleted. `origin abandon --reason "..."`
  issues a `core.abandon` statement. The revision stays available, so the next
  agent can see what was tried and why it failed. Origin doesn't store agent
  transcripts. The descriptions of revisions and abandonments hold the useful
  part.

### Conflicts

When a merge (`converge`, `pull`, or a rebase in the landing queue) leaves
both sides' changes to the same file, the file gets conflict markers. The new
revision gets a `core.conflict` statement listing each conflicted path and
the exact content written there.

- A path is resolved once a later revision has different content there, or
  when the user runs `origin resolve <path>...` to accept it as it is.
- Every operation that makes a revision carries unresolved paths forward in
  its own `core.conflict`: recording a draft, `describe`, `new`, `converge`,
  `pull`, `undo` and `workspace new`.
- The statement is written before the revision, so nothing can find the
  revision without its conflicts, even after a crash.
- The landing queue refuses a revision with unresolved paths.
- Markers are only an editing aid. Origin never scans files for them.

### Behind

After a landing, the working copy of the workspace that ran it follows its
change. If the change landed, or the target already had it, the working copy
starts a new change on the target's tip, so later edits build on what the target
has. If the queue rebased the change, the working copy moves to the rebased
revision. If the working copy is on top of a change that landed, it is rebased
onto the tip.

Other workspaces don't move: they are behind their change until `origin pull`
moves them the same way, merging the edits made since. Where those edits
conflict, the files hold both versions between conflict markers, and a
`core.conflict` statement lists them. The working copy can't land until those
files change or `origin resolve` accepts them (see [Conflicts](#conflicts)).

A change can move on without the working copy. Its head may land from
another workspace, or the queue may rebase it. The working copy is then
*behind*:

- `status` and `log` say so, and suggest `origin pull`.
- Commands that build on the working copy refuse until then.
- Edits made meanwhile aren't recorded, so the change doesn't fork. The files
  are never touched.

`origin pull` is the explicit catch-up. It fetches first when there is a
remote, then moves the working copy:
- onto the landed tip, if the change landed;
- onto the queue's rebase, if the queue rebased it;
- onto the tip of the target below it, if what it was stacked on landed.

In every case it merges the edits made meanwhile, with conflict markers where
they collide. No other command checks files out on its own.

## Statements

Statements are the one mechanism behind every annotation. Core knows nothing
about kinds beyond the reserved `core.*` ones, which each have authors:

| Kind | Who may issue it | Meaning |
|---|---|---|
| `core.abandon` | the change's creator, or an admin | The change was given up; the payload says why. |
| `core.conflict` | the revision's author, or a node | Paths left conflicted, with the content written there. From a node, it blocks only that landing. |
| `core.divergent` | a node | The change has more than one head; the payload lists them. |

A change's creator is the author of its first revisions, who wrote the change,
not someone who took it over or rebased it. A `core.conflict` comes from a node
when a landing queue could not rebase the record. Reserved kinds can't be
promoted, and `origin annotate` refuses them.

Every other kind (`ci.test`, `review`, `semantic.diff`, `flaky`) belongs to
whoever issues it. Statements compose: a human can mark a failed `ci.test`
statement as `flaky`, because a statement's subject can be another
statement.

Rules:

- **Supersession.** An issuer may only supersede their own statements. A
  claim to supersede someone else's is ignored. Statements from different
  issuers never overwrite each other, so a review from Alice and one from Bob
  both stand. `origin annotate` supersedes the user's latest statement of the
  same kind about that revision, reading it and issuing the new one under a
  lock, so two run at once in one store never supersede the same one: the
  second supersedes the first. A statement issued elsewhere, in a clone or by
  a CI runner over the API, supersedes what that issuer saw, so two of one
  issuer's statements from different stores can still both supersede the
  same one. A statement is taken back by superseding it with one that has no
  verdict (`--withdraw`), which claims nothing: it never counts as evidence,
  and nothing promotes it.
- **Statements are permanent.** Reviews, check results and conflicts are
  history, and history is never deleted. Only bulky payloads, such as CI
  logs, can have a retention period, set per kind in the policy
  (`[retention] "ci.*" = "90d"`).
  - Age counts from when this node stored the statement.
  - `origin gc` drops expired payloads and keeps the statements: subject,
    kind, verdict, issuer and inputs.
  - Payloads of `core.*` statements are never dropped.
  - A payload stays while any statement whose retention has not ended refers
    to it, or while a revision's description is the same attachment (an empty
    description and an empty payload, for example).
  - `origin gc` waits for the writers holding objects, landings on `policy`
    among them, then reads the rules from `retention` and the index again.
    This way no writer refers to a payload deleted under it, and a policy
    landed in the meantime applies. If there are no rules, it returns without
    waiting or reading anything.
- **Every statement fires an event** with its subject, kind, verdict and
  issuer (see [Events](#events)).

A change that waits keeps the whole stack waiting, so a retry lands the rest
once that change can land. Landing another target works the same way, with
the source's tip merged from the newest acceptances both targets' histories
share, and the requirements applying to the merged snapshot.
- The statement a conflict issues lists the paths both sides changed. The
  change waits until a new revision of it merges.
- The submitter must have write access to every path the landing changes on
  the target, whatever it lands, so landing a revision made on an old
  acceptance, or another target, never reverts or deletes what they may not
  change.
- On `policy`, the policy is that of the acceptance before, never the one
  landing (`Policy::governing`). Each of the target's requirements needs a
  claim about the revision that lands. Results about other records with the
  same input digest are promoted to it first, and a valid promotion claims
  what its original does, as the original's issuer.
- Each target's queue lives in `queue/<target id>/`. It holds a `lock`, taken
  while landing so landings on one target never race, and a file for each
  waiting landing, `change-<id>` or `target-<target id>`, so waiting landings
  can be listed and retried.

### Promotion by input digest

A statement is always pinned to an exact record. Carrying a check result over
to a new revision never mutates the result. Instead, a node issues a new
statement with `promoted_from` pointing at the original.

A statement can declare what it depended on:

```rust
struct Inputs {
    paths: Vec<RepoPath>,  // e.g. ["src", "Cargo.toml", "Cargo.lock"]; "" is the whole snapshot
    definition: Digest,    // the check's command and configuration
    toolchain: Digest,     // e.g. the hash of `rustc -V` output
    external: bool,        // also depended on things outside the snapshot
}
```

Each path names a file or a directory and covers everything under it. The
input digest hashes what the snapshot stores at each path (absent, or the
entry's kind and id), plus `definition` and `toolchain`. Trees are already
content-addressed, so the digest costs one lookup per path, however large the
tree.

A statement is **promotable** when it declares inputs, isn't `external`,
isn't itself a promotion, isn't about another statement, and its kind isn't
reserved. Promotion happens on demand, never on every snapshot:
- `origin promote`;
- the landing queue, before it checks evidence;
- `POST /promote`;
- `origin-ci`, before it runs a check.

It copies the latest matching result from each issuer, unless that issuer
superseded it, and only results the caller can see, so a promotion never
copies a result, or its payload, that the caller cannot read. It skips a
group if the subject already has a result of that kind from that issuer,
direct or promoted: a result that ran on the subject wins. A statement has no
snapshot, so nothing is promoted to one.

A promotion counts as its original: the original's kind, verdict and issuer
are what policy checks. A result that ran there wins: once the record has a
statement of that kind from that issuer, a result or a withdrawal, the
promotion no longer counts, whichever came first. It is only valid if:
- its kind, verdict, inputs and payload equal the original's;
- its subject is on the original subject's target: a revision is on the
  target of the acceptance it's based on, directly or through the revisions
  below it, an acceptance on its own, and a snapshot on none;
- its subject's input digest equals the original subject's.

Results carry over within a target, never from one target to another,
whose checks may differ.

Nodes check this whenever a promotion is used or put.

So a docs-only change keeps its `lint` and `build` results, because neither
reads `docs`. `docs-test` runs again, because it does read `docs`. Nobody
writes rules about which changes are "docs-only".

Promotion is only as safe as the declared inputs. A check that also reads the
network or the environment still declares its paths, which remain useful
information, but sets `external`. It then runs again on every revision. Human
reviews usually declare no inputs, so they never promote automatically.

The index groups promotable statements by kind, issuer and declared inputs, and
caches each statement's input digest, computed from its subject's trees the
first time a search needs it. Promotion never runs as a side effect of reading.

## Targets and policy

Appending acceptances to a chain: the caller verifies the signatures, since
only it knows the target's authority. The op log records the target advancing
to each acceptance, even one the store already had, such as one fetched earlier
as another target's landing. Each acceptance's `target.advanced` event comes
from that entry.

A target is a hash-chained list of acceptances with one authority. Its tip is
the target's current snapshot. Only the authority writes to the chain, and
every acceptance names the one before it. Two acceptances with the same
predecessor are an error, never something to merge: appends are
compare-and-swap.

**Policy is a target too.** `policy` holds `policy.toml`, and changing
permissions or land rules is a landing on `policy`:
- Policy edits are therefore ordered, reviewed and audited like code, with no
  extra mechanism.
- Landing on `policy` needs the `admin` capability, so an agent can't grant
  itself rights.
- A landing on `policy` is judged by the policy *before* it, never by the one
  it brings. That way a revoked key can't restore itself.
- Every other acceptance names the policy acceptance in force when it landed.

```toml
# policy.toml, in the snapshot of the `policy` target

[nodes]
hosted = "<hex ed25519 public key>"

[groups]
maintainers = ["user:alice"]
runners     = ["node:ci-1", "node:ci-2"]

[targets.policy]
authority = "node:hosted"

[targets.main]
authority = "node:hosted"
require = [
  { kind = "ci.test", verdict = "pass", issuer = "group:runners" },
  { kind = "review",  verdict = "pass", issuer = "group:maintainers" },
]

[targets."release/v3"]
authority = "node:hosted"
require = [{ kind = "ci.test", verdict = "pass", issuer = "group:runners" }]

[retention]
"ci.*" = "90d"

[grants]
all                 = ["land *", "annotate *"]
"group:maintainers" = ["admin"]
"group:runners"     = ["annotate ci.*"]

[[paths]]
path   = "secrets"
grants = { all = "none", "group:maintainers" = "write" }

[[paths]]
path   = "checks.toml"
grants = { all = "read", "group:maintainers" = "write" }
```

The rules for this file:
- Every table is optional, and unknown keys are errors.
- An authority must be a node listed in `[nodes]` with its public key.
- A target without its own table uses `policy`'s authority and has no
  requirements.
- Without `[grants]`, everyone may land on every target except `policy` and
  issue any kind. Only the local owner is an admin.
- Every name a policy uses must be defined in it, so a typo never silently
  drops a requirement.
- With `[grants]`, each caller has only what it grants them. Only an `admin`
  may land on `policy`, so no one can grant themselves rights they lack. No one
  is `admin` unless the policy says so, except the node's own user on its
  command line.
- For a path, the most specific `[[paths]]` rule that mentions the caller
  decides, and paths no rule mentions them for are writable.

`origin init` creates `policy` and `main`. Your node is the authority, and
neither target has requirements, so a solo project lands instantly.
`origin repo create --admin <principal>` does the same on a server and names
a remote admin.

Common workflows:

- **Long-running work.** `origin target new overhaul --from main` creates a
  target, and changes land on `overhaul`. Catching up with `main` is
  `origin land --from main --onto overhaul`. The overhaul finishes with
  `origin land --from overhaul --onto main`.
- **Patching old releases.** A fix is one change.
  `origin land <change> --onto release/v3` rebases it into a new revision of
  the same change, so its history shows every target it reached.

## The landing queue

The service behind every API call, `OriginService`, lives as long as its
process. It keeps the record index open and catches it up with the op log
before each use, and wakes the event long-polls waiting on it whenever it
writes. Calls don't wait for each other: each takes one of the open indexes,
or opens a new one, up to `OriginService::MAX_INDEXES`; past that, calls wait
for one, since each index holds everything in memory. A call holding the push
lock may open one more, and never waits, as landings wait for that lock while
holding indexes; the pool keeps no more than the cap. Landings on one target
still run one at a time, under the target's queue lock.

Writers never move a target. They submit changes, and the target's authority
lands them in order. A stack lands bottom first, one acceptance per change,
and stops at the first one that waits.

1. **Refuse** what can never land as it is: divergent changes, abandoned
   changes, and revisions with unresolved conflicts.
2. **Rebase** the revision onto the target's tip with a three-way merge at
   path level, recursing into directories. A revision already based on the
   tip lands as it is. Any other becomes a new revision of the same change,
   authored by the queue's node, written only if the change's head hasn't
   moved meanwhile.
   - When one target lands on another, the merge base is found from both
     histories: the best common ancestors, merged recursively when there are
     several, as git's recursive strategy does. The merge is checked as a
     revision on the tip, authored by the queue's node, so results about the
     same merge on another target never count.
   - A landing that changes nothing is skipped: the target already has it.
     But a merge of another target's tip that changes nothing still lands,
     as an acceptance of the same state that needs no evidence, so later
     merges start from it, unless either tip is already in the other's
     history: then neither has anything the other lacks.
3. **On conflict,** issue a `core.conflict` statement and keep the landing
   blocked. Nothing fails, and landing again retries it.
4. **Check the policy** in force *now*:
   - this node must be the target's authority;
   - the submitter must be allowed to land there, and must be able to write
     every path the landing changes;
   - the required statements must be present, promoted from the same target
     where the input digest matches.
5. **If evidence is missing,** keep the landing waiting, durably, and request
   the checks (`check.requested` events). It resumes by itself when
   statements about its subject arrive, and when the server starts. A landing
   whose submitter's token has expired waits until it's landed again.
6. **Otherwise,** write the signed acceptance, listing what landed and the
   evidence used, and advance the target.

After a landing, the working copy that submitted it follows: a new change on
the tip if its change landed, or the queue's rebase if it was rebased.

For a small project the queue runs inside your own `origin` process. Later,
batching (one acceptance landing several changes) and speculative merge trains
raise a busy target's throughput without changing the model.

Details of a landing:

- Landing a change lands its stack: the head of each change below it that the
  target doesn't have yet, from the bottom up, each with its own acceptance. If
  a change waits, the queue keeps the whole stack waiting, so a retry lands the
  rest once that change can land.
- A revision made on the tip lands as it is; the queue's node rebases any other
  into a new revision of its change, on the tip. If the merge would leave the
  target unchanged, nothing lands.
- If both sides changed a path, a `core.conflict` statement about the revision
  lists the paths, and the change waits until a new revision of it merges.
- The submitter must have write access to every path the landing changes on the
  target, whatever it lands, so landing a revision made on an old acceptance, or
  another target, never reverts or deletes what they may not change. On
  `policy`, the policy is that of the acceptance before, never the one landing.
- Each of the target's requirements needs a claim about the revision that
  lands. Results about the target's revisions with the same input digest are
  promoted to it first, and a valid promotion claims what its original does, as
  the original's issuer.
- Landing another target merges the source's tip from the newest acceptances
  both targets' histories share. The requirements apply to the merge, a
  revision this node makes on the target's tip, as for a rebase, so results
  about the same merge on another target, whose checks may differ, never count.
- The queue of each target lives in `queue/<target id>/`: a `lock`, taken while
  landing so landings on one target never race, and a file for each waiting
  landing, `change-<id>` or `target-<target id>`, so waiting landings can be
  listed and retried.

## Events

Every node keeps a local **operation log** of what it applied and in what
order: records written, landings left waiting, working-copy moves, target
advances. It is never synced, and it serves three purposes:

- `origin undo` and `origin op log`. Undo only moves this workspace's working
  copy back; records and landings stay.
- Crash recovery, together with the `moving` marker (see
  [On disk](#on-disk)).
- The event stream.

Events are derived from the log when they're read:
- `statement.issued` fires for every statement this node stores, fetched and
  promoted ones included.
- `revision.created` fires for every revision.
- `target.advanced` fires when a target's head moves.
- `check.requested` fires once per missing requirement of a waiting landing.

Events are delivered at least once, and consumers deduplicate by record id.

- **Locally,** `origin watch [--kind ...] [--subject ...] [--since ...]` polls
  the op log for new entries. With a remote it follows the remote instead,
  unless given `--local`.
- **In server mode,** the server dispatches each event to the users
  subscribed to it, filtered by what each user may read. Subscribers
  long-poll: `GET /api/v1/repos/<repo>/events?cursor=...&wait=30s` answers
  as soon as there are events after the cursor, with a JSON batch and the
  next cursor.
  - Without a cursor, only new events come. `wait` is capped at 60 s.
  - A cursor the log no longer holds gets 410, and the client starts over.
  - So does a cursor from another op log (it carries a random generation),
    for example after the remote was replaced.

A cursor belongs to one node. Two nodes may deliver the same events in a
different order.

## Plugins

Plugins only use the public `OriginService` API, over HTTP or in-process. A
plugin can do nothing the API can't, which keeps the API complete.

- **Workers** are long-running processes with a scoped credential. They
  subscribe to events and issue statements. Examples: a CI runner, a review
  bot, a semantic differ.
- **Tools** are executables named `origin-<name>` on `PATH`. `origin <name>`
  replaces itself with them, as cargo and git do with their subcommands. Run
  in a working copy, it records the draft first and sets
  `ORIGIN_REPOSITORY`, `ORIGIN_REVISION` and `ORIGIN_REMOTE`.

Official plugins live in `apps/origin-<name>`. The first is `origin-ci`:
- **Tool mode:** `origin ci run [<rev>]` checks a revision as the user.
- **Worker mode:** `origin ci worker --remote <url>` answers `check.requested`
  with a token that can read the repository and annotate `ci.*`. It
  reconciles the waiting landings at start, every minute and after an
  outage, and outlives server restarts.

Each check runs in a fresh checkout and its own process group. Before running
a check, the plugin asks for a promotion and skips the check if a result
already covers the revision. Results carry the last 64 KiB of output as their
payload.

`origin-ci` reads the checks from `checks.toml`. A worker takes them from the
tip of the target being landed on, never from the candidate. The checks that
guard `main` are the ones already on `main`, so a submitter can't redefine a
required check by editing it in their change. An edit to `checks.toml` takes
effect once it has landed, after passing the checks in force. `origin ci run`
checks a revision with that revision's own `checks.toml`, since it issues
results as the user.

```toml
[checks.lint]
command   = "cargo clippy --workspace -- -D warnings"
inputs    = ["src", "Cargo.toml", "Cargo.lock"]
toolchain = "rustc -V"

[checks.docs-test]
command   = "cargo test --doc"
inputs    = ["src", "docs"]
toolchain = "rustc -V"

[checks.links]
command  = "lychee docs"
inputs   = ["docs"]
external = true          # reads the network: never promoted
```

- Each check's kind is `ci.<name>`.
- `checks.toml` itself is always one of a check's inputs, so editing a check
  runs it again.
- Protecting `checks.toml` with a `[[paths]]` rule, as in the example policy,
  keeps submitters from changing the checks at all.
- A change in a toolchain's output only reaches the queue's promotions through
  the inputs. Pin toolchains in an input file, such as `rust-toolchain.toml`.

The environment of a tool run in a working copy: `ORIGIN_REPOSITORY` is the
directory holding the repository's store (its main working copy's `.origin`),
which a remote on a local path accepts; `ORIGIN_REVISION` is the full id of the
revision the working copy is; `ORIGIN_REMOTE` is the repository's remote, if it
has one. Outside a working copy, none of them is set.

## Permissions and credentials

Path rules map a path to `none`, `read` or `write` for everyone, groups and
users. The most specific rule wins. Paths no rule mentions are writable by
signed-in callers and readable by anonymous ones.

Access is split into capabilities:

| Capability | Scope | Allows |
|---|---|---|
| `read` | path | reading content |
| `write` | path | proposing revisions that change the path |
| `annotate` | statement kind | issuing statements of that kind |
| `land` | target | submitting to the target's queue (`land *` excludes `policy`) |
| `admin` | — | landing on `policy` |

Capabilities are written `read src`, `write src`, `annotate ci.*`, `land main`,
`land *` and `admin`. Writing a path implies reading it. Changing the rules
always takes `admin`.

Reading:
- A caller who can read no path doesn't see the repository at all.
- A revision is visible when it changes a path the caller reads.
- Unreadable files keep their place in trees, but their content is never
  sent. A clone records the user's rights and leaves withheld paths out of the
  working copy.

Writing:
- Writes are attributed: a revision's author and a statement's issuer must be
  the caller. Anonymous callers never write.
- A revision's *effect* on a path is the pair (what its base holds there,
  what its snapshot holds there). On every path its writer can't write:
  - a first revision may have no effect;
  - a later revision must have exactly the effect of each of its
    predecessors;
  - a stacked revision must sit on the same change below as its
    predecessors;
  - unresolved conflicts must be carried forward exactly, none dropped and
    none added; a first revision lists only those of the revision it is on.

  So nobody can empty, revert or redirect another person's work without
  write access to it. Rebasing onto a newer tip still works, and so does
  handing a change to an agent with other rights, as long as they leave alone
  what they can't write. Heads that disagree on such paths are converged by
  someone who can write them.
- A change's history is fixed: a new first revision for a change that already
  exists is refused, and so is a predecessor from another change. Only the
  creator, who wrote the first revision, or an admin can abandon the change.
  An agent who takes a change over can't abandon it.
- A statement's subject must be visible to its issuer, and so must a
  promotion's original.

Records put:
- A statement, or a promotion, must be of a kind the caller may annotate,
  except for reserved kinds, whose authors the policy checks for every caller
  (`Policy::authorizes`).
- A caller sees the targets and acceptances, every revision that changes a
  path they read or changes nothing, and the statements about what they see.
- A record that carries an attachment the repository already has, as a
  description or a payload, must bring that attachment in its batch, so no
  one reads an attachment by naming its id. A promotion is exempt, since it
  carries its original's payload.
- The records of a batch are read before the store's, so a batch may hold
  a stack of revisions and statements about them. The caller sees the records their
  batch adds, but a record the store already had only if they saw it before.

Revisions put (landings check what they change on the target separately, in
the queue):
- **History.** A revision must extend its change's history. Only a new change
  gets a root (a revision with no predecessors), and a successor may only
  evolve from revisions of its own change that the caller sees. So no one
  makes a change diverge with a root of their own, or becomes one of the
  change's creators, who may abandon it.
- **Effects.** A revision's *effect* is each file it changes at paths its
  writer may not write, from its base's entry to its own. A root must have no
  effect. A successor must have exactly each predecessor's effect, and if it
  sits on another change, its predecessors must sit on that same change. It
  must also list, in its `core.conflict`, exactly its predecessors' unresolved
  conflicts at those paths; a root lists those of the revision it is on. Only
  a `core.conflict` may supersede a `core.conflict`, and once a revision's
  listing is stored, it may not change at those paths. So a successor never
  reverts, replaces or resolves someone else's edits, or adds a conflict,
  where its writer may not write, and never brings in a stack that would land
  with it. Rebasing onto a newer tip keeps the effect and the conflicts, so it
  is refused if the new base changed a conflicted entry. Someone who takes a
  change over edits only what they may write, and hands it on.
- **Content.** A revision may only hold content the caller supplies, or that
  a snapshot it refers to holds at the same path (`Store::unsupplied`), so no
  one reads a file by placing its id where they may read.

Tokens:
- Static tokens in the server configuration keep working.
- `origin token issue --scope ... --ttl ...` issues short-lived tokens with
  narrower scopes for agents and CI runners.
  - They are signed by the server's node key and verified without storage.
  - A scope only narrows its holder's current rights.
  - A token lives at most 7 days, and never longer than the token that issued
    it.
- A typical agent token carries `read /` and `write src` and nothing else.

Origin has no way to erase content from history. A secret that leaks into a
revision is rotated. Once anyone has fetched it, deleting it would change
nothing.

## Signing

Each node has an ed25519 key, kept outside the repository in
`~/.config/origin/node.key` (or `$ORIGIN_CONFIG_DIR`). Its public key is
published in the policy's `[nodes]`.

- Authorities sign the acceptances they write. A fetch verifies every
  acceptance against the key named by the policy that governs it, and only a
  policy on the verified `policy` chain counts.
- The node key also signs scoped tokens.
- Other records are attributed by the node that receives them, which has
  already authenticated the sender.

Signed statements from outside issuers, and identities separate from their
keys, come later.

## Sync

A repository has at most one remote, named in `.origin/remote` and shared by
its workspaces. A remote on a local path has no remote of its own, so it holds
every snapshot. A clone holds the chains of the targets it follows, the records
of the changes it knows, and only the snapshots it read. When a command needs a
snapshot the clone lacks, such as for a checkout, it fetches it from the remote,
skipping what the working copy already holds. Syncing never changes files under
a running workspace: `pull` is a fetch, then a move of the working copy.

`.origin/remote-cursor` keeps the remote's event cursor as of the last fetch,
and the generation of the op log it belongs to, so the next fetch only compares
the changes that moved since. Whenever this repository learns of a change, it
brings everything the remote has of it, so the changes it knows are complete as
of that cursor.

- **`clone`** fetches the chains of `policy` and one target (`main` by
  default), then that tip's snapshot. Other snapshots are fetched lazily, when
  a command needs them.
- **`push`** shares revisions and statements. It never lands anything:
  sharing work and accepting it are separate steps.
  - It refuses a stale change, and compares the expected heads atomically.
  - It sends a statement together with the statements it depends on.
  - It splits large transfers into parts of at most 64 MiB.
  - Every object it writes must be well formed, and everything it needs must
    be in the repository or the pack, of the right kind: a tree's entries
    (trees or blobs, as each entry says), a revision's snapshot (a tree),
    description (an attachment), base and dependencies, and a statement's
    subject and the statements it supersedes or promotes. So the repository
    never holds a record it cannot use.
  - A statement's payload may be missing, but if the repository has it, it
    must be an attachment; a `core.conflict`'s payload, its list of conflicts,
    must be present.
  - Acceptances are refused: only a target's authority writes them, by landing.
  - Blobs and trees are made durable before the records that refer to them, and
    statements before the revisions they are about. Promotions come last: each
    is checked against its original, once the original and both subjects are
    stored. A `core.divergent` statement is issued for each change the new
    revisions made diverge.
- **`fetch`** fetches each held target's chain after the last acceptance this
  node has, and verifies it. It then fetches the changes the remote's events
  say moved since the last fetch, and falls back to comparing every known
  change when it has no valid cursor.
- **`pull`** fetches, then catches the working copy up (see
  [Behind](#behind)).
- **`land`** against a remote pushes the stack, submits it to that remote
  authority's queue, then fetches the result.
- **`watch`** against a remote subscribes to its event stream.

Blobs and trees only ever leave a server through a snapshot walk. That walk
filters by read access, and is marked durably on the receiving side so an
interrupted transfer is detected and repaired. Records are fetched by id.

Syncing never overwrites files in a running workspace. A fetch only adds files
the user may newly read.

### Fetching chains

A fetch brings the chain of each target this node follows (those it holds and
those asked for), starting after the last acceptance it holds. Each acceptance
must follow the one before it, and must be signed by the authority that its
governing policy (`Policy::governing`) names:

- An acceptance of `policy` is governed by the acceptance before it. Only the
  first acceptance of `policy` is checked against itself.
- Any other acceptance is governed by the policy it names, which must be on
  this node's verified chain of `policy`.
- Acceptances this node only stores, such as a fetched revision's base, never
  govern anything.

Each acceptance comes with what it landed, and the records of those changes.
Fetched records are written before the chains that refer to them.

Then the fetch compares the records of changes with the remote's, by id. It
fetches the missing ones, the records they refer to, and the records of the
changes those belong to.
The changes it compares are those the remote's events name since the last
fetch (`Sync::moved`), so a fetch that brings nothing costs the same however
long the history. Only without a cursor, or once it expired, does it compare
every change this node knows.

Snapshots are fetched when something reads them, such as a checkout, so a
clone fetches a single snapshot, its target's tip. The remote withholds the
content of the paths the user may not read: their tree entries stay, and
nothing here reads them (`Store::with_withheld`).

### Pushing

A push shares revisions, with their snapshots, and the user's statements. It
never lands anything. Before uploading anything, it compares each change's heads with the
remote's, and refuses a change that is stale (it has a revision on the remote
that this node lacks) or that has a revision changing a path the user may not
write. The remote checks the heads again as it publishes the
revisions, so a push racing another is refused too. Revisions only go to a
remote that has the acceptance they are based on. Each object is sent after
the objects it needs: snapshots and attachments, then revisions, each with
its reserved statements, then the other statements. A statement that needs a
record the remote neither has nor receives stays here, and the push reports
it. So do the statements it supersedes, which the remote would otherwise take
without their replacement, and the statements that need those.

Transfers are bounded both ways: a push is split into requests of at most
`Pack::LIMIT` bytes, and a fetch's answers stop at that size, with the rest
asked for again (`Part`). Each request of a push expects the heads that the
requests before it left. A revision goes in the same request as its reserved
statements, such as its conflicts and its abandonment, and the other
statements about one record also share a request. So the remote never lands a
revision whose abandonment, or a result whose replacement, is still to come.

`put_records` writes the objects of a pack: snapshots, attachments, revisions
and statements.
- Each object must be well formed, and everything it needs must be in the
  repository or the pack, of the right kind: a tree's entries (trees or
  blobs, as each entry says), a revision's snapshot (a tree), description
  (an attachment), base and dependencies, and a statement's subject and the
  statements it supersedes or promotes. So the repository never holds a
  record it cannot use.
- A statement's payload may be missing, but if the repository has it, it must
  be an attachment; a `core.conflict`'s payload, its list of conflicts, must
  be present.
- Records are looked up in the pack before the repository, so a pack may hold
  a whole stack.
- Acceptances are refused: only a target's authority writes them, by landing.
  A statement of a reserved kind must come from whom `Policy::authorizes`
  names, whoever puts it. A caller other than the owner may only put records
  that are theirs, about what they see, and that change only what they may
  write (see Permissions and credentials).
- Blobs and trees are made durable before the records that refer to them, and
  each tree after the trees it holds, so the repository never has a tree
  without its contents, which a later pack may rely on. Statements are made
  durable before the revisions they are about. Promotions come last: each is
  checked against its original by `Store::claim`, once the original and both
  subjects are stored.
- It issues `core.divergent` for each change the new revisions made diverge.
- It fails if the caller is anonymous, if an object is malformed or is an
  acceptance, if it needs an object that neither the pack nor the repository
  has or that is of another kind, if the caller may not put a record or
  doesn't see what a statement is about, if the issuer of a reserved kind may
  not issue it, if a promotion doesn't match its original (the other records
  stay stored), or if the objects cannot be written.

## Workspaces and platform code

A repository can have several working copies, called workspaces, that share its
store and op log. The main working copy's `.origin` holds the store. The others'
`.origin` holds only `workspace`, `state`, `moving`, `lock`, and a `repository`
file that names the main one's `.origin`. The main working copy's
`.origin/workspaces/` holds a file for each other workspace, named by the hash
of its root and containing that root. Records never change, and only target
chains, the index and each working copy are locked, so commands run at the same
time in different workspaces. In one workspace, each command waits for the
previous one to finish, except `watch`, which never touches the working copy,
and `workspace rm`, which opens the workspace it removes.

Creating a workspace registers it and checks out its files, and only then
writes its `.origin/workspace`. So a crash never leaves partial files that a
command would record, only a workspace that `origin workspace rm` removes.
Removing a workspace records its draft first, so its work stays a revision of
its change.

The working copy is always on a revision of the current change. Every command
that reads the working copy snapshots its files first. The files it finds are
the draft. Commands that only read the working copy show the draft without
recording it. Commands that build on the working copy record the draft first: if
the files changed, they become a new revision of the change.

Records are written before anything refers to them: the snapshot's objects,
then the revision, then the `workspace` file that names it. The statements a new
revision needs, such as its unresolved conflicts, are written before the
revision, so it is never found without them.

A command that moves the working copy to another revision, such as a merge,
checks out the files before it writes the revision. If a file was edited since
the snapshot, the checkout fails and puts back the files it changed. The working
copy and its change stay where they were, so the command can run again.

Every move of the working copy is logged in the op log, in an entry made durable
together with the `workspace` file that names the new revision. A move that
changes files first writes `moving`, which names the files it starts from, the
files it goes to, and their revision. If a crash cuts the move short, the next
command to open the workspace finishes it, or takes it back if the revision
wasn't written.

`origin workspace new [--change X] [path]` creates a workspace that shares
the repository's store:
- Each workspace has its own current change and stat cache.
- Without `--change`, it starts a new change on the current change's base.
- Files that another working copy holds unmodified are cloned copy-on-write
  from it (on Btrfs, XFS or APFS), or copied otherwise. The rest are written
  from the store.

Each clone and fetch saves the user's rights on the remote in the repository's
`.origin/access/`, in a file of their own, keyed by who the remote says they
are and their token's scope. So users, or tokens of one user, whose
workspaces share the repository never check their work against each other's
rights, and a new token with the same scope keeps what the old one knew.
Until a user syncs, they get the rights of whoever cloned the repository.
- The paths a user may not read are withheld: their content never reaches the
  repository, they are never in the working copy, and snapshots keep them as
  they were.
- A push that changes a path the user may not write fails before sending
  anything. It checks the user's own rights, which it asks the remote for if
  they never synced.
- When the remote lets the user read more, a fetch restores the files they
  can now read.

Checkouts are crash-safe:
- Before any working-copy move that changes files, Origin durably writes a
  `moving` marker. It records the trees the move goes from and to, and the
  target revision.
- The checked-out files are made durable before the workspace pointer moves.
- A checkout never overwrites a file edited after the command's snapshot. It
  checks each file again just before replacing it, and if one changed, the
  move is refused and undone.
- After a crash, the next command finishes the move or takes it back. It only
  touches files that still match one side, so unrecorded edits survive.

A repository can have several working copies, called workspaces, that share
its store and op log:
- The main working copy's `.origin` holds the store. The others' `.origin`
  holds only `workspace`, `state`, `moving`, `lock`, and a `repository` file
  that names the main one's `.origin`.
- Commands in different workspaces run at the same time. In one workspace,
  each command waits for the previous one to finish, except `watch`, which
  never touches the working copy, and `workspace rm`, which opens the
  workspace it removes.
- The working copy is always on a revision of the current change. Every
  command that reads the working copy snapshots its files first. The files it
  finds are the draft. Commands that only read the working copy show the
  draft without recording it. Commands that build on the working copy record
  the draft first: if the files changed, they become a new revision of the
  change.
- Records are written before anything refers to them: the snapshot's
  objects, then the revision, then the `workspace` file that names it. The
  statements a new revision needs, such as its unresolved conflicts, are
  written before the revision, so it is never found without them.
- A command that moves the working copy to another revision, such as a merge,
  checks out the files before it writes the revision. If a file was edited
  since the snapshot, the checkout fails and puts back the files it changed.
  The working copy and its change stay where they were, so the command can
  run again.
- Another workspace can move the change on without this working copy: the
  change can land, or the landing queue can rebase it. The working copy is
  then behind its change. Only `origin pull` moves it to where the change
  went, merging the edits made since. Until then, edits stay unrecorded so
  the change doesn't fork, commands that only read the working copy say where
  the change went, and commands that build on it fail.
- Every move of the working copy is logged in the op log, in an entry made
  durable together with the `workspace` file that names the new revision. A
  move that changes files first writes `moving`, which names the files it
  starts from, the files it goes to, and their revision. If a crash cuts the
  move short, the next command to open the workspace finishes it, or takes it
  back if the revision wasn't written (`Workspace::finish_move`).

Code that differs between platforms lives behind traits in
`crates/origin-platform`:

| Trait | Linux | macOS |
|---|---|---|
| `Durability` | `fdatasync` for single files, one `syncfs` per batch | `fsync` per file, then one `F_FULLFSYNC` |
| `FileLock` | `flock` | `flock` |
| `CloneFile` | `FICLONE` | `clonefile` |
| `FileSystem` | `statfs` magic numbers | `statfs` type names |
| `ReadAt` | `pread` | `pread` |
| `Processes` | kill a process group once `/proc` shows it holds its run's lock | kill nothing: it can't tell which files a group holds |

Each platform has its own module, and a single `cfg_select!` picks one by
build target. That is the only conditional compilation by OS in the codebase.
One conformance test suite runs against whichever implementation is built.
The standard library, `rustix` and `reflink-copy` provide the system calls,
so `unsafe` stays forbidden everywhere. Windows is not supported.

`FileSystem` lets writers refuse network file systems such as NFS, SMB, 9p
and AFP. It can't detect FUSE file systems or synced folders such as Dropbox
or iCloud, and accepts them.

`DurableWrite` writes files durably as a group. `stage` writes each file under
a temporary name in its destination's directory, and `is_temporary`
recognizes these names. `commit` makes all the files durable with
`Durability::sync_batch`, renames them into place from several threads, and
syncs their directories. Readers see either the previous file or the complete
new one, and once `commit` returns, a crash loses none of them.
- Dropping the write before committing removes its temporary files. A crash
  leaves them behind, so code that lists a directory written this way must
  skip them, and remove the stale ones.
- `sync_with` adds a file written in place, such as an append-only log, to the
  batch sync. The file is then durable by the time the batch is published, at
  no extra sync.
- Files staged with `stage_in` are published in layers. `commit` renames each
  layer's files and syncs their directories before it renames any file of
  the next layer. A crash or a failed rename therefore never publishes a file
  without the files of earlier layers, such as an object without the objects
  it refers to. Each layer costs its directories' syncs, but the batch is
  still synced once.
- Files can be staged from several threads at once.

## On disk

A repository is a directory. Its store:

```
.origin/
  objects/loose/<2 hex>/<62 hex>         # zstd objects, recently written
  objects/packs/<id>.{pack,idx}          # small objects, compacted
  objects/packs/manifest                 # the packs in use; replacing it publishes packs
  objects/packs/lock                     # held by the single compactor
  objects/collection                     # writers hold it shared, gc exclusive
  targets/<target id>/{name,log,head}    # the chain, and its latest acceptance
  queue/<target id>/                     # landings waiting, and the queue's lock
  index/{log,lock}                       # derived: heads, statements by subject, digests
  oplog/{generation,lock,writers/,<segments>}
  push                                   # serializes revision puts and queue rebases
  fetching/<tree>                        # snapshot fetches not yet complete
```

A working copy adds:

```
  workspace                              # the working copy's revision
  state                                  # the stat cache
  moving                                 # a move in progress
  lock                                   # held by the command working in the working copy
  workspaces/<hash>                      # the other workspaces of this repository
  remote                                 # the remote's URL
  access/                                # each user's rights and event cursor on the remote
```

Another workspace's `.origin/` holds only `workspace`, `state`, `moving`,
`lock` and `repository`, which names the main `.origin`.

- **Durability order.** Each batch writes its files beside their
  destinations, syncs them in one batch sync, renames them into place and
  syncs the directories. Only then is anything published that refers to them.
  Statements are published before the revision they describe.
- **Locking.** Records are immutable, so writing one needs no lock. Target
  chains, queues, pushes, the index, the op log, the compactor and each
  working copy's commands take `flock`s, which the kernel releases if the
  process dies.
- **Packs.** Statements are the bulk of the data, so loose objects of up to
  16 KiB are packed. The server compacts in the background once about 8,000
  have piled up, and `origin gc` compacts on demand.
- **Names.** User-chosen names (targets, changes) never become file names.
  They are hashed, which avoids case-folding and Unicode normalization
  problems on macOS.
- **The index is derived.** It catches up from the op log on open.
  `origin index rebuild` reconstructs it from the store, so a corrupt index
  never loses data.
- **Op log segments** are checksummed frames. A half-written last frame is
  cut off. Damage in the middle is read past, reported, and makes the index
  rebuild.
- **Temporary files** are `.tmp-<pid>-<n>` beside their destination.
  `origin gc` removes the ones a crash left behind.
- **Local filesystems only.** Writers refuse network file systems. A shared
  folder is a remote, never a live repository.

### Op log

The op log is the ordered list of everything this node applied. It is local,
never synced with other nodes, and every workspace of the repository shares
it, so each entry names the workspace and the command it came from. An entry
records one of:

- records written (revisions, statements and acceptances), with what the index
  keeps of them;
- a target advancing to a new acceptance;
- a landing left waiting in a queue;
- a working copy moving, by recording a draft or by a command.

Each entry has a sequence number, one more than the entry before it, which
never changes. It is the cursor of the event stream. The entries one command
run appends form an operation, numbered by the sequence number of its first
entry. `origin op log` shows operations, and `origin undo` takes them back.

A segment is a file of frames, one entry per frame. Entries are appended to the
last segment until it is full, then to a new one. The log keeps the latest
16 MiB of entries and deletes older segments; a cursor older than that has
expired. So has a cursor from another log, such as the one a restored
repository had: the log's generation (`OpLog::generation`) tells logs apart.

**Writing.** A write appends its entries (`Store::commit_logged`) just before
it commits, and they are synced together with its files
(`DurableWrite::sync_with`), so logging costs no extra sync. Before appending
its first entry, the writer creates and locks `writers/<n>`, where `n` is that
entry's sequence number; every entry names its writer. Once the write is
published, the writer removes the file. A target's `target.advanced` event
comes from the entry logged when its head moves, not from the entry that wrote
the acceptance. The index catches up from the entries, so writers don't update
it.

**Reading.** Readers stop before the first entry whose writer still holds its
lock (`OpLog::settled`). Every entry before it is durable, and its write is as
published as it will ever be. Otherwise a power loss could drop an entry a
reader had seen, and the next entry would reuse its sequence number. A writer
that crashed leaves its lock file unlocked: readers then sync the segments
holding its entries, remove the file, and take the entries' effects as
whatever was published, so a crashed writer holds nothing back. A crash can
leave a write unpublished or partly published: some or none of a batch's
records, an acceptance the target's head never moved to (the next landing on
the target publishes it), a waiting landing that wasn't kept, or a working
copy that didn't move (the workspace module finishes such a move).

**Damage.** A crash during an append leaves part of a frame at the end of the
last segment. It never was an entry: readers skip it, and the next append cuts
it off. Other damage loses the entries it covers. Readers read past it from
both ends of the segment and report how many entries were lost, and the index
rebuilds itself from the store, since those entries may have named records. The
first reader to find damage in the last segment starts a new segment, so
appends continue past it. When the damage is at the end of the last segment,
the next append skips as many sequence numbers as the damaged bytes could
hold, so none is reused.

## Command line

```
origin init                         # a repository with `policy` and `main`
origin new [desc] [--on <target>]   # start a change
origin describe "..."               # refine its intent
origin edit <change>                # move the working copy
origin status | diff | show [<change>] | log [<target>] | evolog [<change>]
origin converge <change>            # merge divergent heads
origin resolve <path>...            # accept conflicted files as they are
origin abandon [<change>] --reason "..."
origin annotate <rev> <kind> --pass|--fail|--neutral|--withdraw [--inputs ...] [--external] [-m <text> | --payload <file|->]
origin annotate --on <statement> <kind> --pass|--fail|--neutral|--withdraw [-m <text> | --payload <file|->]
origin statements [<rev>] | promote [<rev>]
origin target new <name> --from <target> | targets
origin land [<change>] [--onto <target>] | land --from <target> --onto <target> | land --status
origin clone <url> [path] [--target <t>] | remote [<url>] | fetch | pull | push [<change>...]
origin watch [--kind ...] [--subject ...] [--since <cursor>] [--local]
origin workspace new [--change <c>] [path] | workspace list | workspace rm <path>
origin undo | op log
origin whoami | permissions | token issue --scope ... [--ttl ...]
origin gc | index rebuild
origin serve | repo create <path> [--admin <principal>]
origin <plugin> ...                 # runs origin-<plugin>, e.g. `origin ci run`
```

## Testing and benchmarks

- **Unit and integration tests** cover each module.
- **End-to-end tests** run the `origin` binary in temporary directories, over
  a real `origin serve` and over local paths.
- **HTTP tests** drive every SDK method against a real server.
- **OpenAPI:** `docs/openapi.json` must match the server; `just openapi`
  rewrites it.

Benchmarks (`just bench`) cover:
- store writes and pack lookups;
- snapshots, checkouts and workspace creation on a 20,000-file tree;
- input digests and promotion;
- landing, including concurrent proposers and target-onto-target landings;
- permission checks;
- reading events;
- server request latency and long-poll wakeups;
- clone, push and fetch over HTTP, including a fetch that brings nothing from
  a long history;
- the CI worker's overhead per check.

`just bench-git` times `status`, snapshots and sync against git with
hyperfine.

The **swarm scenario** is a skill run from T3 Code:
1. Three agents work in parallel workspaces and land on a feature target. One
   changes a shared signature that another depends on, so a landing
   conflicts.
2. Stale results are rerun and unchanged ones are promoted.
3. Processes are killed with `kill -9` along the way: a checkout, the CI
   worker and the server.
4. The work reaches `main` in a single acceptance.
5. The skill checks that target chains stay consistent and that no revision
   is lost, and reports timings.

`scripted.sh` is its dry run, with scripted edits in place of the agents.

## Later

- A virtual filesystem for workspaces: FUSE on Linux first, then FSKit on
  macOS. It goes behind a `Vfs` trait in `origin-platform`.
- A `Watch` trait (inotify, FSEvents) instead of polling, if polling ever
  shows up in profiles.
- Webhooks, as a plugin.
- Line-level and structure-aware merging. Merges are path-level today.
- Batching several changes per acceptance, and speculative merge trains.
- Set reconciliation (Negentropy-style) for sync.
- Remotes on plain storage, such as S3 with conditional writes.
- `origin-review` (a UI) and `origin-semantic` (tree-sitter diffs).
- Signed statements from outside issuers, and identities separate from keys.
- A git bridge.
- A local log of drafts, `origin draft list` and `origin draft restore`, to
  take back a group of edits.
