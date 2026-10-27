## Permissions

Each file and directory has an owner, a group and a mode, as on Linux: its owner is checked against the owner's bits, members of its group against the group's, everyone else against others'; root may do anything. Files are made 644 and directories 755 (the umask is 022), homes 700.

- **Special bits**: setgid on a directory passes its group to what is made in it; sticky (`+t`, as `/guest`'s 1777) lets only an entry's owner, the directory's owner or root remove or rename it. setuid is kept and shown, but nothing runs.
- **ACLs**: `setfacl` grants named users and groups more than the classes allow, bounded by the mask (the group bits once there is an ACL); a directory's default ACL is what is made in it gets. `ls -l` marks an ACL with `+`.
- **Links**: a hard link is another name for the same file; a symbolic link holds a path, followed by most commands and removed by `rm` as itself. Following a link another account made in a sticky directory others may write is refused (protected_symlinks); hard-linking another's file needs read and write on it (protected_hardlinks).
- **Extended attributes**: `user.*` attributes go with a file, for anyone who may read or write it.

## Guests

Signed out, you are **guest** (uid 65534, group guest): you may read what others may read, and write where anyone may. `useradd NAME` makes an account of your own, with a password you choose and a home `/guest/NAME` only you may enter. `/guest` is shared and sticky: put files there for others, and only you (or root) can remove yours. Share with every guest by `chgrp guest` and group bits, or with someone in particular by `setfacl -m u:NAME:r`.

## Pipes

`COMMAND | COMMAND` passes what one command writes to the next, as in `fd -e md | xargs wc -l` or `ls | grep -i report | sort`. Other shell operators (`&&`, `;`, `>`, `<`) are not supported; quote them to use them as text.

## Wildcards

Unquoted `*`, `?` and `[...]` expand to the matching paths, sorted by byte order as `ls` sorts them (bash under LC_ALL=C): `rm -rf *`, `cat docs/*.md`, `echo */`. Names starting with a dot match only a pattern that starts with one. Quote or escape a wildcard to keep it literal; within double quotes a backslash escapes only `$`, `` ` ``, `"`, `\` and a newline, as in bash. A pattern that matches nothing stays as typed.

## Tabs

Each tab has its own working directory, history and running command. Alt+T opens a tab, Alt+W closes it, Alt+1–9 switch. All tabs share one account: su, login and exit change it for every tab.

## Keys

Tab / Shift+Tab cycle fuzzy completions; ↑ / ↓ browse history; Ctrl+C clears the input and stops the tab's command at its next step (an upload it runs is cancelled); → accepts and Esc cancels a completion. Typing in the idle workspace focuses the command line. On touch screens, Tab, ↑, ↓ and ^C are buttons under the command line. What other devices change shows as it happens.
