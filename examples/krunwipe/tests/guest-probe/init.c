/*
 * A guest that touches a known amount of RAM and then waits.
 *
 * Used to check the host side of krunwipe: while this is running, the host's
 * /proc/<krunwipe>/smaps should show the whole of guest RAM carrying the crash
 * marks, with resident pages tracking whatever this has actually touched.
 * That distinction is the point -- the marks cover memory that is mapped, not
 * memory that is committed.
 *
 * Built static and used as the sole contents of an initramfs, so it needs no
 * userspace in the guest at all.
 *
 * Usage: init [MiB_to_touch] [seconds_to_wait]
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <fcntl.h>

/*
 * A bare initramfs has no /dev, so the kernel hands us closed stdio. Mount
 * devtmpfs and wire fds 0/1/2 to the virtio console libkrun gave us, or
 * nothing printed here would ever reach the host.
 */
static void open_console(void)
{
	int fd;

	mkdir("/dev", 0755);
	mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);

	fd = open("/dev/hvc0", O_RDWR);
	if (fd < 0)
		fd = open("/dev/console", O_RDWR);
	if (fd < 0)
		return;

	dup2(fd, 0);
	dup2(fd, 1);
	dup2(fd, 2);
	if (fd > 2)
		close(fd);
}

int main(int argc, char **argv)
{
	unsigned long mib = argc > 1 ? strtoul(argv[1], NULL, 10) : 64;
	unsigned long secs = argc > 2 ? strtoul(argv[2], NULL, 10) : 10;
	size_t len = (size_t)mib << 20;

	open_console();

	/* Give the host a moment to sample the marks before we touch RAM. */
	sleep(2);

	printf("guest-probe: touching %lu MiB\n", mib);
	fflush(stdout);

	char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
		       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (p == MAP_FAILED) {
		perror("guest-probe: mmap");
		return 1;
	}

	/* A recognisable pattern, one write per 4 KiB page to fault it in. */
	for (size_t off = 0; off < len; off += 4096)
		memcpy(p + off, "KRUNWIPE-SECRET", 16);

	printf("guest-probe: touched %lu MiB, waiting %lu s\n", mib, secs);
	fflush(stdout);

	sleep(secs);

	printf("guest-probe: done\n");
	fflush(stdout);

	/* PID 1 exiting would panic the guest; ask for a clean shutdown. */
	reboot(RB_POWER_OFF);
	return 0;
}
