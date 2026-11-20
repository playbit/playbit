#include <playbit/sys/abi.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}

int32_t probe_query_take(uint64_t generation, uint32_t slot) {
    PBContext *context = pb_guest_context();
    if (!generation || generation > UINT32_MAX) return PB_INVALID_ARGUMENT;
    PBOperationHandle operation = pb_handle_pack((uint32_t)generation, slot);
    PBPage short_page = {.size=sizeof(PBPage), .objects_stride=sizeof(PBObjectInfo),
        .fields_stride=sizeof(PBField), .values_stride=sizeof(PBValue)};
    PBStatus status = pb_object_query_take(context, operation, &short_page);
    if (status != PB_BUFFER_TOO_SMALL) return status;
    if (short_page.objects_count != 2 || short_page.fields_count != 2 || short_page.values_count != 2 || short_page.bytes_count != 3) return PB_INTERNAL;
    PBObjectInfo objects[2] = {0};
    for (uint32_t i = 0; i < 2; i++) objects[i].size = sizeof(PBObjectInfo); PBField fields[2] = {0};
    for (uint32_t i = 0; i < 2; i++) fields[i].size = sizeof(PBField); PBValue values[2] = {0};
    for (uint32_t i = 0; i < 2; i++) values[i].size = sizeof(PBValue); uint8_t text[3] = {0};
    PBPage page = {.size=sizeof(PBPage),.objects_data=objects,.objects_capacity=2,.objects_count=0,.objects_stride=sizeof(PBObjectInfo),.fields_data=fields,.fields_capacity=2,.fields_count=0,.fields_stride=sizeof(PBField),.values_data=values,.values_capacity=2,.values_count=0,.values_stride=sizeof(PBValue),.bytes_data=text,.bytes_capacity=3,.bytes_count=0};
    status = pb_object_query_take(context, operation, &page);
    if (status != PB_OK) return status;
    if (page.objects_count!=2 || page.fields_count!=2 || page.values_count!=2 || page.bytes_count!=3 ||
        page.has_next!=1 || page.next[0]!=10 || page.projected!=1 || objects[0].id[0]!=9 ||
        objects[1].fields_offset!=1 || fields[1].values_offset!=1 || values[0].kind!=PB_TEXT ||
        values[1].kind!=PB_BOOLEAN || values[1].scalar!=1 || text[0]!='a' || text[2]!='c') return PB_INTERNAL;
    return 0;
}

uint64_t probe_query_begin(uint64_t generation, uint32_t slot) {
    if (!generation || generation > UINT32_MAX) return 0;
    PBVolumeHandle volume = pb_handle_pack((uint32_t)generation, slot);
    PBEntryHandle entry = {.size=sizeof(PBEntryHandle),.volume=volume};
    for (unsigned i = 0; i < 15; i++) entry.entry_id[i] = 5;
    PBId projection = {0};
    PBQuery query = {.size=sizeof(PBQuery),.flags=2,.limit=2,.fields=&projection,.count=1};
    for (unsigned i = 0; i < 16; i++) { query.type_id[i] = 8; projection[i] = 7; }
    PBOperationHandle operation = 0;
    if (probe_handle(pb_object_query_begin(pb_guest_context(), &entry, &query), &operation) != PB_OK || (uint32_t)(operation >> 32) == 0) return 0;
    return operation;
}
