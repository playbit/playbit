#ifndef PLAYBIT_PROVIDER_BRIDGE_H
#define PLAYBIT_PROVIDER_BRIDGE_H
#include <stdint.h>
#include <stddef.h>
typedef struct Provider Provider;
char *pb_provider_open(const char *, Provider **, int32_t *, char **);
char *pb_provider_connect(const Provider *, const uint8_t *, const char *, const uint8_t *);
char *pb_provider_select(const Provider *, const uint8_t *, uint32_t);
char *pb_provider_copy_source(const uint8_t *, size_t);
char *pb_provider_prepare_copy(const Provider *, const uint8_t *, const uint8_t *, const uint8_t *, const uint8_t *);
char *pb_provider_advance(const Provider *, uint64_t *, uint64_t *);
char *pb_provider_flush(const Provider *);
char *pb_provider_request(const Provider *, const uint8_t *, size_t);
void pb_provider_stop(const Provider *);
void pb_provider_close(Provider *);
void pb_provider_response_free(char *);

#endif
