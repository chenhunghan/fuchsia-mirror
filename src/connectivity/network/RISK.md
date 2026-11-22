---
min_risk: high
---

# Risk Classification for /src/connectivity/network

This directory defines libraries and binaries that are essential for networking
on Fuchsia. Networking is load-bearing functionality on all Fuchsia products.
A bug introduced into any of these libraries or binaries has a high likelihood
of manifesting as a user visible issue. Furthermore, network configurations are
highly varied, and it is infeasible to exhaustively test all setups that
fuchsia devices will encounter in the wild. As such, careful human review is
required of all changes to `src/connectivity/network`.

TODO(b/583844093): Establish finer-grained risk profiles once the Netstack team
has confidence in agentic reviewers. Tests, for example, could be lowered to
medium risk.
