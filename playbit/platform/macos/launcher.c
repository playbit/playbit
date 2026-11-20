// Main-thread entry for Playbit and generated guests. Fill the bounded runtime
// path section before signing; recipients need no compiler.
#include <dlfcn.h>
#include <stdio.h>

#ifndef PLAYBIT_RUNTIME_PATH
#define PLAYBIT_RUNTIME_PATH "UNCONFIGURED_PLAYBIT_RUNTIME"
#endif

__attribute__((used, section("__TEXT,__pb_runtime")))
static const char runtime_path[4096] = PLAYBIT_RUNTIME_PATH;

int main(void) {
    void *runtime = dlopen(runtime_path, RTLD_NOW | RTLD_LOCAL);
    if (!runtime) {
        fprintf(stderr, "Cannot load Playbit runtime: %s\n", dlerror());
        return 1;
    }
    void (*entry)(void) = (void (*)(void))dlsym(runtime, "playbit_main");
    if (!entry) {
        fprintf(stderr, "Cannot find Playbit entry point: %s\n", dlerror());
        return 1;
    }
    entry();
    return 0;
}
