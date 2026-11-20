#ifndef PLAYBIT_NATIVE_H
#define PLAYBIT_NATIVE_H
#include <playbit/engine.h>
#ifdef __cplusplus
extern "C" {
#endif

// Concrete local native-app composition: local-store + native-runtime.
// Engine ownership/thread/lifetime rules in engine.h also apply here. Four apps
// may be attached per engine. An app generation is never reused; it cannot name
// an app in another engine. Scoped PBContext calls only work inside init/turn.
// Root must be a volume owned by this engine's account. This local composition
// has no session, resource package, AP or Wasm hot replacement. Apps discover
// the supported operations through pb_capabilities; unsupported calls fail.
// Guest I/O uses Begin/Take. Data workers may signal the engine's thread-safe
// wake callback; no guest callback runs until the owner advances it again.
typedef void (*PBEngineAppView)(
    void* user_data, uint64_t generation, const uint8_t* scene, uint32_t length);
typedef void (*PBEngineAppLog)(
    void* user_data, uint64_t generation, const uint8_t* text, uint32_t length);
typedef struct PBEngineAppOutput {
    uint32_t size, written;
    PBEngineAppView view;
    PBEngineAppLog  log;
    void*           user_data;
} PBEngineAppOutput;
enum { PB_ENGINE_APP_READY = 1, PB_ENGINE_APP_TIMER = 2 };
typedef struct PBEngineAppStep {
    uint32_t size, written;
    uint64_t wake_after_ns;
    uint32_t flags, reserved;
} PBEngineAppStep;

// Copies the callback table, borrows app.user_data through destruction. No app
// callback runs during attach. PBApp's init/turn/destroy ownership rules apply.
PBStatus pb_engine_app_attach(PBEngine*, const PBId* root, const PBApp*, uint64_t* generation);
// Destroys initialized app state once, and retires the generation. Engine close
// also destroys all attached apps before releasing storage. Reentry into the
// engine from app init/turn/destroy or output callbacks returns PB_CONFLICT.
PBStatus pb_engine_app_detach(PBEngine*, uint64_t generation);
// UI input only: enabled target in the currently accepted semantic scene,
// matching generation and valid event payload. No system-event injection.
PBStatus pb_engine_app_input(PBEngine*, const PBEvent*, const uint8_t*, uint32_t length);
// One cooperative init/turn. Both output callbacks are required and their
// arguments are borrowed only during that callback. No callback may unwind.
// View is the existing bounded packed UI format, not a syscall envelope.
// Calls do not replay failed app turns. A failed app stays halted until detached.
PBStatus pb_engine_app_advance(
    PBEngine*, uint64_t generation, const PBEngineAppOutput*, PBEngineAppStep* output);
// Readiness without executing the app. Drain committed engine changes before
// consulting app readiness: affected roots and new feed gaps coalesce one data
// notification per app. Clear scheduled host readiness before drain. With READY,
// schedule another owner turn; with TIMER, arm the host timer for wake_after_ns.
// No periodic polling. Deadlines are relative to this call's completion boundary.
PBStatus pb_engine_app_step(PBEngine*, uint64_t generation, PBEngineAppStep* output);
#ifdef __cplusplus
}
#endif
#endif
