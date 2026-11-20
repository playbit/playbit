#include <playbit/sys/abi.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}

uint32_t probe_pending(uint64_t generation,uint32_t slot) {
    PBContext *context=pb_guest_context();PBVolumeHandle volume = pb_handle_pack(generation,slot);
    PBEntryHandle entry = {.size=sizeof(PBEntryHandle),.volume=volume};
    for (unsigned i = 0; i < 15; i++) entry.entry_id[i] = 5;
    PBId type={0},field={0};PBFileReference source={.size=sizeof(PBFileReference),.volume=volume},destination={.size=sizeof(PBFileReference),.volume=volume};
    for(unsigned i=0;i<16;i++){type[i]=8;field[i]=7;}
    for(unsigned i=0;i<15;i++){source.entry_id[i]=9;destination.entry_id[i]=10;}
    uint8_t copied[15]={0};
    PBOperationHandle creation = 0,copy={0},selection={0};PBId result={0};PBSelection selected = {.size=sizeof(PBSelection)};
    if(probe_handle(pb_object_create_begin(context, &entry, &type, 0, 0, sizeof(PBFieldInput)), &creation)!=PB_OK || (uint32_t)(creation >> 32) == 0 || (uint32_t)creation==0) return 1;
    if(pb_object_create_poll(context,creation,&result)!=PB_OK || result[0]!=9) return 2;
    if(probe_handle(pb_file_copy_begin(context,&source,&destination), &copy)!=PB_OK || copy==creation) return 3;
    if(pb_file_copy_poll(context,copy,&copied)!=PB_OK || copied[0]!=10) return 4;
    if(probe_handle(pb_file_select_begin(context), &selection)!=PB_OK) return 5;
    if(pb_file_select_poll(context,selection,&selected)!=PB_OK || selected.present!=1 || selected.reserved!=0 ||
        (uint32_t)(selected.volume >> 32)!=generation || (uint32_t)selected.volume!=slot ||
        selected.entry_id[0]!=5 || selected.entry_reserved!=0) return 6;
    return 0;
}
