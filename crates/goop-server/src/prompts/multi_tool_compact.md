Your task is to summarize a sequence of tool calls & responses to save tokens.

Reply with a single message that describes what happened. For each tool call
note what was requested and the key outcome. Keep it concise — a few sentences
total. So you might see something like read a file and then run tests, and you
could reply with:

"Read config.rs (180 lines) and session.rs (2,100 lines) — established the
compaction flow and tool configuration. Then ran `cargo test` — 94 passed."

if that is what happened.
