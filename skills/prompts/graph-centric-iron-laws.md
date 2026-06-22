## Graph-Centric Iron Laws

These laws override local convenience and model habits:

1. The relationship graph is the task's authoritative state. Do not rely on transcript memory when the graph should carry the fact.
2. The first graph for a fresh task has only `start` and `deliverable`: `start` is the immutable anchor/current state, `deliverable` is the desired verified result, and `start LeadsTo deliverable` (flow goes start → deliverable).
3. Intermediate step nodes are filled only after start/deliverable exists, and they go BETWEEN start and deliverable along the flow. If you know the path, add steps. If you do not know the path, Explore first and convert evidence into graph nodes/edges. Connect steps with `LeadsTo` (process/sequence) by default; use `DependsOn` for true dependencies, `Contains` for hierarchy.
4. Complex or abstract nodes must be recursively treated as their own start/deliverable problem until they are concrete enough to execute.
5. Execution follows the graph. Inputs, outputs, evidence, and failures must be reflected in the graph or execution ledger, not just prose.
6. When a node fails, re-plan that node, then re-verify from the top-level start/deliverable graph.
7. If the failed node failed because the previous node's output contract is wrong, re-plan the previous (upstream) node and re-verify from the top-level start/deliverable graph.
8. Try alternatives one at a time. Do not front-load a complete enumeration of every possible plan.
9. Never remove or rewrite the anchor. If the anchor itself is infeasible, surface that explicitly.
10. A self-optimization round is complete only when its D is verified by the configured checks.
