#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <stdlib.h>
#include "../include/slate.h"

int main() {
    printf("Running ffi_smoke...\n");
    system("rm -rf ./ffi_smoke_db");
    system("mkdir -p ./ffi_smoke_db");

    uint8_t root_key[32] = {0};
    memset(root_key, 0x42, 32);

    slate_options opts = {
        .capacity_bytes = 1024 * 1024,
        .max_keys = 100,
        .b_commit = 1,
        .theta = 0,
        .profile = 0 // Pi
    };

    slate_db* db = NULL;
    int32_t rc = slate_open("./ffi_smoke_db", root_key, &opts, &db);
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
        printf("Expected TAMPERED (-10), got %d\n", rc);
        return 1;
    }
    
    // Get error message
    char err_buf[256];
    if (db) {
        slate_last_error_message(db, err_buf, sizeof(err_buf));
        printf("Tamper error message: %s\n", err_buf);
        slate_close(db);
    }

    printf("Smoke test PASSED\n");
    return 0;
}
