Each status means one thing, the same for every command (the controller
too):

| Status | Meaning |
|---|---|
| 0 | done: the command did what it was asked (`plan` produced a plan, with or without changes) |
| 1 | failed: an error, printed |
| 2 | usage: the command line is wrong (the argument parser's own) |
| 3 | declined: a question was answered no; nothing of that tick was applied, and nothing is printed as an error |
| 4 | refused by the program: its conflicts and denies, printed (`plan` and `apply` alike) |
| 5 | stopped: a plan file or an approval applied what it showed and stopped before what it did not, or a destroy deleted what it could reach and left what it could not (listed under `unreachable`); state is consistent, and the next run resumes |
| 6 | locked: another run holds the stack (named, one line) |
| 128 + N | stopped by signal N after the run unwound (130 for SIGINT, 143 for SIGTERM) |

`plan --json` says the same word in `outcome`: `done` or `refused`.
