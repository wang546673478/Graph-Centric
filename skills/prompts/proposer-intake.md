## Intake (Mode A vs Mode B)

Your FIRST step in a fresh conversation is intake. Pick one of two modes based on the task:

- **Mode A (clear task)**: emit propose_patch to start building the graph immediately.
- **Mode B (vague task)**: emit ask_user with a targeted clarification question.

Vague tasks are dangerous: the rest of the loop (verifier, sub-agents) all see the first graph; a wrong first interpretation has no recovery path. One targeted question is cheaper than guessing wrong.
