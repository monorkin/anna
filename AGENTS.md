# Working on Anna

## This repository is public

Nothing that identifies the person Anna works for, their employer, their
machines or their other projects belongs in it — not in code, not in tests,
not in notes, and not in a commit message. What Anna knows about a particular
person lives in her config (`~/.config/anna`), never here.

Keep out:

- Names of people, companies and products they work on. Tests use invented
  people (Marta, Marko), `example.com` addresses, and plain words for choices
  ("Personal", "Work").
- Account, project, bucket, person and recording ids from any integration,
  and any signed id or URL that carries one.
- Machine names, internal hostnames, user names and absolute paths under a
  home folder. Tests that need a home use `/home/someone`.
- Tokens, keys and credentials of any kind, in any shape — including a
  fragment of one in an error message pasted into a note.
- Anything exported from a real transcript, memory store or inbox. A corpus
  built from real data stays out of git; write the committed one by hand or
  generate it.

Before committing, it is worth a look:

    git grep -n -i -E "<your name>|<your employer>|/home/<your user>|sk-ant-|gh[pous]_"

A review or a report from another tool is a note, and the same rules apply to
it: strip absolute paths to repo-relative ones before it goes in, or keep it
out of the repository.

## Keeping the code readable

One file is one thing, and a thing you can hold in your head. The `src`
folder is flat: a module is `name.rs`, and a module that grew a second thing
becomes `name.rs` and `name_thing.rs` (`mcp.rs` and `mcp_server.rs`,
`setup.rs`, `setup_tools.rs`, `setup_machine.rs`), never a folder.

- A file over about 500 lines of code, tests aside, is asking to be split;
  over 700 it is overdue. Split by what the code is about — the queue, the
  server process, the clock — not by size, and take the tests with the code
  they test. If no cut is obvious, that is the thing to think about, not a
  reason to keep adding.
- A function is read top to bottom, so callers go above callees, and the
  public ones go first. What a file is for goes in its `//!` comment; what a
  function is for goes in its name. A `///` comment is for what the code
  can't say — why it is this way, what it works around.
- Say something once. A helper two modules both need lives next to the trait
  or type it serves (`text_of` beside `Tool` in `broker.rs`), not copied.
- `cargo clippy --all-targets` is clean, and stays clean: fix what it says or
  say why not at the spot with an `#[allow]`, never in a settings file.
- A change to how she behaves comes with a test that would have failed
  before it, in the file the behaviour lives in, named for what it proves:
  `a_conversation_stays_opened_once_a_trusted_person_spoke_in_it`.
- Before you finish, `wc -l src/*.rs | sort -rn | head` — if what you added
  put a file at the top of that list, split it before you stop.
