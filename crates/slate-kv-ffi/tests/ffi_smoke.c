#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <stdlib.h>
#include "../include/slate.h"

int main() {
    printf("Running ffi_smoke...\n");
    system("rm -rf ./ffi_smoke_db");
    system("mkdir -p ./ffi_smoke_db");

    // H3: check ABI version
    uint32_t ver = slate_abi_version();
    printf("ABI version: %u.%u\n", ver >> 16, ver & 0xFFFF);
    if ((ver >> 16) != SLATE_ABI_VERSION_MAJOR || (ver & 0xFFFF) != SLATE_ABI_VERSION_MINOR) {
        printf("ABI version mismatch!\n");
        return 1;
    }

    uint8_t root_key[32] = {0};
    memset(root_key, 0x42, 32);

    // H1: check capacity truncation rejection
    slate_options bad_opts = {
        .capacity_bytes = (uint64_t)0xFFFFFFFFULL + 10ULL,
        .max_keys = 100,
        .b_commit = 1,
        .theta = 0,
        .profile = 0 // Pi
    };
    slate_db* bad_db = NULL;
    int32_t rc = slate_open("./ffi_smoke_db", root_key, &bad_opts, &bad_db);
    if (rc != SLATE_ERR_INVALID_ARG) {
        printf("Expected INVALID_ARG (-3) for oversized capacity, got %d\n", rc);
        return 1;
    }

    slate_options opts = {
        .capacity_bytes = 1024 * 1024,
        .max_keys = 100,
        .b_commit = 1,
        .theta = 0,
        .profile = 0 // Pi
    };

    slate_db* db = NULL;
    rc = slate_open("./ffi_smoke_db", root_key, &opts, &db);
    if (rc != SLATE_OK) {
        printf("slate_open failed: %d\n", rc);
        return 1;
    }

    // Verify security mode
    int32_t sec = slate_security_mode(db);
    if (sec != 1) { // BEST_EFFORT
        printf("Expected security mode 1, got %d\n", sec);
        return 1;
    }

    // put_durable
    const char* k = "mykey";
    const char* v = "myvalue";
    rc = slate_put_durable(db, (const uint8_t*)k, strlen(k), (const uint8_t*)v, strlen(v));
    if (rc != SLATE_OK) {
        printf("slate_put_durable failed: %d\n", rc);
        return 1;
    }

    // H2: klen == 0 should return INVALID_ARG
    rc = slate_put_durable(db, (const uint8_t*)"", 0, (const uint8_t*)"val", 3);
    if (rc != SLATE_ERR_INVALID_ARG) {
        printf("Expected INVALID_ARG for klen == 0, got %d\n", rc);
        return 1;
    }

    // H2: empty value round-trip (vlen == 0, v == NULL is allowed when vlen == 0)
    const char* k_empty = "empty_key";
    rc = slate_put_durable(db, (const uint8_t*)k_empty, strlen(k_empty), NULL, 0);
    if (rc != SLATE_OK) {
        printf("slate_put_durable with empty value failed: %d\n", rc);
        return 1;
    }
    uint8_t empty_out[16];
    uintptr_t empty_len = sizeof(empty_out);
    rc = slate_get(db, (const uint8_t*)k_empty, strlen(k_empty), empty_out, &empty_len);
    if (rc != SLATE_OK || empty_len != 0) {
        printf("slate_get empty value failed: rc=%d len=%lu\n", rc, (unsigned long)empty_len);
        return 1;
    }

    // get
    uint8_t v_out[64];
    uintptr_t v_len = sizeof(v_out);
    rc = slate_get(db, (const uint8_t*)k, strlen(k), v_out, &v_len);
    if (rc != SLATE_OK) {
        printf("slate_get failed: %d\n", rc);
        return 1;
    }
    
    v_out[v_len] = '\0';
    if (strcmp((char*)v_out, v) != 0) {
        printf("Value mismatch: %s != %s\n", v_out, v);
        return 1;
    }

    // Two-call pattern (BUFFER_TOO_SMALL)
    uintptr_t v_len_small = 2;
    rc = slate_get(db, (const uint8_t*)k, strlen(k), v_out, &v_len_small);
    if (rc != SLATE_ERR_BUFFER_TOO_SMALL) {
        printf("slate_get did not return BUFFER_TOO_SMALL: %d\n", rc);
        return 1;
    }
    if (v_len_small != strlen(v)) {
        printf("slate_get didn't return needed len: %lu\n", (unsigned long)v_len_small);
        return 1;
    }

    // close
    slate_close(db);
    db = NULL;

    // Tamper the file bytes
    printf("Tampering file...\n");
    FILE* f = fopen("./ffi_smoke_db/counter.bin", "r+b");
    if (!f) return 1;
    fseek(f, 0, SEEK_SET); // Corrupt slot 0
    fputc(0xFF, f);
    fseek(f, 40, SEEK_SET); // Corrupt slot 1
    fputc(0xFF, f);
    fclose(f);

    // Reopen -> expect TAMPERED
    rc = slate_open("./ffi_smoke_db", root_key, &opts, &db);
    if (rc != SLATE_ERR_TAMPERED) {
        char err_buf[256] = {0};
        slate_last_error_message(NULL, err_buf, sizeof(err_buf));
        printf("Expected TAMPERED (-10), got %d. Open error: %s\n", rc, err_buf);
        return 1;
    }
    
    // Get error message with db == NULL (H4)
    char err_buf[256] = {0};
    size_t msg_len = slate_last_error_message(NULL, err_buf, sizeof(err_buf));
    printf("Tamper error message (db=NULL): %s (len=%zu)\n", err_buf, msg_len);
    if (msg_len == 0 || strlen(err_buf) == 0) {
        printf("Expected non-empty open-time error message when db=NULL\n");
        return 1;
    }
    if (db) {
        slate_close(db);
    }

    printf("Smoke test PASSED\n");
    return 0;
}
