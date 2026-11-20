#include <playbit/sys/abi.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}


uint32_t probe_results(void) {
    PBContext *context = pb_guest_context();
    PBRange names[32] = {0};
    for (uint32_t i = 0; i < 32; i++) names[i].size = sizeof(PBRange);
    uint8_t text[4096] = {0};
    PBCapabilities capabilities = {.size=sizeof(PBCapabilities),.names_data=names,.names_capacity=32,.names_count=0,.names_stride=sizeof(PBRange),.bytes_data=text,.bytes_capacity=4096,.bytes_count=0};
    if (pb_capabilities(context, &capabilities) != PB_OK || capabilities.names_count == 0)
        return 1;
    PBArgument argument = {.size=sizeof(PBArgument)};
    PBArguments arguments = {.size=sizeof(PBArguments),.arguments_data=&argument,.arguments_capacity=1,.arguments_count=0,.arguments_stride=sizeof(PBArgument),.bytes_data=text,.bytes_capacity=4096,.bytes_count=0};
    if (pb_startup_arguments(context, &arguments) != PB_OK || argument.name_count != 4 ||
        argument.value_offset != 4 || argument.value_count != 4 || text[4] != 'd')
        return 2;
    PBParticipant participant = {.size=sizeof(PBParticipant)};
    uint32_t participant_count = 0;
    if (pb_presence(context, &participant, 1, sizeof(participant), &participant_count) != PB_OK || participant.build_id[0] != 5)
        return 3;
    PBVolume volume = {.size=sizeof(PBVolume)};
    PBRange tag = {.size=sizeof(PBRange)};
    PBVolumes volumes = {.size=sizeof(PBVolumes),.volumes_data=&volume,.volumes_capacity=1,.volumes_count=0,.volumes_stride=sizeof(PBVolume),.tags_data=&tag,.tags_capacity=1,.tags_count=0,.tags_stride=sizeof(PBRange),.bytes_data=text,.bytes_capacity=4096,.bytes_count=0};
    if (pb_volumes(context, &volumes) != PB_OK || volume.flags != (PB_VOLUME_ROOT | PB_VOLUME_PLAYBIT) || volume.name_count != 4 ||
        tag.count != 4 || text[tag.offset] != 't')
        return 4;
    if (pb_file_dispose(context, volume.file) != PB_OK) return 5;
    return 0;
}

uint64_t probe_result_begin(uint64_t generation, uint32_t volume_slot, uint32_t kind) {
    if (!generation || generation > UINT32_MAX) return 0;
    PBContext *context = pb_guest_context();
    PBVolumeHandle volume = pb_handle_pack((uint32_t)generation, volume_slot);
    PBEntryHandle entry = {.size=sizeof(PBEntryHandle),.volume=volume};
    for (unsigned i = 0; i < 15; i++) entry.entry_id[i] = 5;
    PBId object = {0}, field = {0};
    for (unsigned i = 0; i < 16; i++) { object[i] = 9; field[i] = 7; }
    PBFileHandle file = pb_handle_pack((uint32_t)generation, volume_slot);
    PBOperationHandle operation = 0;
    uint8_t bytes[] = {'n', 'e', 'w'};
    PBStatus status;
    switch (kind) {
        case 0: status = probe_handle(pb_object_read_begin(context, &entry, &object, &field, 1), &operation); break;
        case 1: status = probe_handle(pb_file_read_begin(context, file, 0, 64), &operation); break;
        case 2: status = probe_handle(pb_file_write_begin(context, file, 0, bytes, 3), &operation); break;
        default: return 0;
    }
    return status == PB_OK ? operation : 0;
}
int32_t probe_result_take(uint64_t generation, uint32_t slot, uint32_t kind, uint32_t edited) {
    (void)edited;
    if (!generation || generation > UINT32_MAX) return PB_INVALID_ARGUMENT;
    PBContext *context = pb_guest_context();
    PBOperationHandle operation = pb_handle_pack((uint32_t)generation, slot);
    uint8_t text[64] = {0};
    if (kind == 0) {
        PBObject output = {.size=sizeof(PBObject),.fields_stride=sizeof(PBField),.values_stride=sizeof(PBValue)};
        PBStatus status = pb_object_read_take(context, operation, &output);
        if (status != PB_BUFFER_TOO_SMALL) return status;
        if (output.fields_count != 1 || output.values_count != 1 || output.bytes_count != 3) return PB_INTERNAL;
        PBField field = {.size=sizeof(PBField)};
        PBValue value = {.size=sizeof(PBValue)};
        output = (PBObject){.size=sizeof(PBObject),.fields_data=&field,.fields_capacity=1,.fields_stride=sizeof(field),.values_data=&value,.values_capacity=1,.values_stride=sizeof(value),.bytes_data=text,.bytes_capacity=64};
        status = pb_object_read_take(context, operation, &output);
        if (status != PB_OK) return status;
        return output.type_id[0] == 8 && field.present == 1 && value.kind == PB_TEXT &&
            value.bytes_count == 3 && text[value.bytes_offset] == 'a' ? PB_OK : PB_INTERNAL;
    }
    if (kind == 1) {
        uint32_t count = 0;
        PBStatus status = pb_file_read_take(context, operation, 0, 0, &count);
        if (status != PB_BUFFER_TOO_SMALL) return status;
        if (count != 5) return PB_INTERNAL;
        status = pb_file_read_take(context, operation, text, 64, &count);
        if (status != PB_OK) return status;
        return count == 5 && text[0] == 'h' ? PB_OK : PB_INTERNAL;
    }
    if (kind == 2) return pb_file_write_take(context, operation);
    return PB_INVALID_ARGUMENT;
}
