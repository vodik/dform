Each status means one thing, the same for every command (the controller
too):

| Status | Meaning |
|---|---|
| 0 | done: the command did what it was asked (`plan` produced a plan, with or without changes) |
| 1 | failed: an error, printed; or an apply's wait on a value the world has not reached ran past its deadline (`not reached in 10m`: state is consistent, and the next apply waits again); or `status` found an object not healthy or suspended (its line says which); or `render` met a value it cannot print (each named) |
| 2 | usage: the command line is wrong (the argument parser's own) |
| 3 | declined: a question was answered no; nothing of that tick was applied, and nothing is printed as an error |
| 4 | refused by the program: its conflicts and denies, printed (`plan`, `apply` and `render` alike) |
| 5 | stopped: a plan file or an approval applied what it showed and stopped before what it did not (a tick that adds a change, or whose re-plan differs from the one it showed), or a destroy deleted what it could reach and left what it could not (listed under `unreachable`), or an apply without the deployment's master made what it could and not what needs the master (listed); state is consistent, and the next run resumes |
| 6 | locked: another run holds the stack (named, one line) |
| 128 + N | stopped by signal N after the run unwound (130 for SIGINT, 143 for SIGTERM) |

`plan --json` says the same word in `outcome`: `done` or `refused`. A
command of several deployments (`apply X` with what X reads, a plan file
of several, the project's) exits with the status of the first that did
not end done, the deployments after it not run.
