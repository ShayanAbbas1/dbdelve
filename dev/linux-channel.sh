# Sourced by dev/package-linux.sh and dev/package-appimage.sh: the version and
# the names the Linux packages carry for the channel being built.
#
# DBDELVE_CHANNEL=nightly packages DBDelve Nightly under the slug
# dbdelve-nightly, which names its binary, desktop entry, icons and assets, so
# it installs beside stable rather than over it. Anything else is stable, under
# dbdelve.

# The variables are read by the scripts that source this.
# shellcheck shell=bash disable=SC2034

VERSION="${DBDELVE_VERSION:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
if [[ "${DBDELVE_CHANNEL:-}" == nightly ]]; then
  SLUG=dbdelve-nightly
else
  SLUG=dbdelve
fi
ICON_SIZES=(16 32 64 128 256 512)

# The channel's desktop entry, on stdout. Nightly's is stable's with every name
# swapped, and each swap is checked, so an edit to stable's entry cannot quietly
# leave Nightly launching or grouping under stable's names.
desktop_entry() {
  local entry=assets/linux/dbdelve.desktop
  if [[ "$SLUG" == dbdelve ]]; then
    cat "$entry"
    return
  fi
  local swapped
  swapped="$(sed \
    -e 's/^Name=DBDelve$/Name=DBDelve Nightly/' \
    -e "s/^Exec=dbdelve /Exec=$SLUG /" \
    -e "s/^Icon=dbdelve$/Icon=$SLUG/" \
    -e "s/^StartupWMClass=dbdelve$/StartupWMClass=$SLUG/" \
    "$entry")"
  local line
  for line in "Name=DBDelve Nightly" "Exec=$SLUG %f" "Icon=$SLUG" "StartupWMClass=$SLUG"; do
    if ! grep -qxF "$line" <<<"$swapped"; then
      echo "$entry changed shape: the nightly entry has no $line" >&2
      return 1
    fi
  done
  printf '%s\n' "$swapped"
}
