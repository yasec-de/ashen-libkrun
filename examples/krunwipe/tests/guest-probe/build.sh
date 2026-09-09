#!/bin/sh
# Build the guest-probe initramfs.
#
# The result is a cpio archive holding one statically linked binary as /init,
# enough to bring a guest kernel up to userspace without any rootfs at all.
set -eu

cd "$(dirname "$0")"
OUT=${OUT:-guest-probe.cpio}

# CROSS_CC wins over CC: inside a nix devshell CC is the *host* compiler,
# and this has to be built for the guest. Set CROSS_CC to override.
for candidate in "${CROSS_CC:-}" aarch64-unknown-linux-gnu-gcc aarch64-linux-gnu-gcc; do
	if [ -n "$candidate" ] && command -v "$candidate" >/dev/null 2>&1; then
		CC=$candidate
		break
	fi
done
if [ -z "${CC:-}" ] || ! command -v "$CC" >/dev/null 2>&1; then
	echo "$0: no aarch64 cross compiler found; run inside 'nix develop' or set CROSS_CC" >&2
	exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The nix devshell's cross gcc keeps its static libc out of the default
# search path; CROSS_STATIC_LIBS points at it.
if [ -n "${CROSS_STATIC_LIBS:-}" ]; then
	set -- "-L$CROSS_STATIC_LIBS"
else
	set --
fi

"$CC" -static -Os -o "$work/init" init.c "$@"
printf 'init\n' | cpio -o -H newc -D "$work" > "$OUT"

echo "$OUT: $(wc -c < "$OUT") bytes"
