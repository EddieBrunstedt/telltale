# One executable per gauge, in one package

Each gauge is its own executable (`telltale-net`, `telltale-disk`, …), all built
from one Cargo package that shares a library for readings, formats and the
check/watch loop. We considered one executable with subcommands
(`telltale net`), which would give the same code sharing and a single install.
Separate executables won for two reasons. Each running gauge shows up under its
own name in `ps`/`pkill`. And the secrets shim (`~/.local/bin/shims`) maps a
symlink name to a real executable of the same name, so `telltale-inbox` gets
its credentials with one manifest line, while a subcommand would need a name
mapping the shim doesn't have.
