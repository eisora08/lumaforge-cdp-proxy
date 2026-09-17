/*
 * LumaForge pressure-vessel shim
 *
 * Replaces pressure-vessel-unruntime in the shim directory.
 * Opens CDP FIFOs from $PRESSURE_VESSEL_PREFIX, maps them to FDs 3 and 4,
 * then execs the real pressure-vessel-unruntime with --pass-fd.
 *
 * This allows CDP pipe communication to survive through the bubblewrap container.
 *
 * Build: gcc -O2 -o pvs_shim pvs_shim.c -Wall
 * Must be compiled for x86_64 (64-bit).
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <errno.h>

static void log_msg(const char *msg) {
    int fd = open("/tmp/steamcdp_proxy.log", O_WRONLY | O_CREAT | O_APPEND, 0644);
    if (fd >= 0) {
        write(fd, "[pvs-shim] ", 11);
        write(fd, msg, strlen(msg));
        write(fd, "\n", 1);
        close(fd);
    }
}

static int open_fifo_at_fd(const char *path, int target_fd, int flags) {
    int fd = open(path, flags);
    if (fd < 0) {
        char buf[512];
        snprintf(buf, sizeof(buf), "open(%s) failed: %s", path, strerror(errno));
        log_msg(buf);
        return -1;
    }
    if (fd != target_fd) {
        if (dup2(fd, target_fd) < 0) {
            char buf[512];
            snprintf(buf, sizeof(buf), "dup2(%d -> %d) failed: %s", fd, target_fd, strerror(errno));
            log_msg(buf);
            close(fd);
            return -1;
        }
        close(fd);
    }
    return target_fd;
}

int main(int argc, char *argv[]) {
    const char *prefix = getenv("PRESSURE_VESSEL_PREFIX");
    const char *runtime_base = getenv("PRESSURE_VESSEL_RUNTIME_BASE");

    if (!prefix || !*prefix) {
        log_msg("PRESSURE_VESSEL_PREFIX not set, passing through");
        goto exec_real;
    }

    char cmd_path[512], resp_path[512];
    snprintf(cmd_path, sizeof(cmd_path), "%s/cmd.fifo", prefix);
    snprintf(resp_path, sizeof(resp_path), "%s/resp.fifo", prefix);

    char buf[512];
    snprintf(buf, sizeof(buf), "opening FIFOs from %s", prefix);
    log_msg(buf);

    /* Open cmd.fifo as FD 3 (read end — child reads CDP commands) */
    if (open_fifo_at_fd(cmd_path, 3, O_RDONLY) < 0) {
        log_msg("failed to open cmd.fifo, falling through");
        goto exec_real;
    }

    /* Open resp.fifo as FD 4 (write end — child writes CDP responses) */
    if (open_fifo_at_fd(resp_path, 4, O_WRONLY) < 0) {
        log_msg("failed to open resp.fifo, falling through");
        goto exec_real;
    }

    /* Unlink FIFOs after opening (they're now held by FDs) */
    unlink(cmd_path);
    unlink(resp_path);

    /* Unset so child doesn't try to use it again */
    unsetenv("PRESSURE_VESSEL_PREFIX");

    log_msg("FIFOs opened as FDs 3,4 — passing through to real unruntime");

exec_real:
    /* Find the real pressure-vessel-unruntime */
    const char *real_path = NULL;

    if (runtime_base && *runtime_base) {
        static char runtime_buf[1024];
        snprintf(runtime_buf, sizeof(runtime_buf),
                 "%s/pressure-vessel/bin/pressure-vessel-unruntime", runtime_base);
        if (access(runtime_buf, X_OK) == 0) {
            real_path = runtime_buf;
        }
    }

    if (!real_path) {
        /* Try common locations */
        static const char *candidates[] = {
            "/usr/lib/steamrt64/pv-runtime/steam-runtime-steamrt/pressure-vessel/bin/pressure-vessel-unruntime",
            "/usr/lib/pressure-vessel/pressure-vessel-unruntime",
            "/usr/bin/pressure-vessel-unruntime",
            NULL
        };
        for (const char **c = candidates; *c; c++) {
            if (access(*c, X_OK) == 0) {
                real_path = *c;
                break;
            }
        }
    }

    if (!real_path) {
        log_msg("ERROR: could not find real pressure-vessel-unruntime");
        /* Try to exec whatever was originally in argv[0] */
        if (argc > 0) {
            execv(argv[0], argv);
        }
        _exit(1);
    }

    char logbuf[512];
    snprintf(logbuf, sizeof(logbuf), "execing real unruntime: %s", real_path);
    log_msg(logbuf);

    /* Build new argv with --pass-fd for the CDP pipes */
    /* Count original args (skip argv[0] which is the shim path) */
    int new_argc = 3 + (argc - 1) + 1; /* prog + 2 pass-fd + original args + NULL */
    char **new_argv = malloc(sizeof(char*) * new_argc);
    if (!new_argv) {
        _exit(1);
    }

    int i = 0;
    new_argv[i++] = (char*)real_path;

    /* Pass the CDP pipe FDs through the container */
    new_argv[i++] = "--pass-fd=3";
    new_argv[i++] = "--pass-fd=4";

    /* Copy original args (skip argv[0]) */
    for (int j = 1; j < argc; j++) {
        new_argv[i++] = argv[j];
    }

    new_argv[i] = NULL;

    execv(real_path, new_argv);
    /* If exec fails, try with original argv */
    execv(real_path, argv);
    _exit(1);
}
