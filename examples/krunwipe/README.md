# krunwipe

Run a Python script or an OCI container inside a libkrun microVM whose entire
guest RAM is described to the host's kdump kernel as holding secrets, so that
it never reaches a vmcore untouched.

A guest's memory is not the host's to hand to whoever ends up debugging a host
crash, but it is also by far the largest thing in the dump. The kernel patch
this tool goes with adds `MADV_CRASH_SECRET` and `MADV_CRASH_CACHE`, letting a
process say what its memory *holds*; what the kdump kernel then does about it
-- zero it, leave it out of the dump -- is the kdump kernel's policy, not the
VMM's request. `krunwipe` drives libkrun with that turned on.

## What it does

The marking itself lives in libkrun, behind a new call:

```c
krun_set_crash_memaction(ctx_id, KRUN_CRASH_MEMACTION_SECRET);
```

libkrun marks every region of the guest's `GuestMemoryMmap` as soon as it is
mapped -- before the payload is loaded and before any vCPU runs -- so there is
no window in which the guest owns memory the kdump kernel knows nothing about.
Nothing is pinned and nothing is populated: the marks live in the VMA flags, so
pages faulted in later are covered as they arrive and a VM configured with
64 GiB of RAM still commits only what its guest touches.

`krunwipe` is the part that decides what runs in that VM.

## Usage

```
krunwipe [OPTIONS] python <SCRIPT> [ARGS]...
krunwipe [OPTIONS] container <IMAGE> [CMD]...
krunwipe [OPTIONS] run <ROOTFS> <CMD> [ARGS]...
```

`python` exposes the host's own filesystem as the guest root over virtiofs, so
the guest just has the host's Python -- no image and no registry involved.

`container` flattens an OCI image into a directory with `podman`/`docker
export` and boots it with the entrypoint, command, environment and working
directory the image declares. A command given on the command line replaces the
image's `Cmd` while still being appended to its `Entrypoint`, as OCI specifies.
The unpacked tree is removed when the VM exits.

`run` is the escape hatch for a rootfs you already have.

### Options that matter

| Option | Meaning |
| --- | --- |
| `--crash-memaction secret\|cache\|secret-cache\|none` | What guest RAM holds. Default `secret`. |
| `--mem MIB` / `--cpus N` | Guest RAM (all of it is marked) and vCPU count. |
| `--kernel PATH` | Guest kernel, if libkrunfw is not installed. |
| `--kernel-format FMT` | `raw` (default on aarch64), `elf` (default on x86_64), `image-gz`, ... |
| `--initramfs PATH` | Boot an initramfs with `--kernel`. |
| `--net` | Guest networking through libkrun's TSI vsock. |
| `--verify` | Read the marks back out of `/proc/self/smaps` and report them. |

**A host kernel that does not track the type asked for drops the request rather
than refusing it**, so the VM starts either way and `madvise()` returning 0
proves nothing. `--verify` is what tells the two cases apart.

## Host requirements

* A kernel with `CONFIG_CRASH_MEMACTION=y`, booted with `crash_memaction=` that
  names the types you ask for (`crash_memaction=secret`). Check with
  `cat /sys/kernel/crash_memaction`.
* KVM.
* `podman` or `docker`, for `container` mode only.

## Guest kernel requirements

libkrun delivers its init as a virtual file injected by the virtiofs driver and
mounts virtiofs as the guest root, so virtiofs must be **built in**, not a
module -- there is no initramfs in the boot path to load one from. If you are
supplying your own kernel with `--kernel`:

```
CONFIG_FUSE_FS=y
CONFIG_VIRTIO_FS=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_VSOCKETS=y          # only for --net
CONFIG_VIRTIO_VSOCKETS=y   # only for --net
```

Without `--kernel`, libkrun uses libkrunfw, which already has all of this.

## Building

```sh
# from the libkrun checkout
make                                    # builds libkrun.so
cd examples/krunwipe && cargo build --release
```

The build script finds libkrun through pkg-config if it is installed, and
otherwise falls back to the workspace `target/` directory, with an `$ORIGIN`
rpath so the binary runs next to a copied `libkrun.so`.

### Cross-compiling for an aarch64 target

`flake.nix` at the top of the tree provides a shell with rust (both the
`aarch64-unknown-linux-gnu` and `aarch64-unknown-linux-musl` targets, the
latter for init-blob's guest init), the cross toolchain, `cpio` and `adb`:

```sh
nix develop
cargo build --release --target $CROSS_TARGET -p libkrun
(cd examples/krunwipe && cargo build --release --target $CROSS_TARGET)
```

A plain `cargo build` in that shell still targets the host, so the tests below
work there too.

Two things the shell handles that are easy to trip over otherwise: nixpkgs'
cross gcc points the ELF interpreter at its own glibc under `/nix/store`, which
does not exist on the target, so the shell overrides it back to
`/lib/ld-linux-aarch64.so.1`; and that glibc is newer than a typical distro's,
so it is worth checking that nothing newer than the target's glibc is required
before deploying:

```sh
readelf -V <binary> | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1
```

### Deploying

libkrun's soname is `libkrun.so.2`, so the bare `libkrun.so` from the target
directory has to be named that on the far end:

```sh
adb push target/$CROSS_TARGET/release/libkrun.so           /path/on/target/libkrun.so.2
adb push examples/target/$CROSS_TARGET/release/krunwipe    /path/on/target/krunwipe
```

The binary has an `$ORIGIN` rpath, so the two sitting side by side is enough.

For `--kernel`, the simplest guest kernel is the one the target is already
running, which is guaranteed to match:

```sh
# on the target
sudo cp "/boot/efi/$(cat /etc/machine-id)/$(uname -r)/linux" ./Image
```

For `container` mode the target also needs a working container engine; see
`setup-docker-nft.sh` for getting dockerd up on a kernel without the legacy
iptables path.

## Checking that it worked

`--verify` parses `/proc/self/smaps` and totals the mappings the kernel reports
as `cm` (`VM_CRASH_MARK`):

```
$ krunwipe --kernel ./Image --mem 1024 --crash-memaction secret --verify run / /bin/true
krunwipe: 1024 MiB of guest RAM marked secret
krunwipe: verify: at guest start: 1 VMA(s): 1024 MiB marked, 0 MiB resident
krunwipe: verify: peak:           1 VMA(s): 1024 MiB marked, 1024 MiB resident
```

The two lines are the point: the whole of guest RAM is marked while **nothing
is resident yet**, and it stays marked as the guest faults it in. The marks
cover memory that is mapped, not memory that is committed.

This is also the only honest support check. The kernel sets the flag only if
the type asked for is one it tracks, and drops the request silently otherwise,
so a host without `crash_memaction=secret` gets no `cm` and `--verify` says so.
One flag covers both types, so smaps cannot report which of them took.

`tests/guest-probe/` builds a tiny initramfs holding one static binary that
touches a given amount of RAM and waits, which is what makes that second line
reproducible without needing a real workload:

```sh
tests/guest-probe/build.sh
krunwipe --kernel ./Image --initramfs tests/guest-probe/guest-probe.cpio \
         --kernel-cmdline "rdinit=/init" --mem 1024 --verify run / /bin/true
```

## Known limits

* The marks follow the folios a marked mapping holds. A page swapped out before
  the panic is not in memory for the kdump kernel to act on, and the copy in
  swap is untouched -- a system that cares wants swap off or encrypted.
* A virtiofs DAX window or a vhost-user region mapped over part of a guest
  memory region afterwards carries its own VMA and so is not covered by the
  mark on the region it replaced.
* Everything here is best effort, as the rest of kdump is.
