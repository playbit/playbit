#ifndef PLAYBIT_ENGINE_H
#define PLAYBIT_ENGINE_H
#include <playbit/sys/abi.h>
#ifdef __cplusplus
extern "C" {
#endif

// Trusted local engine management, distinct from an app's scoped PBContext.
// This composition owns a durable local store and a bounded change feed. It
// creates no VM, AP connection, worker, socket or helper process.
// Record size/written follow the addressed-record contract in sys/abi.h.
// Initialize each output size before calls; input written is publication metadata.
typedef struct PBEngine PBEngine;
typedef void (*PBEngineWake)(void* user_data);
typedef struct PBEngineVolume {
    uint32_t size, written;
    PBId     id, owner;
    uint8_t  name[256];
    uint32_t name_length;
    uint32_t share_key_with_service;
} PBEngineVolume;
enum {
    PB_ENGINE_DATABASE = 1,
    PB_ENGINE_BLOBLOG = 2,
    PB_ENGINE_VOLUME_AVAILABLE = 4
};
typedef struct PBEngineChange {
    uint32_t size, written;
    PBId     volume;
    uint64_t sequence;
    uint32_t kind;
    uint8_t database[16];
    uint32_t reserved;
} PBEngineChange;
typedef void (*PBEngineChangeFn)(
    void* user_data, const PBEngineChange*, const uint8_t* bytes, uint32_t byte_length);
typedef struct PBEngineChanges {
    uint32_t size, written;
    PBId     epoch;
    uint64_t gap_generation, account_generation;
    // 0=open, 1=source dropped, 2=receiver dropped, 3=notification failed,
    // 4=sequence exhausted. A nonzero value is terminal for this feed.
    uint32_t closed, records, remaining, reserved;
} PBEngineChanges;

// All calls belong to the thread that opens the engine. Calls on another thread
// and reentrant calls return PB_CONFLICT. Callers must synchronize close with all
// uses; a copied pointer is invalid after successful close. Invalid allocation
// addresses cannot be checked: every nonempty span/fixed output must be live,
// suitably aligned and readable/writable for this call, disjoint from engine
// storage and each other. Inputs are stable for the call and never retained.
// Null/misaligned/overflowing spans and overlapping inputs/outputs are rejected
// before effects. Failures leave outputs unchanged. No callback may unwind.
// Internal invariant failures terminate the process; errors are not panics.
//
// Path is nonempty UTF-8, <=4096 bytes, no embedded NUL, and absolute. Opening may
// create the profile. Wake/user_data are borrowed through close, and must be valid
// during open as well. Wake may run on the owner or an asynchronous app data
// worker, and may overlap another wake. It must be thread-safe and only schedule
// a later owner event-loop turn. Do not drain, mutate or close from wake. Clear the host's
// scheduled flag before draining; remaining records rearm wake without polling.
PBStatus pb_engine_open(
    const uint8_t* path,
    uint32_t       path_length,
    PBEngineWake   wake,
    void*          user_data,
    PBEngine**     output);
PBStatus pb_engine_close(PBEngine** engine); // Success consumes and nulls *engine.
PBStatus pb_engine_account(PBEngine*, PBId* output);
// Create an owned volume with its database key shared with AP. Acknowledge only
// after durable commit; no automatic retry on errors. Label is 1..256 UTF-8 bytes.
PBStatus pb_engine_volume_create(
    PBEngine*, const uint8_t* label, uint32_t label_length, PBEngineVolume* output);
PBStatus pb_engine_volume_read(PBEngine*, const PBId*, PBEngineVolume* output);
// Consume at most eight records. Callback arguments are borrowed only for that
// callback; copy anything retained. Callback cannot reject/retry a consumed
// record. The sticky epoch/gap/closed fields must be inspected even with no records:
// a gap requires re-reading durable state, never treating the batch as complete.
// A full feed retains at most 256 records/1 MiB. Payloads remain charged while
// callbacks run. Records are committed changes, not remote acknowledgments.
PBStatus pb_engine_drain(
    PBEngine*, PBEngineChangeFn callback, void* user_data, PBEngineChanges* output);
#ifdef __cplusplus
}
#endif
#endif
