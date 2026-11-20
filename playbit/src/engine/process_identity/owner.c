// A small test peer owns one profile socket and the same flock as LocalStore.
// It can retain SIGTERM so reset exercises identity-bound SIGKILL and lock release.
#define _POSIX_C_SOURCE 200809L
#define _DARWIN_C_SOURCE 1
#define _DEFAULT_SOURCE 1
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static void fail(const char *operation) {
    perror(operation);
    exit(1);
}

static void retain_engine(int signal_number) {
    (void)signal_number;
    int saved = errno;
    const char message[] = "engine-fixture-term\n";
    // The write is async-signal-safe; preserve the interrupted operation's errno.
    ssize_t result = write(STDOUT_FILENO, message, sizeof(message) - 1);
    (void)result;
    errno = saved;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        return 1;
    }
    char path[4096];
    int length = snprintf(path, sizeof(path), "%s/control/local.lock", argv[1]);
    if (length < 0 || (size_t)length >= sizeof(path)) {
        return 1;
    }
    int lock = open(path, O_RDWR);
    if (lock < 0 || flock(lock, LOCK_EX | LOCK_NB) != 0) {
        fail("profile lock");
    }
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    length = snprintf(address.sun_path, sizeof(address.sun_path), "%s/control/engine.sock", argv[1]);
    if (length < 0 || (size_t)length >= sizeof(address.sun_path)) {
        return 1;
    }
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        listen(listener, 16) != 0) {
        fail("profile socket");
    }
    struct sigaction action = {
        .sa_handler = argv[2][0] == '1' ? retain_engine : SIG_DFL,
        .sa_flags = SA_RESTART,
    };
    if (sigemptyset(&action.sa_mask) != 0 || sigaction(SIGTERM, &action, NULL) != 0) {
        fail("stop handler");
    }
    puts("engine-fixture-ready");
    if (fflush(stdout) != 0) {
        fail("readiness");
    }
    char byte;
    while (read(STDIN_FILENO, &byte, 1) < 0) {
        if (errno != EINTR) {
            fail("parent input");
        }
    }
    return 0;
}
