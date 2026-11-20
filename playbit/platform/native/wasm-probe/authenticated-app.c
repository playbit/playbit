#include <playbit/sys/abi.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}

uint32_t probe_authenticated(uint64_t generation,uint32_t slot) {
    PBContext *context=pb_guest_context();PBOperationHandle operation = pb_handle_pack((uint32_t)generation,slot);
    PBValue value = {.size=sizeof(PBValue)};uint8_t bytes[3]={0};
    PBFieldMetadata metadata = PB_RECORD_INIT(PBFieldMetadata);
    PBAuthenticatedField result = {.size=sizeof(PBAuthenticatedField),.metadata=&metadata,.values_data=&value,.values_capacity=1,.values_count=0,.values_stride=sizeof(PBValue),.bytes_data=bytes,.bytes_capacity=3,.bytes_count=0};
    if(pb_object_field_read_authenticated_take(context,operation,&result)!=PB_OK)return 1;
    PBFieldMetadata *m=result.metadata;
    if(m->volume_id[0]!=6 || m->object_id[0]!=9 || m->type_id[0]!=8 || m->field_id[0]!=7 ||
        m->revision[0]!=13 || m->principal_id[0]!=11 || m->device_id[0]!=12 || m->state!=2 ||
        m->provenance!=1 || m->principal_kind!=1 || m->reserved!=0 || result.values_count!=1 || result.bytes_count!=3 ||
        value.kind!=PB_TEXT || value.bytes_offset!=0 || value.bytes_count!=3 || bytes[0]!='a' || bytes[2]!='c')return 2;
    return 0;
}
uint64_t probe_authenticated_begin(uint64_t generation,uint32_t slot) {
    PBContext *context=pb_guest_context();PBVolumeHandle volume = pb_handle_pack((uint32_t)generation,slot);
    PBEntryHandle entry = {.size=sizeof(PBEntryHandle),.volume=volume};
    for (unsigned i = 0; i < 15; i++) entry.entry_id[i] = 5;PBId object={0},field={0};
    for(unsigned i=0;i<16;i++){object[i]=9;field[i]=7;}
    PBOperationHandle operation;
    if(probe_handle(pb_object_field_read_authenticated_begin(context,&entry,&object,&field), &operation)!=PB_OK)return 0;
    return operation;
}
