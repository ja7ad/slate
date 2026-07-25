#ifndef SLATE_H
#define SLATE_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdint.h>
#include <stddef.h>

#define SLATE_OK 0

#define SLATE_ERR_NOT_FOUND -1

#define SLATE_ERR_BUFFER_TOO_SMALL -2

#define SLATE_ERR_INVALID_ARG -3

#define SLATE_ERR_TAMPERED -10

#define SLATE_ERR_ROLLBACK -11

#define SLATE_ERR_INTERNAL -99

#define SLATE_ERR_IO -100

#define SLATE_ABI_VERSION_MAJOR 1

#define SLATE_ABI_VERSION_MINOR 0

/**
 * `slate_options::profile` selector: Raspberry-Pi-class host (report §9: B = 9).
 */
#define SLATE_PROFILE_PI 0

/**
 * `slate_options::profile` selector: ESP32-class device (report §9: B = 27).
 */
#define SLATE_PROFILE_ESP32 1

typedef struct slate_db slate_db;

typedef struct slate_options {
  uint64_t capacity_bytes;
  uint32_t max_keys;
  uint32_t b_commit;
  uint32_t theta;
  uint8_t profile;
} slate_options;

uint32_t slate_abi_version(void);

int32_t slate_open(const char *path,
                   const uint8_t *key,
                   const struct slate_options *opts,
                   struct slate_db **out);

int32_t slate_put(struct slate_db *db,
                  const uint8_t *k,
                  uintptr_t klen,
                  const uint8_t *v,
                  uintptr_t vlen);

int32_t slate_put_durable(struct slate_db *db,
                          const uint8_t *k,
                          uintptr_t klen,
                          const uint8_t *v,
                          uintptr_t vlen);

int32_t slate_get(struct slate_db *db,
                  const uint8_t *k,
                  uintptr_t klen,
                  uint8_t *v_out,
                  uintptr_t *vlen_inout);

int32_t slate_delete(struct slate_db *db, const uint8_t *k, uintptr_t klen);

int32_t slate_commit(struct slate_db *db);

int32_t slate_compact(struct slate_db *db);

int32_t slate_security_mode(struct slate_db *db);

int32_t slate_close(struct slate_db *db);

uintptr_t slate_last_error_message(struct slate_db *db, char *buf, uintptr_t len);

#endif  /* SLATE_H */
