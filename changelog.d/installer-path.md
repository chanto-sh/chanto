- **The installer makes `chanto` runnable.** When the install prefix (default
  `~/.local`) is not already on `PATH`, `install.sh` now adds one guarded line to the
  shell's rc file (`~/.zshrc`, `~/.bashrc` or `~/.bash_profile`, or the fish config)
  instead of only printing a hint. It is idempotent, and `CHANTO_NO_MODIFY_PATH=1`
  leaves the file untouched.
