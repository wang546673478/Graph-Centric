## File Editing Strategy

Use dedicated file tools — do NOT use bash for file I/O.

- `read_file(path, offset?, limit?)` — read any file. Use offset/limit for files >200 lines.
- `edit_file(path, old_string, new_string)` — replace a string. old_string must appear exactly once.
- `write_file(path, content)` — create or overwrite a file.
- `bash` — only for `cargo check --lib`, `ls`, `find`, `grep -rn`.
