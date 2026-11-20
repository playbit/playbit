#include <playbit/sys/abi.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}

uint64_t probe_schema_establish_begin(uint64_t generation, uint32_t slot) {
    if (!generation || generation > UINT32_MAX) return 0;
    PBContext *context = pb_guest_context();
    PBVolumeHandle volume = pb_handle_pack((uint32_t)generation, slot);
    PBEntryHandle entry = {.size=sizeof(PBEntryHandle),.volume=volume};
    for (unsigned i = 0; i < 15; i++) entry.entry_id[i] = 5;
    PBSchemaFieldInput field = {.size=sizeof(PBSchemaFieldInput),.name_data=(const uint8_t *)"link",.name_length=4,.kind=PB_REFERENCE,.flags=3};
    PBSchemaTypeInput type = {.size=sizeof(PBSchemaTypeInput),.name_data=(const uint8_t *)"Item",.name_length=4,.fields=&field,.count=1,.flags=1,.stride=sizeof(PBSchemaFieldInput)};
    for (unsigned i = 0; i < 16; i++) { type.id[i] = 8; field.id[i] = 7; field.reference_type[i] = 8; }
    PBOperationHandle operation = 0;
    if (probe_handle(pb_schema_establish_begin(context, &entry, &type, 1, sizeof(PBSchemaTypeInput)), &operation) != PB_OK || (uint32_t)(operation >> 32) == 0) return 0;
    return operation;
}
int32_t probe_schema_establish_take(uint64_t generation, uint32_t slot) {
    if (!generation || generation > UINT32_MAX) return PB_INVALID_ARGUMENT;
    PBOperationHandle operation = pb_handle_pack((uint32_t)generation, slot);
    return pb_schema_establish_take(pb_guest_context(), operation);
}
uint64_t probe_schema_read_begin(uint64_t generation, uint32_t slot) {
    if (!generation || generation > UINT32_MAX) return 0;
    PBOperationHandle operation = 0;
    PBVolumeHandle volume = pb_handle_pack((uint32_t)generation, slot);
    PBEntryHandle entry = {.size=sizeof(PBEntryHandle),.volume=volume};
    for (unsigned i = 0; i < 15; i++) entry.entry_id[i] = 5;
    if (probe_handle(pb_schema_read_begin(pb_guest_context(), &entry), &operation) != PB_OK || (uint32_t)(operation >> 32) == 0) return 0;
    return operation;
}
uint32_t probe_schema(uint64_t generation, uint32_t slot) {
    if (!generation || generation > UINT32_MAX) return 1;
    PBContext *context = pb_guest_context();
    PBOperationHandle operation = pb_handle_pack((uint32_t)generation, slot);
    PBSchema short_output = {.size=sizeof(PBSchema),.types_stride=sizeof(PBSchemaType),.fields_stride=sizeof(PBSchemaField)};
    if (pb_schema_read_take(context, operation, &short_output) != PB_BUFFER_TOO_SMALL ||
        short_output.types_count != 1 || short_output.fields_count != 1 || short_output.bytes_count != 8) return 2;
    PBSchemaType output_type = {.size=sizeof(PBSchemaType)};
    PBSchemaField output_field = {.size=sizeof(PBSchemaField)};
    uint8_t bytes[8] = {0};
    PBSchema output = {.size=sizeof(PBSchema),.types_data=&output_type,.types_capacity=1,.types_count=0,.types_stride=sizeof(PBSchemaType),.fields_data=&output_field,.fields_capacity=1,.fields_count=0,.fields_stride=sizeof(PBSchemaField),.bytes_data=bytes,.bytes_capacity=8,.bytes_count=0};
    if (pb_schema_read_take(context, operation, &output) != PB_OK) return 3;
    if (output.types_count != 1 || output.fields_count != 1 || output.bytes_count != 8 ||
        output_type.flags != 1 || output_type.reserved != 0 || output_type.fields_count != 1 ||
        output_field.kind != PB_REFERENCE || output_field.flags != 3 ||
        output_field.reference_type[0] != 8 || output_field.name_offset != 4 ||
        bytes[0] != 'I' || bytes[4] != 'l') return 4;
    return 0;
}
