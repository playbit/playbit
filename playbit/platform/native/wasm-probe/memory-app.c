// Compiler-laid-out mutation inputs must be owned when Begin returns.
#include <playbit/sys/abi.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}


uint64_t probe_memory_begin(uint64_t volume, int32_t expected_platform, uint32_t edit) {
    PBContext *context = pb_guest_context();
    if (pb_host_platform(context) != expected_platform || edit > 1)
        return 0;
    PBIdentity identity = {.size=sizeof(PBIdentity)};
    if (pb_identity(context, &identity) != PB_OK || identity.account_id[0] != 1)
        return 0;
    PBEntryHandle entry = {.size=sizeof(PBEntryHandle),.volume=volume};
    for (unsigned i = 0; i < 15; i++) entry.entry_id[i] = 5;
    PBId type = {0}, object = {0};
    PBValueInput value = {.size=sizeof(PBValueInput),.kind=PB_TEXT,.length=8,.data=(const uint8_t *)"compiled"};
    PBFieldInput field = {.size=sizeof(PBFieldInput),.values=&value,.count=1,.present=1,.stride=sizeof(PBValueInput)};
    for (uint32_t i = 0; i < 16; i++) {
        type[i] = 8;
        object[i] = 9;
        field.id[i] = 7;
    }
    PBOperationHandle operation = 0;
    PBStatus result;
    if (edit) {
        value = (PBValueInput){.size=sizeof(PBValueInput),.kind = PB_BOOLEAN, .scalar = 1};
        result = probe_handle(pb_object_edit_begin(context, &entry, &object, &field, 1, sizeof(PBFieldInput)), &operation);
    } else {
        result = probe_handle(pb_object_create_begin(context, &entry, &type, &field, 1, sizeof(PBFieldInput)), &operation);
    }
    return result == PB_OK ? operation : 0;
}
int32_t probe_memory_take(uint32_t generation, uint32_t slot, uint32_t edit) {
    PBContext *context = pb_guest_context();
    PBOperationHandle operation = pb_handle_pack(generation, slot);
    if (edit)
        return pb_object_edit_take(context, operation);
    PBId object = {0};
    PBStatus result = pb_object_create_poll(context, operation, &object);
    if (result != PB_OK)
        return result;
    for (uint32_t i = 0; i < 16; i++) {
        if (object[i] != 9)
            return PB_INTERNAL;
    }
    return PB_OK;
}
