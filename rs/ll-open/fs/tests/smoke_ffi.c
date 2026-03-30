/*
 * C smoke test for leyline-fs FFI.
 *
 * This test links against libleyline_fs and exercises the full handle lifecycle:
 *   leyline_open → get_node / list_children / lookup_child / read_content → leyline_close
 *
 * Build & run (from rs/):
 *   cargo build
 *   cc -o /tmp/smoke_ffi crates/fs/tests/smoke_ffi.c \
 *      -I crates/fs/include \
 *      -L target/debug -lleyline_fs \
 *      -framework Security -framework CoreFoundation \
 *      -lSystem -lresolv -lc
 *   /tmp/smoke_ffi /path/to/control_file
 *
 * For automated testing, the Rust integration test (smoke_ffi_integration)
 * creates a temp arena and invokes this binary.
 */

#include <stdio.h>
#include <string.h>
#include "leyline_fs.h"

#define BUF_SIZE 4096

static int failures = 0;

#define CHECK(cond, msg) do { \
    if (!(cond)) { \
        fprintf(stderr, "FAIL: %s\n", msg); \
        failures++; \
    } else { \
        printf("  ok: %s\n", msg); \
    } \
} while (0)

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s <control-path>\n", argv[0]);
        return 1;
    }

    printf("=== leyline-fs C smoke test ===\n");

    /* 1. Open */
    struct LeylineCtx *ctx = leyline_open(argv[1]);
    CHECK(ctx != NULL, "leyline_open returned non-null handle");
    if (!ctx) return 1;

    uint8_t buf[BUF_SIZE];
    int32_t n;

    /* 2. get_node — root (empty string) */
    n = leyline_get_node(ctx, "", buf, BUF_SIZE);
    CHECK(n > 0, "get_node root returns data");
    if (n > 0) {
        buf[n] = '\0';
        printf("    root: %s\n", buf);
        CHECK(strstr((char*)buf, "\"kind\":1") != NULL, "root is a directory (kind=1)");
    }

    /* 3. list_children — root should have children */
    n = leyline_list_children(ctx, "", buf, BUF_SIZE);
    CHECK(n > 0, "list_children root returns data");
    if (n > 0) {
        buf[n] = '\0';
        printf("    children: %s\n", buf);
        CHECK(buf[0] == '[', "children is a JSON array");
    }

    /* 4. lookup_child — find "vulns" under root */
    n = leyline_lookup_child(ctx, "", "vulns", buf, BUF_SIZE);
    CHECK(n > 0, "lookup_child('', 'vulns') found");
    if (n > 0) {
        buf[n] = '\0';
        CHECK(strstr((char*)buf, "\"id\":\"vulns\"") != NULL, "child id is 'vulns'");
    }

    /* 5. read_content — leaf node */
    n = leyline_read_content(ctx, "vulns/CVE-1", buf, BUF_SIZE, 0);
    CHECK(n > 0, "read_content returns data");
    if (n > 0) {
        buf[n] = '\0';
        printf("    content: %s\n", buf);
    }

    /* 6. Null handle returns -2 */
    n = leyline_get_node(NULL, "vulns", buf, BUF_SIZE);
    CHECK(n == -2, "null ctx returns -2");

    /* 7. Missing node returns -1 */
    n = leyline_get_node(ctx, "nonexistent", buf, BUF_SIZE);
    CHECK(n == -1, "missing node returns -1");

    /* 8. Close */
    leyline_close(ctx);
    printf("  ok: leyline_close succeeded\n");

    /* 9. Close null is safe */
    leyline_close(NULL);
    printf("  ok: leyline_close(NULL) is safe\n");

    printf("\n=== %s (%d failures) ===\n", failures == 0 ? "PASS" : "FAIL", failures);
    return failures > 0 ? 1 : 0;
}
