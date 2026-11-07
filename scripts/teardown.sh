#!/usr/bin/env bash
# Removes the Foundry install that `make setup` added: the ~/.foundry tree
# and the PATH lines the installer appended to shell profiles.
set -euo pipefail

posix_line="export PATH=\"\$PATH:$HOME/.foundry/bin\""
fish_line="fish_add_path -a $HOME/.foundry/bin"

# Deletes every line of file $1 exactly equal to $2, through a temp file.
strip_line() {
  local file="$1"
  local line="$2"
  local tmp
  tmp="$(mktemp)"
  grep -v -x -F -- "$line" "$file" > "$tmp" || true
  mv "$tmp" "$file"
  echo "[+] cleaned $file"
}

if [ -d "$HOME/.foundry" ]; then
  rm -rf "$HOME/.foundry"
  echo "[+] removed $HOME/.foundry"
fi

for file in "${ZDOTDIR:-$HOME}/.zshenv" "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile"; do
  if [ -f "$file" ]; then
    strip_line "$file" "$posix_line"
  fi
done

fish_config="$HOME/.config/fish/config.fish"
if [ -f "$fish_config" ]; then
  strip_line "$fish_config" "$fish_line"
fi
