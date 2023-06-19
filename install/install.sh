#!/usr/bin/env bash
set -euo pipefail

__wrap__() {

VERSION=${PIXI_VERSION:-latest}
INSTALL_DIR=${PIXI_DIR:-"$HOME/.pixi/bin"}

REPO=prefix-dev/pixi
PLATFORM=$(uname -s)
ARCH=$(uname -m)

if [[ $PLATFORM == "Darwin" ]]; then
  PLATFORM="apple-darwin"
elif [[ $PLATFORM == "Linux" ]]; then
  PLATFORM="unknown-linux-musl"
fi

if [[ $ARCH == "arm64" ]] || [[ $ARCH == "aarch64" ]]; then
  ARCH="aarch64"
fi



BINARY="pixi-${ARCH}-${PLATFORM}"

if [[ $VERSION == "latest" ]]; then
  DOWNLOAD_URL=https://github.com/${REPO}/releases/latest/download/${BINARY}.tar.gz
else
  DOWNLOAD_URL=https://github.com/${REPO}/releases/download/${VERSION}/${BINARY}.tar.gz
fi

printf "This script will automatically download and install Pixi (${VERSION}) for you.\nGetting it from this url: $DOWNLOAD_URL\nThe binary will be installed into '$INSTALL_DIR'\n"

if ! hash curl 2> /dev/null; then
  echo "error: you do not have 'curl' installed which is required for this script."
  exit 1
fi

if ! hash tar 2> /dev/null; then
  echo "error: you do not have 'tar' installed which is required for this script."
  exit 1
fi

TEMP_FILE=$(mktemp "${TMPDIR:-/tmp}/.pixi_install.XXXXXXXX")

cleanup() {
  rm -f "$TEMP_FILE"
}

trap cleanup EXIT

HTTP_CODE=$(curl -SL --progress-bar "$DOWNLOAD_URL" --output "$TEMP_FILE" --write-out "%{http_code}")
if [[ ${HTTP_CODE} -lt 200 || ${HTTP_CODE} -gt 299 ]]; then
  echo "error: '${DOWNLOAD_URL}' is not available"
  exit 1
fi

# Extract pixi from the downloaded tar file
mkdir -p "$INSTALL_DIR"
tar -xzf "$TEMP_FILE" -C "$INSTALL_DIR"

# bash: add line to config such that pixi is in the path
if [ -f ~/.bash_profile ]; then
  BASH_FILE=~/.bash_profile
else
  # Default to bashrc as that is used in non login shells instead of the profile.
  BASH_FILE=~/.bashrc
fi

LINE="export PATH=\$PATH:${INSTALL_DIR}"
if ! grep -Fxq "$LINE" "$BASH_FILE"
then
    echo "Updating ${BASH_FILE} ..."
    echo "$LINE" >> "$BASH_FILE"
fi

# fish: if config.fish exists, add pixi to the path
if [[ -f ~/.config/fish/config.fish ]]; then
  LINE="fish_add_path ${INSTALL_DIR}"
  if ! grep -Fxq "$LINE" ~/.config/fish/config.fish
  then
      echo "$LINE" >> ~/.config/fish/config.fish
  fi
fi

# zsh: if zshrc exists, add pixi to the path
if [[ -f ~/.zshrc ]]; then
  LINE="export PATH=\$PATH:${INSTALL_DIR}"
  if ! grep -Fxq "$LINE" ~/.zshrc
  then
      echo "$LINE" >> ~/.zshrc
  fi
fi

chmod +x "$INSTALL_DIR/pixi"

echo "Please restart or source your shell."

}; __wrap__
