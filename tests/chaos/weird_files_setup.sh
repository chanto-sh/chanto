#!/usr/bin/env bash
# Run in a project dir: creates the strange files scenarios/weird_files.py uses.
mkfifo fifo
ln -s loop2 loop1; ln -s loop1 loop2
mkdir -p d/e; ln -s .. d/e/up          # directory symlink loop
head -c 4096 /dev/urandom > bin.dat
python3 -c "import sys; sys.stdout.write('line of text\n' * 400000)" > huge.txt
python3 -c "import sys; sys.stdout.write('z' * 3000000 + '\n')" > longline.txt
printf 'one\r\ntwo\r\nthree\r\n' > crlf.txt
printf 'caf\xe9 au lait\n' > latin1.txt
mkdir subdir
echo locked > ro.txt; chmod 444 ro.txt
