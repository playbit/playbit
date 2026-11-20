// Passive Finder double-click timing. Does not launch apps or synthesize input.
// . scripts/macos-env.sh
// clang -O2 -Wall -Wextra -Werror -framework ApplicationServices \
//     scripts/observe-macos-launch.c -o /tmp/observe-macos-launch
#include <ApplicationServices/ApplicationServices.h>
#include <errno.h>
#include <limits.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>

typedef struct {
    uint64_t event_ns, observed_ns;
    CGEventType type;
    int64_t clicks;
} Observation;

static Observation events[4];
static unsigned count;
static bool invalid;
static mach_timebase_info_data_t timebase;

static CGEventRef observe(CGEventTapProxy proxy, CGEventType type,
    CGEventRef event, void *context) {
    (void)proxy;
    (void)context;
    if (type == kCGEventTapDisabledByTimeout || type == kCGEventTapDisabledByUserInput) {
        invalid = true;
        CFRunLoopStop(CFRunLoopGetCurrent());
        return event;
    }
    int64_t clicks = CGEventGetIntegerValueField(event, kCGMouseEventClickState);
    // Start again on an unrelated first click; accept only a complete pair.
    if (type == kCGEventLeftMouseDown && clicks == 1) count = 0;
    if (count == 4 || type != (count % 2 ? kCGEventLeftMouseUp : kCGEventLeftMouseDown)
        || clicks != (int64_t)(count / 2 + 1)) {
        count = 0;
        return event;
    }
    uint64_t now = (uint64_t)((__uint128_t)mach_absolute_time() * timebase.numer / timebase.denom);
    events[count++] = (Observation){CGEventGetTimestamp(event), now, type, clicks};
    if (count == 4) CFRunLoopStop(CFRunLoopGetCurrent());
    return event;
}

int main(int argc, char **argv) {
    char *end;
    errno = 0;
    long pid = argc == 2 ? strtol(argv[1], &end, 10) : 0;
    if (argc != 2 || errno || !pid || pid < 0 || pid > INT_MAX || *end) {
        fprintf(stderr, "usage: observe-macos-launch FINDER_PID\n");
        return 2;
    }
    if (mach_timebase_info(&timebase) != KERN_SUCCESS || !timebase.denom) return 1;
    CFMachPortRef tap = CGEventTapCreateForPid((pid_t)pid, kCGHeadInsertEventTap,
        kCGEventTapOptionListenOnly,
        CGEventMaskBit(kCGEventLeftMouseDown) | CGEventMaskBit(kCGEventLeftMouseUp), observe, NULL);
    if (!tap) {
        fprintf(stderr, "Passive Finder observation unavailable; no timing captured\n");
        return 1;
    }
    CFRunLoopSourceRef source = CFMachPortCreateRunLoopSource(NULL, tap, 0);
    if (!source) { CFRelease(tap); return 1; }
    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
    fprintf(stderr, "Ready: double-click the verified test app in Finder within 60 seconds\n");
    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 60, false);
    CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
    CFRelease(source);
    CFRelease(tap);
    if (invalid || count != 4) {
        fprintf(stderr, "Incomplete or disabled observation; no launch sample\n");
        return 1;
    }
    // No callback I/O, key data, cursor coordinates, or unbounded event queue.
    printf("{\"format\":1,\"clock\":\"mach_absolute_time\",\"finder_pid\":%ld,\"events\":[", pid);
    for (unsigned i = 0; i < 4; i++) {
        printf("%s{\"event_ns\":%llu,\"observed_ns\":%llu,\"type\":%u,\"click_count\":%lld}",
            i ? "," : "", (unsigned long long)events[i].event_ns,
            (unsigned long long)events[i].observed_ns, events[i].type, (long long)events[i].clicks);
    }
    puts("]}");
    return ferror(stdout) ? 1 : 0;
}
