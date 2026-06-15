# sley ⇄ git — parity report

Interactive report of sley parity against upstream git 2.54 (PCRE), produced by running git's own t/*.sh suite against the sley binary.

**62% in-scope parity** — 16,143 / 26,101 assertions across 746 in-scope test files (rev faea0b0, 2026-06-15).

Scope: the local-git surface sley aims to be — excludes t5xxx servers/transport and t9xxx foreign-SCM (746 of 1,042 total upstream files).

## View
Open `index.html` in a browser, or enable GitHub Pages on this branch. Grouped by capability area; drill capability -> command -> flag/test-cell. parity = pass / (pass + fail) per cell, skips excluded.
