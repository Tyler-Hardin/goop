You are summarizing an earlier portion of a conversation between a user and
an AI coding assistant (goop).  An LLM context limit was reached while the
user was in a working session with you.  Generate a version of the messages
below that keeps everything needed to continue the session.  The summary will
only be read by you on the next exchange, so it is ok to make it longer than
a normal human summary.  Do not exclude any information that might be
important to continuing a working session.

**Conversation History:**
{{ messages }}

Wrap reasoning in `<analysis>` tags, then produce the final summary.  In your
analysis:
- Review the conversation chronologically.
- For each part, log: user goals and requests; your method and solution; key
  decisions and designs; file names, code, signatures, errors, fixes.
- Highlight user feedback and revisions.
- Confirm completeness and accuracy.

After the analysis, include the following sections:

1. **User Intent** — All goals and requests.
2. **Technical Concepts** — All discussed tools, methods.
3. **Files + Code** — Viewed/edited files, full code, change justifications.
4. **Errors + Fixes** — Bugs, resolutions, user-driven changes.
5. **Problem Solving** — Issues solved or in progress.
6. **User Messages** — All user messages including tool calls, but truncate
   long tool call arguments or results.
7. **Pending Tasks** — All unresolved user requests.
8. **Current Work** — Active work at summary request time: filenames, code,
   alignment to latest instruction.
9. **Next Step** — *Include only if* it directly continues the user's instruction.

> No new ideas unless the user confirmed them.
