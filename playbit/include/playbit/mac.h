#ifndef PLAYBIT_MAC_H
#define PLAYBIT_MAC_H
#include <playbit/sys/abi.h>
#ifdef __cplusplus
extern "C" {
#endif

// Run one statically linked app using local storage and the shared Mac renderer.
// Call once on the process's main thread. This owns the AppKit event loop until
// quit. Normal AppKit termination joins the worker, destroys app state, then
// exits the process; register final process cleanup with atexit. No VM,
// IPC helper, AP connection or dynamic app loading is constructed.
//
// Inputs: an absolute UTF-8 profile path (1..4096 bytes), UTF-8 title (1..256
// bytes, not blank), root declaration JSON (1..65536 bytes), and complete PBApp.
// Strings have no NUL terminator and may not contain embedded NUL bytes. Every
// input allocation must be readable and stable through initial admission; the
// host copies it before invoking callbacks. Bounds, text encoding and JSON decoding
// are checked before creating files or starting an event loop. All returned
// failures are PBStatus.
//
// The profile must be dedicated to this app. The host persists a newly generated
// root ID before atomic initialization. Reopening retains that ID and user edits;
// changed declarations or unrelated preexisting volumes fail, with no reset.
// Storage/initialization errors after launch appear in the application window.
// This function returns a PBStatus for admission/startup errors; ordinary Quit
// terminates the process. Native callbacks cannot unwind or promise Wasm trap isolation.
//
// App callbacks execute on one worker thread, never the main thread. The caller
// retains callback code and app.user_data, accessible from that worker, until
// process termination (or an early return). init transfers state to turn/destroy
// under the PBApp ownership rules.
// The host installs the native fail-stop panic policy. A second call, including
// during callbacks, returns PB_CONFLICT. Use playbit/native.h when embedding in an
// existing native application with its own UI and event loop.
PBStatus pb_mac_run(
    const uint8_t* profile,
    uint32_t       profile_length,
    const uint8_t* title,
    uint32_t       title_length,
    const uint8_t* declaration,
    uint32_t       declaration_length,
    const PBApp*   app);
// Run a portable app with sign-in, account recovery and create/join-session UI.
// This uses the same thread, pointer and callback lifetime rules as pb_mac_run.
// The absolute UTF-8 path names a canonical .pbapp; expected_digest has 32 bytes.
// The package is verified and its file backing retained. Its exact contents identify the
// Wasm counterpart distributed through AP; this process never executes that Wasm.
// The caller guarantees that app implements this exact package. Its embedded
// initializer is validated before creating files or UI.
// A null profile with zero length selects the runtime channel/profile through
// PLAYBIT_CHANNEL, PLAYBIT_PROFILE and this installation's remembered selection.
// An explicit profile is isolated and cannot be reinterpreted as another channel.
//
// A fresh dedicated profile signs in and saves/restores its account key before
// callbacks receive account-bound services. Existing local-only data is retained
// and rejected for enrollment; choose a separate profile. The selected session is
// durable, printed with its invitation, and reopened locally on later launches.
// Local input never waits for AP work. Exact build mismatches are explicit errors.
PBStatus pb_mac_run_portable(
    const uint8_t* profile,
    uint32_t       profile_length,
    const uint8_t* package_path,
    uint32_t       package_path_length,
    const uint8_t* expected_digest,
    const PBApp*   app);
#ifdef __cplusplus
}
#endif
#endif
