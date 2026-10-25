#!/bin/sh
# Docker's command line, as far as wrangler uses it, over Podman. `podman build` lacks the
# buildx flags wrangler passes (--load, --provenance), and cannot read a Dockerfile from a
# piped standard input (`-f -`), so the Dockerfile goes through a temporary file. Without a
# systemd user session, Podman must manage cgroups itself.
podman() {
  if [ -z "$DBUS_SESSION_BUS_ADDRESS" ]; then
    command podman --cgroup-manager=cgroupfs "$@"
  else
    command podman "$@"
  fi
}
if [ "$1" != build ]; then
  podman "$@"
  exit $?
fi
shift
dockerfile=
file_follows=
for arg do
  shift
  if [ -n "$file_follows" ]; then
    file_follows=
    if [ "$arg" = - ]; then
      dockerfile=$(mktemp)
      cat > "$dockerfile"
      arg=$dockerfile
    fi
    set -- "$@" "$arg"
    continue
  fi
  case "$arg" in
    --load | --provenance=*) ;;
    -f | --file) file_follows=1; set -- "$@" "$arg" ;;
    *) set -- "$@" "$arg" ;;
  esac
done
podman build "$@"
status=$?
[ -n "$dockerfile" ] && rm -f "$dockerfile"
exit $status
