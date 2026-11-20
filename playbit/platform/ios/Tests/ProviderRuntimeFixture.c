// A failed native connect publishes readiness before returning, just as the
// real account connection does. Run the production Swift/GCD owner against it.
#include "ProviderRuntimeFixture.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

struct Provider { int reader; };
static atomic_int reader = -1;
static atomic_int writer = -1;
static atomic_int attempts;
static atomic_int advances;
static atomic_int generation;
static atomic_int advance_error;
static atomic_int flushes;
static atomic_int pending_publications;

int provider_fixture_attempts(void) { return atomic_load(&attempts); }
int provider_fixture_advances(void) { return atomic_load(&advances); }
int provider_fixture_flushes(void) { return atomic_load(&flushes); }
int provider_fixture_pending_publications(void) { return atomic_load(&pending_publications); }
void provider_fixture_queue_publications(void) { atomic_store(&pending_publications, 3); }
void provider_fixture_reconnect(void) {
    atomic_store(&advance_error, 1);
    atomic_fetch_add(&generation, 1);
    provider_fixture_notify();
}
int provider_fixture_readable(void) {
    struct pollfd fd = { .fd = atomic_load(&reader), .events = POLLIN };
    return poll(&fd, 1, 0) > 0 && (fd.revents & (POLLIN | POLLHUP)) != 0;
}
void provider_fixture_notify(void) {
    const unsigned char byte = 1;
    assert(write(atomic_load(&writer), &byte, 1) == 1);
}
void provider_fixture_close_writer(void) {
    int fd = atomic_exchange(&writer, -1);
    if (fd >= 0) { assert(close(fd) == 0); }
}
char *pb_provider_open(const char *path,
    Provider **output, int32_t *descriptor, char **baseline) {
    (void)path;
    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    assert(fcntl(sockets[0], F_SETFL, O_NONBLOCK) == 0);
    assert(fcntl(sockets[1], F_SETFL, O_NONBLOCK) == 0);
    Provider *provider = calloc(1, sizeof(*provider));
    assert(provider);
    provider->reader = sockets[0];
    atomic_store(&reader, sockets[0]);
    atomic_store(&writer, sockets[1]);
    atomic_store(&attempts, 0);
    atomic_store(&advances, 0);
    atomic_store(&generation, 1);
    atomic_store(&advance_error, 0);
    atomic_store(&flushes, 0);
    atomic_store(&pending_publications, 0);
    *output = provider;
    *descriptor = dup(sockets[0]);
    assert(*descriptor >= 0);
    *baseline = strdup("{\"anchor\":null,\"revision\":0}");
    return NULL;
}
char *pb_provider_connect(const Provider *provider, const uint8_t *account,
    const char *email, const uint8_t *secret) {
    (void)provider; (void)account; (void)email; (void)secret;
    provider_fixture_notify();
    return atomic_fetch_add(&attempts, 1) == 0 ? strdup("fixture AP unavailable") : NULL;
}
char *pb_provider_select(const Provider *provider, const uint8_t *volumes, uint32_t count) {
    (void)provider; (void)volumes; assert(count <= 2); return NULL;
}
char *pb_provider_advance(const Provider *provider, uint64_t *deadline, uint64_t *connection) {
    unsigned char buffer[256];
    for (int count = 0; count < 64; count++) {
        ssize_t length = read(provider->reader, buffer, sizeof(buffer));
        if (length > 0 || (length < 0 && errno == EINTR)) { continue; }
        assert(length < 0 && (errno == EAGAIN || errno == EWOULDBLOCK));
        break;
    }
    atomic_fetch_add(&advances, 1);
    *deadline = UINT64_MAX; *connection = (uint64_t)atomic_load(&generation);
    return atomic_exchange(&advance_error, 0) ? strdup("fixture sibling volume unavailable") : NULL;
}
static int contains(const uint8_t *bytes, size_t length, const char *token) {
    size_t width = strlen(token);
    for (size_t offset = 0; offset + width <= length; offset++) {
        if (memcmp(bytes + offset, token, width) == 0) { return 1; }
    }
    return 0;
}
char *pb_provider_request(const Provider *provider, const uint8_t *bytes, size_t length) {
    (void)provider;
    if (atomic_load(&attempts) < 2 && contains(bytes, length, "\"root\"")) {
        return strdup("{\"error\":\"access denied: File Provider account is no longer signed in\",\"code\":\"permission_denied\"}");
    }
    if (contains(bytes, length, "\"catalog\"")) {
        if (atomic_load(&attempts) < 2) {
            return strdup("{\"error\":\"access denied: File Provider account catalog\",\"code\":\"permission_denied\"}");
        }
        return strdup("{\"items\":[{\"id\":\"GsLrMUhj8fYeuSijQRNh4B\",\"filename\":\"Discovered\",\"role\":\"volume\",\"kind\":\"directory\"}],\"retired\":[]}");
    }
    if (contains(bytes, length, "\"volume_item\"")) {
        return strdup("{\"error\":\"access denied: File Provider personal volume\",\"code\":\"permission_denied\"}");
    }
    return strdup("{\"retired\":[],\"anchor\":null,\"revision\":0}");
}
char *pb_provider_flush(const Provider *provider) {
    (void)provider;
    atomic_store(&pending_publications, 0);
    atomic_fetch_add(&flushes, 1);
    return NULL;
}
char *pb_provider_copy_source(const uint8_t *bytes, size_t length) {
    (void)bytes; (void)length; assert(0); return NULL;
}
char *pb_provider_prepare_copy(const Provider *provider, const uint8_t *account,
    const uint8_t *destination, const uint8_t *volume, const uint8_t *entry) {
    (void)provider; (void)account; (void)destination; (void)volume; (void)entry; assert(0); return NULL;
}
void pb_provider_stop(const Provider *provider) { (void)provider; }
void pb_provider_close(Provider *provider) {
    atomic_store(&reader, -1);
    assert(close(provider->reader) == 0);
    provider_fixture_close_writer();
    free(provider);
}
void pb_provider_response_free(char *response) { free(response); }
