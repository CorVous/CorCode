# Working in a CorCode workspace

`/workspace` is a git clone on a chat branch. Git is the only durable record of
your work: the container is disposable and can be torn down between turns.

- Commit as work progresses, in meaningful units with messages that explain the
  change. Do not batch a whole session into one mechanical snapshot.
- Push every commit. A branch with no upstream is pushed with
  `git push -u origin HEAD`.
- End every turn with a clean tree and nothing unpushed. A stop hook checks this
  and hands the turn back to you when work is still unsaved.
- When a push fails, say so in the reply and carry on. Do not invent workarounds
  such as rewriting history or pushing elsewhere.
