/*
 * Minimal libkrun embedder for the TSI port-map reproduction.
 *
 * It uses only the public libkrun C API (include/libkrun.h) and contains no
 * LLO code. It boots one microVM with:
 *   - an explicit vsock device with INET hijacking (KRUN_TSI_HIJACK_INET),
 *     which is the configuration under which a guest INET listener has any
 *     route to the host at all;
 *   - a port map supplied on the command line: either a list of
 *     "host:guest" entries, or an explicit EMPTY array (--empty-map), which
 *     the header documents as "no port from the guest will be exposed".
 *
 * It then execs the guest workload. krun_start_enter does not return.
 *
 * Usage:
 *   embedder --root DIR --exec PATH [--map host:guest ]... [--empty-map]
 *            [--log-level N] -- [GUEST_ARG ]...
 */

#include <errno.h>
#include <getopt.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <libkrun.h>

#define MAX_MAP 32

static const struct option long_options[] = {
    {"root", required_argument, NULL, 'r'},
    {"exec", required_argument, NULL, 'e'},
    {"map", required_argument, NULL, 'm'},
    {"empty-map", no_argument, NULL, 'E'},
    {"log-level", required_argument, NULL, 'l'},
    {NULL, 0, NULL, 0},
};

int main(int argc, char *const argv[])
{
    const char *root = NULL;
    const char *exec_path = NULL;
    const char *port_map[MAX_MAP + 1];
    int map_count = 0;
    bool empty_map = false;
    uint32_t log_level = KRUN_LOG_LEVEL_WARN;
    int err;
    int c;

    while ((c = getopt_long(argc, argv, "+r:e:m:El:", long_options, NULL)) != -1) {
        switch (c) {
        case 'r':
            root = optarg;
            break;
        case 'e':
            exec_path = optarg;
            break;
        case 'm':
            if (map_count >= MAX_MAP) {
                fprintf(stderr, "too many --map entries\n");
                return 2;
            }
            port_map[map_count++] = optarg;
            break;
        case 'E':
            empty_map = true;
            break;
        case 'l':
            log_level = (uint32_t)strtoul(optarg, NULL, 10);
            break;
        default:
            fprintf(stderr, "usage: %s --root DIR --exec PATH [--map h:g]... "
                            "[--empty-map] -- [GUEST_ARG]...\n", argv[0]);
            return 2;
        }
    }
    port_map[map_count] = NULL;

    if (!root || !exec_path) {
        fprintf(stderr, "both --root and --exec are required\n");
        return 2;
    }
    if (map_count > 0 && empty_map) {
        fprintf(stderr, "--map and --empty-map are mutually exclusive\n");
        return 2;
    }

    err = krun_init_log(KRUN_LOG_TARGET_DEFAULT, log_level, KRUN_LOG_STYLE_AUTO, 0);
    if (err) {
        errno = -err;
        perror("krun_init_log");
        return 1;
    }

    int ctx = krun_create_ctx();
    if (ctx < 0) {
        errno = -ctx;
        perror("krun_create_ctx");
        return 1;
    }

    if ((err = krun_set_vm_config(ctx, 1, 512))) {
        errno = -err;
        perror("krun_set_vm_config");
        return 1;
    }

    if ((err = krun_set_root(ctx, root))) {
        errno = -err;
        perror("krun_set_root");
        return 1;
    }

    /*
     * Make the vsock device and its TSI feature set explicit rather than
     * relying on libkrun's implicit-vsock heuristics, so the configuration is
     * identical across every upstream SHA under test.
     */
    if ((err = krun_disable_implicit_vsock(ctx))) {
        errno = -err;
        perror("krun_disable_implicit_vsock");
        return 1;
    }
    if ((err = krun_add_vsock(ctx, KRUN_TSI_HIJACK_INET))) {
        errno = -err;
        perror("krun_add_vsock");
        return 1;
    }

    /*
     * Always call krun_set_port_map so we exercise the "called with an array"
     * contract, never the NULL (expose-everything) default. An empty array is
     * documented to expose nothing.
     */
    if ((err = krun_set_port_map(ctx, &port_map[0]))) {
        errno = -err;
        perror("krun_set_port_map");
        return 1;
    }

    if ((err = krun_set_workdir(ctx, "/"))) {
        errno = -err;
        perror("krun_set_workdir");
        return 1;
    }

    /* Remaining argv after "--" are the guest's argv (argv[0] = program). */
    const char *const *guest_argv = (const char *const *)&argv[optind];
    const char *const envp[] = {NULL};
    if ((err = krun_set_exec(ctx, exec_path, guest_argv, envp))) {
        errno = -err;
        perror("krun_set_exec");
        return 1;
    }

    /* Signal readiness to the host before entering the VM, mirroring the
     * convention the harness waits on (a line on stderr). The guest's own
     * marker file is the authoritative "listeners are up" signal. */
    fprintf(stderr, "embedder: ready\n");
    fflush(stderr);

    if ((err = krun_start_enter(ctx))) {
        errno = -err;
        perror("krun_start_enter");
        return 1;
    }
    return 0; /* not reached */
}
