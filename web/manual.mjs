// The terminal's manual: an entry a command, read three ways: `man COMMAND`, a page in the
// viewer; `COMMAND -h`, the same in short in the terminal; and `help`, a line a command, by
// section. Each entry: a one-line summary, its usage, what it does in short paragraphs, its
// options, what it needs, examples with what they do, and related commands.

const sections = [
 ['Files', ['ls', 'cd', 'pwd', 'open', 'vim', 'mkdir', 'touch', 'rm', 'mv', 'cp', 'ln', 'readlink', 'fd', 'upload']],
 ['Text', ['echo', 'cat', 'head', 'tail', 'grep', 'wc', 'sort', 'uniq', 'xargs']],
 ['Archives', ['zip', 'unzip', 'tar', 'jobs']],
 ['Permissions', ['chmod', 'chown', 'chgrp', 'getfacl', 'setfacl', 'getfattr', 'setfattr']],
 ['Accounts', ['whoami', 'id', 'groups', 'users', 'login', 'su', 'sudo', 'exit', 'logout', 'passwd', 'useradd', 'usermod', 'userdel', 'groupadd', 'groupmod', 'groupdel']],
 ['Terminal', ['help', 'man', 'clear']],
];

const entries = {
 ls: {
  summary: 'list a directory, or show one file',
  usage: 'ls [-l] [-d] [-a] [PATH]',
  about: [
   'Lists what the directory PATH, or the working directory, holds: directories in green with a slash, executables in amber, links in cyan with where they point.',
   'A file is shown by itself. A link to a directory lists that directory, but with -l or -d the link itself is shown; a trailing slash (ls -l link/) follows it.',
  ],
  options: [
   ['-l', 'long format: type and mode (+ marks an ACL), names (links), owner, group, size'],
   ['-d', 'show a directory itself, not what it holds'],
   ['-a', 'accepted for habit: names starting with a dot are always listed'],
  ],
  needs: 'r and x on the directory listed, x on every directory above it.',
  examples: [['ls -l docs', 'long listing of docs'], ['ls -ld ~', 'your home itself'], ['ls -l link', 'the link, and where it points']],
  see: ['cd', 'fd', 'getfacl'],
 },
 cd: {
  summary: 'change the working directory',
  usage: 'cd [DIR]',
  about: ['Without DIR, or with ~, goes to your home; .. goes up. A link to a directory can be entered: the path keeps the link\'s name, as bash does, and .. goes back up through it.'],
  needs: 'x on DIR and every directory above it. Without r you may enter, but not list.',
  examples: [['cd /shared', 'an absolute path'], ['cd ..', 'one up'], ['cd', 'home']],
  see: ['pwd', 'ls'],
 },
 pwd: {
  summary: 'print the working directory',
  usage: 'pwd',
  about: ['Prints the working directory, as cd named it, links included.'],
  see: ['cd'],
 },
 open: {
  summary: 'preview a file',
  usage: 'open FILE',
  about: [
   'Shows Markdown, text and code, PDF, images, video, audio, tables, archives\' contents, *.slides.md presentations and registered *.game entries. Nothing is executed.',
   'A file opens where it was left: its slide, PDF page, scroll or playback. A link in it to another file opens that file, with ← to come back. Slides have themes (Light, Dark, Sepia, Night) in their bar.',
   'In text, code, diffs and Markdown, Ctrl/Cmd+F searches, by regular expression or plain text: Enter and Shift+Enter step through the matches, Esc closes the search.',
  ],
  needs: 'r on FILE, x on every directory above it.',
  examples: [['open notes.md', 'Markdown, rendered'], ['open deck.slides.md', 'slides']],
  see: ['vim', 'cat'],
 },
 vim: {
  summary: 'edit a text file',
  usage: 'vim FILE',
  about: [
   'Opens a floating editor with highlighting for Markdown, Rust, Python, JS/TS, JSON, HTML/CSS, C/C++, TOML, YAML and shell. It uses ordinary editing keys, not Vim\'s modes: Ctrl/Cmd+S saves, Ctrl/Cmd+F finds, Esc asks to close.',
   'UTF-8 text up to 2 MB. Unsaved changes need an explicit discard. A save that would overwrite another session\'s change is refused, and your draft kept for download. Use touch first for a new file without an extension.',
  ],
  needs: 'r to open; w to save, else the file opens read-only. The server checks every save.',
  examples: [['vim notes.md', 'edit, or create on save']],
  see: ['open', 'touch'],
 },
 mkdir: {
  summary: 'make directories',
  usage: 'mkdir [-p] DIR...',
  about: [
   'Makes each DIR, with mode 755 (777 less the umask 022). In a directory with a default ACL, the new one takes that ACL instead; in a setgid directory, it takes the directory\'s group and the setgid bit.',
   'Each DIR is made on its own: one that fails is reported, the others are still made. A plain mkdir of a single path also makes missing parents.',
  ],
  options: [['-p', 'make missing parents; a directory that exists is no error']],
  needs: 'w and x on the directory DIR goes in.',
  examples: [['mkdir -p projects/2026/notes', 'with its parents'], ['mkdir drafts archive', 'two at once']],
  see: ['rm', 'chmod', 'setfacl'],
 },
 touch: {
  summary: 'make empty files',
  usage: 'touch [-c] [--] FILE...',
  about: [
   'Makes each FILE that does not exist, empty, owned by you, with mode 644 (666 less the umask 022), or what the directory\'s default ACL gives. A file that exists keeps its content and mode: this terminal keeps no times. Through a link, the file it points to is made.',
   'Use -- before a name that starts with a hyphen.',
  ],
  options: [['-c', 'make nothing; only check those that exist']],
  needs: 'w and x on the directory a new file goes in; w on a file that exists.',
  examples: [['touch notes.md main.rs README', 'three files'], ['touch -- -h', 'a file named -h']],
  see: ['vim', 'mkdir'],
 },
 rm: {
  summary: 'remove files and directories',
  usage: 'rm [-r] [-f] [--] PATH...',
  about: [
   'Removes each PATH: a link is removed, not what it points to; a file with other names (hard links) stays under those. -r removes directories with everything in them, checking the whole tree before changing anything. There is no trash.',
   'Each PATH is removed on its own: one that fails is reported, the others are still removed. The root, the working directory and the directories above it cannot be removed.',
  ],
  options: [['-r, -R', 'remove directories and what they hold'], ['-f', 'say nothing of paths that do not exist']],
  needs: 'w and x on the directory PATH is in; its own w does not matter. In a sticky directory (mode +t, as /guest), only PATH\'s owner, the directory\'s owner or root may remove it. -r needs r, w and x on every directory emptied.',
  examples: [['rm notes.md draft.md', 'two files'], ['rm -rf old-builds', 'a tree'], ['fd -e tmp -X rm', 'every .tmp file below']],
  see: ['mv', 'ln'],
 },
 mv: {
  summary: 'move or rename',
  usage: 'mv [-f|-n|-i] [-v] [--] SOURCE... DEST',
  about: [
   'Moves each SOURCE to DEST, or into DEST if it is, or leads to, a directory. What moves keeps its owner, mode, ACLs and other names; a link moves as itself. Moving changes no content, so it is instant whatever the size.',
   'As on Linux, a file at the destination is replaced, but never a directory. The working directory, the directories above it and account homes cannot be moved (usermod -d HOME -m moves a home).',
  ],
  options: [['-n', 'never replace a file'], ['-i', 'ask before replacing each file (also under sudo, xargs and fd -x)'], ['-f', 'replace without asking (the default)'], ['-v', 'say what moved']],
  needs: 'w and x on both directories; w on a directory that changes directory. In a sticky directory, only the owner of what is moved or replaced, the directory\'s owner or root may.',
  examples: [['mv draft.md notes.md', 'rename'], ['mv -i notes.md drafts.md archive/', 'into a directory, asking'], ['fd -e log -X mv -n {} old-logs/', 'without replacing']],
  see: ['cp', 'rm'],
 },
 cp: {
  summary: 'copy files and directories',
  usage: 'cp [-r] [-a|-d|-p] [-L|-P|-H] [-f|-n|-i] [-v] [--] SOURCE... DEST',
  about: [
   'Copies each SOURCE to DEST, or into DEST if it is, or leads to, a directory, merging into directories that exist. Copies share the original content, so even large files copy at once, and editing either later leaves the other as it was.',
   'A new copy is yours, with the source\'s mode less the umask (022), or what the directory\'s default ACL gives; -p keeps the mode, ACLs and, as far as you may, owner and group. A file that exists is written as it is, keeping its owner, mode and other names; -f removes one you may not write and copies anew.',
   'Links: without -r, a link named is followed and its file copied; with -r, links are copied as links (-P), unless -L follows them all or -H those named. A file with several names is copied as several files, unless -d or -a keep them one. A file and another name of it are "the same file". A file still uploading cannot be copied yet.',
  ],
  options: [
   ['-r, -R', 'copy directories and what they hold'],
   ['-a', 'archive: -r, -d and -p, and extended attributes'],
   ['-d', 'copy links as links, and keep a file\'s names together'],
   ['-p', 'keep mode, ACLs, owner and group'],
   ['-L / -P / -H', 'follow every link / none / those named'],
   ['-n', 'never replace a file'],
   ['-i', 'ask before replacing each file, also below a directory'],
   ['-f', 'remove a file that cannot be written, and copy anew'],
   ['-v', 'say what was copied'],
  ],
  needs: 'r on files and r and x on directories copied; w and x where copies are made; w on a file written over.',
  examples: [['cp notes.md notes-backup.md', 'a copy'], ['cp -r projects /shared/team', 'a tree, into a directory'], ['cp -a site site.bak', 'everything, links and modes kept'], ['fd -e pdf -X cp -n {} archive/', 'without replacing']],
  see: ['mv', 'ln'],
 },
 ln: {
  summary: 'make hard or symbolic links',
  usage: 'ln [-s] [-f] [-n|-T] [-L|-P] [-v] [--] TARGET... [LINK|DIR]',
  about: [
   'A hard link (ln TARGET LINK) is another name for the same file: same content, owner, mode and ACL, changed through any name; the file goes when its last name is removed. Directories cannot be hard-linked.',
   'A symbolic link (ln -s TARGET LINK) holds a path, read from where the link is: relative targets are relative to the link\'s directory. It may point to nothing yet. Its own mode is always rwxrwxrwx and means nothing: what it points to decides.',
   'Into DIR if the last argument is, or leads to, a directory; with one TARGET only, a link of the same name in the working directory.',
  ],
  options: [['-s', 'symbolic link'], ['-f', 'remove a file in the way first'], ['-n, -T', 'take LINK as the name even if it leads to a directory'], ['-L / -P', 'hard-link what a link points to / the link itself (default)'], ['-v', 'say what was linked']],
  needs: 'w and x on the directory the link goes in. Without root, you may hard-link only what you own, or a regular file you may read and write that is not setuid (protected_hardlinks). Following a link in a sticky directory others may write needs you, or the directory\'s owner, to own the link (protected_symlinks).',
  examples: [['ln -s ../shared/report.pdf report.pdf', 'a relative symbolic link'], ['ln notes.md notes-too.md', 'a second name'], ['ln -sf v2 current', 'repoint a link']],
  see: ['readlink', 'ls', 'cp'],
 },
 readlink: {
  summary: 'print where a link points',
  usage: 'readlink [-f|-e|-m] [-n] [--] FILE...',
  about: ['Prints the target of each symbolic link, as written. With -f, -e or -m, prints instead the absolute path FILE leads to, every link followed.'],
  options: [['-f', 'canonical path; all but the last part must exist'], ['-e', 'canonical path; all of it must exist'], ['-m', 'canonical path; nothing need exist'], ['-n', 'end with NUL instead of a newline']],
  needs: 'x on the directories on the way.',
  examples: [['readlink current', 'its target'], ['readlink -f current', 'where it leads']],
  see: ['ln', 'ls'],
 },
 fd: {
  summary: 'find files by name',
  usage: 'fd [-H] [-t f|d|l|x] [-e EXT] [-d N] [-g|-F] [-i|-s] [-p] [-a] [-0] [PATTERN] [PATH...] [-x|-X COMMAND...]',
  about: [
   'Finds what is below the working directory, or each PATH, whose name matches PATTERN, a regular expression; without one, everything. It covers what you may list: directories you cannot read are reported and skipped. Links are found, not followed.',
   'Results print relative to where you searched, or absolute with -a; one a line, or NUL-terminated with -0. -x runs COMMAND for each result, -X once with all of them; in COMMAND, {} is the path, {/} its name, {//} its directory, {.} the path and {/.} the name without extension. Without a placeholder the paths are added at the end.',
  ],
  options: [
   ['-t f|d|l|x', 'files, directories, links or executables only'],
   ['-e EXT', 'with this extension'],
   ['-d N', 'at most N levels down'],
   ['-g / -F', 'PATTERN is a glob / a fixed string'],
   ['-i / -s', 'ignore / respect case (a pattern without capitals ignores it)'],
   ['-p', 'match the whole path, not the name'],
   ['-H', 'include names starting with a dot'],
   ['-a', 'absolute paths'],
   ['-0', 'NUL-terminated, for xargs -0'],
   ['-x / -X', 'run COMMAND on each result / on all at once'],
  ],
  needs: 'r and x on every directory searched.',
  examples: [['fd report', 'names containing report'], ['fd -e md -t f docs', 'Markdown files in docs'], ['fd -t l', 'every link'], ['fd -e txt -x mv {} {.}.md', 'rename each'], ['fd -0 -t f | xargs -0 wc -l', 'count lines, odd names too']],
  see: ['xargs', 'ls'],
 },
 upload: {
  summary: 'upload files from this device',
  usage: 'upload [DIR]',
  about: [
   'Choose files, or drop them onto the page, to upload into DIR or the working directory, up to 1 GB each, in chunks. After an interruption, choosing the same file again resumes it. Progress shows in the tab.',
   'Ctrl+C (^C on a touch screen) stops and removes the unfinished file, as removing it elsewhere does. Names that exist are not overwritten. MP4/MOV-family files are checked for faststart. Others can read a file while it uploads.',
  ],
  needs: 'w and x on DIR. A directory anyone may write needs no sign-in.',
  examples: [['upload', 'into the working directory'], ['upload ~/photos', 'into a directory']],
  see: ['jobs', 'open'],
 },
 echo: {summary: 'write text', usage: 'echo [-n] [TEXT...]', about: ['Writes TEXT, its words separated by spaces, and a newline unless -n.'], options: [['-n', 'no newline at the end']], examples: [['echo hello | wc -c', 'count bytes']], see: ['cat']},
 cat: {summary: 'write files one after another', usage: 'cat [FILE...]', about: ['Writes each FILE, or standard input; - stands for standard input. Files are read as UTF-8 text, up to 10 MB each; links are followed.'], needs: 'r on each FILE.', examples: [['cat notes.md', 'one file'], ['ls | cat', 'standard input']], see: ['head', 'tail', 'open']},
 head: {summary: 'write the first lines', usage: 'head [-n [-]N] [FILE...]', about: ['Writes the first N lines (10) of each FILE, or of standard input; -n -N all but the last N. With several files, each is headed by its name.'], options: [['-n N, -N', 'the first N lines'], ['-n -N', 'all but the last N']], examples: [['head -n 3 notes.md', 'three lines'], ['ls -l | head -5', 'five entries']], see: ['tail']},
 tail: {summary: 'write the last lines', usage: 'tail [-n [+]N] [FILE...]', about: ['Writes the last N lines (10) of each FILE, or of standard input; -n +N the lines from line N on.'], options: [['-n N', 'the last N lines'], ['-n +N', 'from line N on']], examples: [['tail -n 20 server.log', 'twenty lines'], ['tail -n +2 table.csv', 'without the header']], see: ['head']},
 grep: {
  summary: 'write the lines that match',
  usage: 'grep [-i] [-v] [-n] [-c] [-F] [-e PATTERN | PATTERN] [FILE...]',
  about: ['Writes the lines of each FILE, or of standard input, that match PATTERN, a regular expression (as grep -E reads it). With several files, lines are prefixed by their file\'s name. Fails when nothing matches.'],
  options: [['-i', 'ignore case'], ['-v', 'lines that do not match'], ['-n', 'number the lines'], ['-c', 'count them instead'], ['-F', 'PATTERN is a fixed string'], ['-e PATTERN', 'a pattern that starts with -']],
  examples: [['grep -in todo notes.md', 'numbered, any case'], ['ls | grep -v tmp', 'all but tmp'], ['fd -e md -X grep -c TODO', 'count per file']],
  see: ['fd', 'wc'],
 },
 wc: {summary: 'count lines, words and bytes', usage: 'wc [-l] [-w] [-c] [FILE...]', about: ['Counts the lines, words and bytes of each FILE, or of standard input, with a total for several.'], options: [['-l', 'lines'], ['-w', 'words'], ['-c', 'bytes']], examples: [['wc -l notes.md', 'lines'], ['fd -e rs -X wc -l', 'per file, and in all']], see: ['grep']},
 sort: {summary: 'sort lines', usage: 'sort [-r] [-n] [-u] [FILE...]', about: ['Writes the lines of the FILEs, or of standard input, sorted.'], options: [['-n', 'by leading number'], ['-r', 'reversed'], ['-u', 'one of each equal line']], examples: [['ls | sort -r', 'reversed'], ['fd -e md -X wc -l | sort -n', 'by size']], see: ['uniq']},
 uniq: {summary: 'drop repeated lines', usage: 'uniq [-c] [FILE]', about: ['Writes the lines of FILE, or of standard input, adjacent repeats once. Sort first to count all repeats.'], options: [['-c', 'prefix each with how many times it came']], examples: [['sort names.txt | uniq -c', 'count each name']], see: ['sort']},
 xargs: {
  summary: 'run a command on what it reads',
  usage: 'xargs [-0] [-d DELIM] [-n N] [-I REPLACE] [-r] [-t] [COMMAND...]',
  about: [
   'Runs COMMAND (echo) with the items read from standard input added as arguments. Items are separated by blanks and newlines, with quotes and backslashes as in GNU xargs.',
   'File commands run many at a time on the server; a failed run does not stop the next. Ctrl+C stops between runs.',
  ],
  options: [['-0', 'items end with NUL (fd -0)'], ['-d DELIM', 'items end with DELIM'], ['-n N', 'at most N items a run'], ['-I REPLACE', 'once per line, REPLACE in the arguments'], ['-r', 'nothing to read, nothing run'], ['-t', 'print each command first']],
  examples: [['fd -e tmp | xargs rm', 'remove them all'], ['fd -0 -t f | xargs -0 chmod 600', 'names with spaces too'], ['ls | xargs -I {} cp {} /backup/{}', 'one at a time']],
  see: ['fd'],
 },
 zip: {
  summary: 'make a zip archive',
  usage: 'zip [-r] [-y] [-q] [--] ARCHIVE[.zip] PATH...',
  about: [
   'Writes PATHs into the new archive ARCHIVE (.zip is added to a name without an extension), on the server, from the files as they were when it started. It appears at once and can be read while it is written.',
   'Names are stored as typed, without a leading / or ../. Links are followed, their files stored, unless -y stores them as links. Files already compressed are stored as they are, the rest deflated. What you cannot read is reported and left out. An archive that exists is not updated: remove it first.',
   'Ctrl+C cancels and removes the unfinished archive; closing the tab does not (see jobs).',
  ],
  options: [['-r', 'what directories hold too'], ['-y', 'store symbolic links as links'], ['-q', 'quietly']],
  needs: 'r on what is archived, w and x where the archive goes.',
  examples: [['zip -r notes.zip notes', 'a tree'], ['zip -ry site.zip site', 'links kept'], ['fd -e md -X zip docs.zip', 'what fd found']],
  see: ['unzip', 'tar', 'jobs'],
 },
 unzip: {
  summary: 'extract or list a zip archive',
  usage: 'unzip [-l] [-o|-n] [-q] [-d DIR] [--] ARCHIVE.zip [MEMBER...]',
  about: [
   'Extracts the members of ARCHIVE, or the MEMBERs named, into the working directory or DIR, which is made if missing. Files appear one by one; Ctrl+C leaves those already extracted, and -n continues later.',
   'A file that exists is replaced only if you say so: unzip asks replace? [y]es, [n]o, [A]ll, [N]one. A replaced file is removed and made anew, as Info-ZIP does: yours, with the archived mode; its other names keep the old file.',
   'New files and directories get the archived permission bits (000 too), or 644 and 755 when the archive keeps none, as zips made elsewhere may. Symbolic links are made last, so nothing is written through one from the same archive. Members with .. in their names, devices and duplicates are skipped with a warning; a leading / is removed. Members named that are not there, and an empty archive, are failures.',
  ],
  options: [['-l', 'list the members instead'], ['-o', 'replace without asking'], ['-n', 'never replace'], ['-q', 'quietly'], ['-d DIR', 'extract into DIR']],
  needs: 'r on the archive; w and x where members go.',
  examples: [['unzip photos.zip', 'here'], ['unzip -d restored -o backup.zip', 'elsewhere, replacing'], ['unzip -l backup.zip | grep report', 'search the list']],
  see: ['zip', 'tar', 'jobs'],
 },
 tar: {
  summary: 'make, extract or list a tar archive',
  usage: 'tar -c|-x|-t [-z] [-v] [-h] -f ARCHIVE [-C DIR] [-k|--skip-old-files|--overwrite] [PATH...]',
  about: [
   'Creates (-c), extracts (-x) or lists (-t) a tar archive, gzipped with -z or a .tgz or .tar.gz name (gzip is recognized on reading, whatever the name). Options may be bundled (-czf) or written without a dash (czf); -f names the archive, -C the directory to work in.',
   'Creating stores symbolic links as links and a file\'s second name as a hard link, unless -h stores what links lead to; it replaces an archive that exists. Extracting replaces files that exist, removing them first, as GNU tar does; links are made, symbolic ones last.',
   'Extracted files and directories get the archived permission bits less the umask (022), or, for root, exactly, setuid, setgid and sticky too. Members with .. and devices are skipped with a warning; a leading / is removed. PATHs not in the archive are failures.',
  ],
  options: [['-c / -x / -t', 'create / extract / list'], ['-z', 'gzip'], ['-v', 'name each member'], ['-h', 'store what links lead to'], ['-f ARCHIVE', 'the archive'], ['-C DIR', 'work in DIR'], ['-k', 'keep files that exist, reporting each'], ['--skip-old-files', 'keep them silently']],
  needs: 'r on what is archived and on the archive; w and x where files go.',
  examples: [['tar -czf notes.tgz notes', 'create, gzipped'], ['tar -xvf notes.tgz -C restored', 'extract elsewhere'], ['tar -tvf notes.tgz', 'list with modes and links']],
  see: ['zip', 'unzip', 'jobs'],
 },
 jobs: {
  summary: 'list archive jobs and unfinished uploads',
  usage: 'jobs [cancel ID]',
  about: ['Lists your zip, unzip and tar jobs and your unfinished uploads, with their state and progress. Jobs run on the server, and go on when their tab or the page closes.'],
  options: [['cancel ID', 'stop one, by the first characters of its ID']],
  examples: [['jobs', 'what is running'], ['jobs cancel 3fa9', 'stop one']],
  see: ['zip', 'unzip', 'tar', 'upload'],
 },
 chmod: {
  summary: 'change permissions',
  usage: 'chmod [-R] MODE PATH...',
  about: [
   'MODE is octal (640, 2775, 1777) or symbolic: u, g, o or a, then +, - or =, then r, w, x, X (x for directories and what is already executable), s (setuid for u, setgid for g) or t (sticky); clauses join with commas (u=rw,go=).',
   'Directory r lists, w changes entries, x lets through. File r reads, w writes; x is kept and shown, but nothing runs. setgid on a directory gives what is made in it the directory\'s group, and the bit to new directories. Sticky on a directory lets only an entry\'s owner, the directory\'s owner or root remove or rename it, as on /guest. As GNU chmod does, a short octal mode keeps a directory\'s setuid and setgid bits; five digits (00755) clear them.',
   'With an ACL, the group bits are its mask, which bounds every named entry. A link has no mode: chmod changes what it points to, and -R passes over links.',
  ],
  options: [['-R', 'everything below too']],
  needs: 'to own PATH, or be root. Without root, setgid needs the file\'s group to be one of yours, or is dropped.',
  examples: [['chmod 700 ~/notes', 'yours alone'], ['chmod o+r report.pdf', 'others may read'], ['chmod -R u+rwX,go-rwx ~/private', 'a tree'], ['chmod g+s team', 'new files take the group']],
  see: ['chown', 'setfacl', 'ls'],
 },
 chown: {
  summary: 'change owner and group',
  usage: 'chown [-R] [-h] USER[:GROUP] PATH...\nchown [-R] [-h] :GROUP PATH...',
  about: ['Root may change the owner and group. An owner may change only the group, to one of their own. Changing who owns a file clears its setuid bit, and setgid if its group may execute it. Without -h, a link\'s file changes; with -h, or met below with -R, the link itself.'],
  options: [['-R', 'everything below too'], ['-h', 'the link itself']],
  needs: 'root, or owning PATH and being in GROUP.',
  examples: [['chown alice:research /shared', 'owner and group'], ['chown -R :research ~/notes', 'group only']],
  see: ['chgrp', 'chmod'],
 },
 chgrp: {
  summary: 'change group',
  usage: 'chgrp [-R] [-h] GROUP PATH...',
  about: ['Gives PATH the group GROUP, by name or number: chown :GROUP PATH. Accounts guests make are in group guest, so chgrp guest shares a file with every guest by its group bits.'],
  options: [['-R', 'everything below too'], ['-h', 'the link itself']],
  needs: 'root, or owning PATH and being in GROUP.',
  examples: [['chgrp guest notes.md', 'share with guests'], ['chgrp -R team project', 'a tree']],
  see: ['chown', 'chmod'],
 },
 getfacl: {
  summary: 'show access control lists',
  usage: 'getfacl [-a|-d] [-c] [-n] [-p] [-R] [-P] [--] FILE...',
  about: [
   'Shows each FILE\'s ACL as getfacl does: a header (file, owner, group, flags for setuid, setgid and sticky), then user::, user:NAME:, group::, group:NAME:, mask:: and other:: entries, and a directory\'s default: entries. An entry the mask limits shows its #effective permissions.',
   'Without an ACL beyond the mode, the three base entries are the mode.',
  ],
  options: [['-a', 'access ACL only'], ['-d', 'default ACL only'], ['-c', 'no header'], ['-n', 'numbers, not names'], ['-p', 'keep the leading /'], ['-R', 'everything below too'], ['-P', 'a link itself']],
  needs: 'x on the directories on the way.',
  examples: [['getfacl report.md', 'one file'], ['getfacl -d /guest/team', 'what new files will get']],
  see: ['setfacl', 'ls', 'chmod'],
 },
 setfacl: {
  summary: 'change access control lists',
  usage: 'setfacl [-b] [-k] [-d] [-R] [-n] {-m|-x|--set} ENTRIES FILE...',
  about: [
   'Grants named users and groups permissions beyond the owner, group and other classes, as POSIX ACLs do. ENTRIES are comma-separated: u:NAME:rwx, g:NAME:r-x, u::rw, g::r, o::-, m::rx (the mask), each with d: for a directory\'s default ACL; permissions are letters from rwxX- or a digit.',
   'Access is decided as on Linux: the owner by the owner\'s bits; a named user by that entry; then the owning group and named groups you are in, any of which may allow; else others. Every entry but the owner\'s and others\' is bounded by the mask, which setfacl recomputes as the union of the group entries unless one is given or -n.',
   'A directory\'s default ACL is what is made in it gets, instead of the umask: new files and directories take it, and new directories keep it. ls -l marks an ACL with +.',
  ],
  options: [['-m ENTRIES', 'add or change entries'], ['-x ENTRIES', 'remove entries (names only, no permissions)'], ['--set ENTRIES', 'replace the whole ACL'], ['-b', 'remove all extended entries'], ['-k', 'remove the default ACL'], ['-d', 'the entries are default ones'], ['-R', 'everything below too'], ['-n', 'do not recompute the mask']],
  needs: 'to own FILE, or be root.',
  examples: [['setfacl -m u:bob:rw notes.md', 'bob may read and write'], ['setfacl -m d:g:team:rwx,g:team:rwx shared', 'the team, now and for new files'], ['setfacl -x u:bob notes.md', 'take it back'], ['setfacl -b notes.md', 'back to the mode']],
  see: ['getfacl', 'chmod', 'chgrp'],
 },
 getfattr: {
  summary: 'show extended attributes',
  usage: 'getfattr [-d] [-n NAME] [-m PATTERN] [-e text|hex|base64] [-h] [-R] FILE...',
  about: ['Shows the names of each FILE\'s extended attributes, user.* ones (root sees trusted.* too); -d shows their values, -n one attribute. Values are written as text in quotes, or as -e says.'],
  options: [['-d', 'names and values'], ['-n NAME', 'one attribute'], ['-m PATTERN', 'names matching a regular expression (- for all)'], ['-e ENC', 'text, hex (0x...) or base64 (0s...)'], ['-h', 'a link itself'], ['-R', 'everything below too']],
  needs: 'r on FILE.',
  examples: [['getfattr -d report.md', 'every user. attribute'], ['getfattr -n user.status report.md', 'one']],
  see: ['setfattr'],
 },
 setfattr: {
  summary: 'set extended attributes',
  usage: 'setfattr -n NAME [-v VALUE] [-h] FILE...\nsetfattr -x NAME [-h] FILE...',
  about: ['Sets, or with -x removes, an extended attribute: user.NAME for anyone who may write FILE (not on links, and on a sticky directory only its owner), trusted.NAME for root. VALUE is text, "quoted text" with \\ escapes, 0x and hexadecimal, or 0s and base64; up to 4 KB of attributes a file. They stay with the file through mv and its other names, and cp -a copies them.'],
  options: [['-n NAME', 'the attribute to set'], ['-v VALUE', 'its value (empty without)'], ['-x NAME', 'the attribute to remove'], ['-h', 'a link itself']],
  needs: 'w on FILE for user.*; root for trusted.*.',
  examples: [['setfattr -n user.status -v reviewed report.md', 'tag a file'], ['setfattr -x user.status report.md', 'untag it']],
  see: ['getfattr'],
 },
 whoami: {summary: 'print your account name', usage: 'whoami', about: ['Prints the account this session acts as: guest when signed out.'], see: ['id']},
 id: {summary: 'print uid, gid and groups', usage: 'id [USER]', about: ['Prints the uid, primary group and every group of you, or USER.'], examples: [['id', 'yourself'], ['id guest', 'who is signed out']], see: ['groups', 'whoami']},
 groups: {summary: 'print group names', usage: 'groups [USER]', about: ['Prints the groups of you, or USER.'], see: ['id']},
 users: {summary: 'list accounts (root)', usage: 'users', about: ['Lists the accounts, their uids, homes, and whether they are locked.'], needs: 'root.', see: ['useradd']},
 login: {summary: 'sign in', usage: 'login [USER]', about: ['Signs in through a dialog, and enters the account\'s home. The username (root) can be changed in the dialog. Sessions last 8 hours; passwords never enter the history.'], see: ['su', 'exit', 'useradd']},
 su: {summary: 'switch account', usage: 'su [-] [USER]', about: ['Switches this browser to USER (root). - also enters its home. Others give the account\'s password; root switches to any enabled account without one. exit returns to guest, not to the account before.'], see: ['login', 'sudo', 'exit']},
 sudo: {
  summary: 'run a command as root',
  usage: 'sudo COMMAND\nsudo -u USER\nsudo -k',
  about: [
   'Runs a file or account command as root, after you verify your own password, which then holds for 5 minutes; other commands stay yours. Members of wheel only; guest cannot.',
   'sudo -u USER switches to USER and enters its home (exit returns to guest); -k forgets the verification. Use su root before open, vim or upload as root.',
  ],
  options: [['-u USER', 'switch to USER'], ['-k', 'forget the verification']],
  examples: [['sudo chown alice:team /shared', 'as root'], ['sudo -k', 'forget']],
  see: ['su', 'passwd'],
 },
 exit: {summary: 'sign out', usage: 'exit', about: ['Ends this session, closes the preview, and returns to guest at /; also after su and sudo -u, as there is no account stack.'], see: ['logout', 'login']},
 logout: {summary: 'sign out', usage: 'logout', about: ['Ends this session and returns to guest, who is signed out, at /.'], see: ['exit', 'login']},
 passwd: {
  summary: 'change a password',
  usage: 'passwd [--force] [USER]',
  about: ['Changes a password through a dialog. You give your current password and change only your own; root, or sudo passwd, may reset another account\'s. Old sessions end; your own change keeps this browser signed in. guest has no password: it is who is signed out.'],
  options: [['--force', 'allow a password shorter than 12 characters (never an empty one); at most 512 bytes']],
  examples: [['passwd', 'your own'], ['sudo passwd alice', 'reset another']],
  see: ['useradd', 'su'],
 },
 useradd: {
  summary: 'make an account',
  usage: 'useradd [--force] NAME            (signed out)\nuseradd [-m] [-d HOME] [-g GROUP] [-G G1,G2] NAME   (root)',
  about: [
   'Signed out, useradd NAME makes an account of your own: a dialog asks for its password, and you are signed in. Its home is /guest/NAME, mode 700, which only you may enter; /guest itself is shared and sticky (1777), so anyone may put files there and only their owners remove them. The account has a group of its own and is in group guest. Guests may make up to 1000 accounts, a few a minute from one address.',
   'Root makes any account: -m makes its home (/home/NAME, or -d HOME), mode 700; without -g, a group of the same name is its primary group; -G adds others. Set its password with passwd NAME.',
   'Names start with a lowercase letter, with up to 32 lowercase letters, digits, underscores or hyphens; guest is taken.',
  ],
  options: [['--force', 'signed out: allow a short password'], ['-m', 'make the home'], ['-d HOME', 'its home'], ['-g GROUP', 'its primary group'], ['-G G1,G2', 'other groups']],
  examples: [['useradd ada', 'signed out: an account of your own'], ['sudo useradd -m -G research alice', 'root: another account']],
  see: ['passwd', 'usermod', 'userdel'],
 },
 usermod: {
  summary: 'change an account (root)',
  usage: 'usermod [-a] [-G G1,G2] [-g GROUP] [-d HOME [-m]] [-l NEW] [-L|-U] USER',
  about: ['-G replaces the other groups, -aG adds to them; -g changes the primary group (not files\'). -d changes the home, -m also moves it, to a path not taken. -l renames the account, keeping its uid and files. -L locks it and ends its sessions; -U unlocks it. guest cannot be changed.'],
  options: [['-aG G1,G2', 'add groups'], ['-G G1,G2', 'set groups'], ['-g GROUP', 'primary group'], ['-d HOME [-m]', 'home, moved with -m'], ['-l NEW', 'rename'], ['-L / -U', 'lock / unlock']],
  needs: 'root.',
  examples: [['usermod -aG wheel alice', 'may sudo'], ['usermod -d /homes/alice -m alice', 'move a home'], ['usermod -L alice', 'lock']],
  see: ['useradd', 'userdel'],
 },
 userdel: {summary: 'delete an account (root)', usage: 'userdel [-r] USER', about: ['Deletes the account and ends its sessions; -r also removes its home. Its files keep its uid, and root may give them to another. root and guest cannot be deleted.'], options: [['-r', 'remove the home too']], needs: 'root.', see: ['useradd']},
 groupadd: {summary: 'make a group (root)', usage: 'groupadd GROUP', about: ['Makes a group; usermod -aG GROUP USER adds members.'], needs: 'root.', see: ['groupmod', 'groupdel']},
 groupmod: {summary: 'rename a group (root)', usage: 'groupmod -n NEW OLD', about: ['Renames the group OLD, by its name (never its number, as on Linux), keeping its gid, members and files. root, wheel and guest cannot be renamed.'], needs: 'root.', see: ['groupadd']},
 groupdel: {summary: 'delete a group (root)', usage: 'groupdel GROUP', about: ['Deletes the group GROUP, by its name, and takes it from accounts\' other groups. A group still some account\'s primary group stays. Files keep its gid. root, wheel and guest cannot be deleted.'], needs: 'root.', see: ['groupadd']},
 clear: {summary: 'clear the terminal', usage: 'clear', about: ['Clears this tab\'s output; nothing else changes.']},
 help: {summary: 'list the commands', usage: 'help', about: ['Lists the commands by section, a line each. COMMAND -h shows how to use one; man COMMAND its manual page.'], see: ['man']},
 man: {summary: 'read the manual', usage: 'man [COMMAND]', about: ['Opens COMMAND\'s manual page, or, without one, the index and the overview of permissions, links, pipes and keys. It describes what this terminal does, not a whole Linux system.'], examples: [['man ln', 'one page'], ['man', 'the index']], see: ['help']},
};

/// The general part of the manual, after the index.
const overview = `## Permissions

Each file and directory has an owner, a group and a mode, as on Linux: its owner is checked against the owner's bits, members of its group against the group's, everyone else against others'; root may do anything. Files are made 644 and directories 755 (the umask is 022), homes 700.

- **Special bits**: setgid on a directory passes its group to what is made in it; sticky (\`+t\`, as \`/guest\`'s 1777) lets only an entry's owner, the directory's owner or root remove or rename it. setuid is kept and shown, but nothing runs.
- **ACLs**: \`setfacl\` grants named users and groups more than the classes allow, bounded by the mask (the group bits once there is an ACL); a directory's default ACL is what is made in it gets. \`ls -l\` marks an ACL with \`+\`.
- **Links**: a hard link is another name for the same file; a symbolic link holds a path, followed by most commands and removed by \`rm\` as itself. Following a link another account made in a sticky directory others may write is refused (protected_symlinks); hard-linking another's file needs read and write on it (protected_hardlinks).
- **Extended attributes**: \`user.*\` attributes go with a file, for anyone who may read or write it.

## Guests

Signed out, you are **guest** (uid 65534, group guest): you may read what others may read, and write where anyone may. \`useradd NAME\` makes an account of your own, with a password you choose and a home \`/guest/NAME\` only you may enter. \`/guest\` is shared and sticky: put files there for others, and only you (or root) can remove yours. Share with every guest by \`chgrp guest\` and group bits, or with someone in particular by \`setfacl -m u:NAME:r\`.

## Pipes

\`COMMAND | COMMAND\` passes what one command writes to the next, as in \`fd -e md | xargs wc -l\` or \`ls | grep -i report | sort\`. Other shell operators (\`&&\`, \`;\`, \`>\`, \`<\`) are not supported; quote them to use them as text.

## Wildcards

Unquoted \`*\`, \`?\` and \`[...]\` expand to the matching paths, sorted by byte order as \`ls\` sorts them (bash under LC_ALL=C): \`rm -rf *\`, \`cat docs/*.md\`, \`echo */\`. Names starting with a dot match only a pattern that starts with one. Quote or escape a wildcard to keep it literal; within double quotes a backslash escapes only \`$\`, \`\` \` \`\`, \`"\`, \`\\\` and a newline, as in bash. A pattern that matches nothing stays as typed.

## Tabs

Each tab has its own working directory, history and running command. Alt+T opens a tab, Alt+W closes it, Alt+1–9 switch. All tabs share one account: su, login and exit change it for every tab.

## Keys

Tab / Shift+Tab cycle fuzzy completions; ↑ / ↓ browse history; Ctrl+C clears the input and stops the tab's command at its next step (an upload it runs is cancelled); → accepts and Esc cancels a completion. Typing in the idle workspace focuses the command line. On touch screens, Tab, ↑, ↓ and ^C are buttons under the command line. What other devices change shows as it happens.`;

const titled = name => `${name} — ${entries[name].summary}`;
const lines = text => text.split('\n');

/** The manual page of `topic`, as Markdown, or the index without one. */
export function manual(topic) {
 if (!topic) {
  const index = sections.map(([section, names]) => `## ${section}\n\n| Command | What it does |\n| --- | --- |\n` + names.map(name => `| \`${name}\` | ${entries[name].summary} |`).join('\n')).join('\n\n');
  return `# Terminal Worker Manual\n\n\`man COMMAND\` opens a page; \`COMMAND -h\` shows it in short in the terminal; \`help\` lists every command. All paths are in this terminal's own filesystem: nothing runs on a host.\n\n${index}\n\n${overview}`;
 }
 if (!Object.hasOwn(entries, topic)) throw new Error(`No manual entry for ${topic}. Enter man for the index.`);
 const entry = entries[topic];
 let page = `# ${titled(topic)}\n\n\`\`\`text\n${entry.usage}\n\`\`\`\n\n` + entry.about.join('\n\n');
 if (entry.options) page += '\n\n## Options\n\n| Option | Meaning |\n| --- | --- |\n' + entry.options.map(([flag, meaning]) => `| \`${flag}\` | ${meaning} |`).join('\n');
 if (entry.needs) page += `\n\n## Permissions\n\n${entry.needs}`;
 if (entry.examples) {
  const width = Math.max(...entry.examples.map(([command]) => command.length));
  page += '\n\n## Examples\n\n```text\n' + entry.examples.map(([command, what]) => `${command.padEnd(width)}   # ${what}`).join('\n') + '\n```';
 }
 if (entry.see) page += '\n\n## See also\n\n' + entry.see.map(name => `\`${name}\``).join(', ');
 return page;
}

/** `topic`'s help, as text for the terminal. */
export function help(topic) {
 if (!Object.hasOwn(entries, topic)) throw new Error(`Unknown command: ${topic}. Enter man for the index.`);
 const entry = entries[topic];
 const out = [titled(topic), '', ...lines(entry.usage).map((line, i) => (i ? '       ' : 'Usage: ') + line), '', ...entry.about.flatMap(p => [p, '']).slice(0, -1)];
 if (entry.options) {
  const width = Math.max(...entry.options.map(([flag]) => flag.length));
  out.push('', 'Options:', ...entry.options.map(([flag, meaning]) => `  ${flag.padEnd(width)}  ${meaning}`));
 }
 if (entry.needs) out.push('', `Needs: ${entry.needs}`);
 if (entry.examples) {
  const width = Math.max(...entry.examples.map(([command]) => command.length));
  out.push('', 'Examples:', ...entry.examples.map(([command, what]) => `  ${command.padEnd(width)}   # ${what}`));
 }
 out.push('', (entry.see ? `See also: ${entry.see.join(', ')}. ` : '') + `man ${topic} opens the manual page.`);
 return out.join('\n');
}

/** help: every command, a line each, by section. */
export function commands() {
 const width = Math.max(...Object.keys(entries).map(name => name.length));
 const out = [];
 for (const [section, names] of sections) {
  out.push(section, ...names.map(name => `  ${name.padEnd(width)}  ${entries[name].summary}`), '');
 }
 out.push(
  'COMMAND -h shows how to use one; man COMMAND its manual page; man, the index.',
  'COMMAND | COMMAND pipes; *, ? and [...] expand to paths; quote to keep them.',
  'Tab completes · ↑↓ history · Ctrl+C stops · Alt+T / Alt+W / Alt+1–9 tabs.',
  'Signed out? useradd NAME makes an account of your own, at home in /guest.',
 );
 return out.join('\n');
}
