# ADR 0006: Restarting servers: backoff, crash loops and recovery

Date: 2026-10-09. Status: accepted.

## Context

The kernel restarts a dead server (diskfs, netd, procfs, the self-tests' ringtest) at the next
use of its service (`Server::revive`, `kernel/src/process/mod.rs`). Without a limit, a crash loop
(a server that cannot start, or dies whenever it is used) costs a start, and its clients a
wait, at every use, for ever. The first limit was a lifetime count: five restarts per boot, then
`EIO` for good. That took the service away for the rest of the boot from deaths of any kind
however far apart (each lxtest run kills ringtest twice on purpose, so a third run failed), and
let one program that crashes a server six times take it away from every other program.
Its successor measured a life up to the next use rather than to the death, and still gave up for
good.

## Decision

The kernel records each incarnation of a server: its pid when spawned, the time it registered
its service (`ipc_register`, also when a start timed out and it registered later), and the time
its process exited (`process_exit`, before its services are marked dead, so whoever finds the
service dead finds the death recorded) or executed another program (`exec`: the server's
program is gone, so its services and interrupt lines go too, and the new program gets none of
their requests). A life is registration to exit.

- **Young deaths and backoff** (as Kubernetes' CrashLoopBackOff): a death after a life of less
  than half a second (`STABLE_LIFE`), or before registering, is young. After the k-th young
  death in a row the restart waits until 0.1 s × 2^(k-1) after the death (`BACKOFF_BASE`); a
  longer life resets the row, and the next restart is immediate.
- **Crash loop**: a sixth young death in a row (`MAX_YOUNG`), or more than 20 restarts within
  60 s whatever the lives (`RESTART_INTENSITY` in `RESTART_PERIOD`, as Erlang's supervisor
  intensity and systemd's start limit): the service is down. Its clients get `EIO` at once.
- **Recovery**: a crash loop lasts 5 s (`COOLDOWN_BASE`), doubling with every crash loop in a row
  up to 5 min (`COOLDOWN_MAX`); a life of 60 s ends the row. Once it is over, the next use
  restarts the server with a clean history.
- **One restart at a time**: one caller restarts, the others wait for it and look again. The
  restarting caller looks once more after it won (another may just have finished a restart),
  and restarts only an incarnation that exited: one still starting or exiting (a server that
  executed another program) is waited for, up to the start timeout.

## Consequences

- A server killed now and then (a test, `kill` in the kernel monitor, a program crashing it
  occasionally) is restarted every time, at once.
- A crash loop costs at most six starts and 3.1 s of backoff, then nothing until the cooldown is
  over; a program that keeps crashing a server keeps it down only while it does.
- A server dying at every use while its clients come more slowly than its backoff grows is
  bounded by the restart intensity (20 a minute).
- `lxtest crashloop` (in `runtests.sh`) checks the backoff, the service down and its return.
