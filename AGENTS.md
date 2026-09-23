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
