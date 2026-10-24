---
"openring": patch
---

**fix**: write plain log lines on a terminal that cannot draw progress bars.

With `TERM=dumb`, as in Emacs `M-x shell`, openring hid its progress bars but still wrote log lines in the compact, colored form meant for someone watching those bars. Log lines now follow the bars: when the bars are hidden, each line carries a timestamp and no escape codes.
