// Pure C app and launcher, linked to the installed Mac SDK with no Rust host wrapper.
#include <playbit/mac.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "scenes.h"
static const PBId type = {1}, field = {2};
static const uint8_t declaration[] =
    "{\"permissions\":{\"grants\":[]},\"databases\":[{"
    "\"id\":\"2d7dWtQMvj9WttA3mMnX\",\"name\":\"data\","
    "\"kind\":{\"type\":\"object_db\",\"schema\":{\"types\":{\"8AQGAut7N92awznwCnjuR\":{"
    "\"name\":\"Item\",\"deprecated\":false,\"fields\":{\"FKoXLpmDjH4AtzasQaUoq\":{"
    "\"name\":\"done\",\"value_type\":{\"kind\":\"boolean\"},\"repeated\":false,\"deprecated\":false}}}}},"
    "\"objects\":[{\"id\":\"NVCnWjeL6R5kqzNocNDiF\","
    "\"type_id\":\"8AQGAut7N92awznwCnjuR\","
    "\"fields\":{\"FKoXLpmDjH4AtzasQaUoq\":[{\"kind\":\"boolean\",\"value\":false}]}}]}}],\"blobs\":[]}";

typedef enum Phase { RESOLVE, QUERY, READ, EDIT, IDLE } Phase;
typedef struct State {
    PBEntryHandle database;
    PBId object;
    PBOperationHandle operation;
    Phase phase;
    unsigned initialized, destroyed, checked, dirty, opened;
} State;

static void read_begin(PBContext *ctx, State *s) {
    assert(probe_handle(pb_object_read_begin(ctx,&s->database,&s->object,&field,1), &s->operation)==PB_OK);
    s->phase=READ; s->dirty=0;
}

static PBStatus init(PBContext *ctx, void *data, void **output) {
    State *s=data; *output=s; ++s->initialized;
    s->database.size = sizeof(s->database);
    PBVolume volume = {.size=sizeof(PBVolume)}; uint8_t bytes[256];
    PBVolumes volumes = {.size=sizeof(PBVolumes),.volumes_data=&volume,.volumes_capacity=1,.volumes_count=0,.volumes_stride=sizeof(PBVolume),.tags_stride=sizeof(PBRange),.bytes_data=bytes,.bytes_capacity=sizeof(bytes),.bytes_count=0};
    assert(pb_volumes(ctx,&volumes)==PB_OK && volumes.volumes_count==1);
    assert(probe_handle(pb_entry_resolve_begin(ctx,volume.handle,(const uint8_t *)"data",4), &s->operation)==PB_OK);
    s->phase=RESOLVE;
    return PB_OK;
}
static PBStatus turn(PBContext *ctx, void *state) {
    State *s=state;
    for (unsigned n=0;n<256;++n) {
        PBEvent event = {.size=sizeof(PBEvent)}; uint8_t data[64];
        int32_t result=pb_event_poll(ctx,&event,data,sizeof(data));
        if (result==PB_WOULD_BLOCK) break;
        assert(result>=0 && result<=(int32_t)sizeof(data));
        if (event.kind==PB_EVENT_DATA_READY) {
            PBOperationHandle operation;
            assert(result==sizeof(operation) && event.target==0);
            memcpy(&operation,data,sizeof(operation));
            assert(memcmp(&operation,&s->operation,sizeof(operation))==0);
            switch(s->phase) {
                case RESOLVE: {
                    assert(pb_entry_resolve_take(ctx,s->operation,&s->database)==PB_OK);
                    PBQuery query = {.size=sizeof(PBQuery),.limit=2};memcpy(query.type_id, type, 16);
                    assert(probe_handle(pb_object_query_begin(ctx,&s->database,&query), &s->operation)==PB_OK);
                    s->phase=QUERY; break;
                }
                case QUERY: {
                    PBObjectInfo objects[2] = {0};
                    for (uint32_t i = 0; i < 2; i++) objects[i].size = sizeof(PBObjectInfo);
                    PBPage page = {.size=sizeof(PBPage),.objects_data=objects,.objects_capacity=2,.objects_count=0,.objects_stride=sizeof(PBObjectInfo),.fields_stride=sizeof(PBField),.values_stride=sizeof(PBValue)};
                    assert(pb_object_query_take(ctx,s->operation,&page)==PB_OK && page.objects_count==1);
                    memcpy(s->object, objects[0].id, sizeof(PBId)); read_begin(ctx,s); break;
                }
                case READ: {
                    PBField f = {.size=sizeof(PBField)}; PBValue v = {.size=sizeof(PBValue)};
                    PBObject record = {.size=sizeof(PBObject),.fields_data=&f,.fields_capacity=1,.fields_count=0,.fields_stride=sizeof(PBField),.values_data=&v,.values_capacity=1,.values_count=0,.values_stride=sizeof(PBValue)};
                    PBStatus status=pb_object_read_take(ctx,s->operation,&record);
                    if(status==PB_STALE_HANDLE) { read_begin(ctx,s); break; }
                    assert(status==PB_OK && record.fields_count==1 && record.values_count==1);
                    assert(v.kind==PB_BOOLEAN && v.scalar<=1);
                    s->checked=(unsigned)v.scalar; s->phase=IDLE; s->opened=1;
                    if(s->dirty) read_begin(ctx,s);
                    break;
                }
                case EDIT:
                    assert(pb_object_edit_take(ctx,s->operation)==PB_OK);
                    read_begin(ctx,s); break;
                case IDLE: assert(0); break;
            }
        } else if (event.kind==PB_EVENT_CLICK && event.target==1 && s->phase==IDLE) {
            PBValueInput value = {.size=sizeof(PBValueInput),.kind=PB_BOOLEAN,.scalar=!s->checked};
            PBFieldInput edit = {.size=sizeof(PBFieldInput),.values=&value,.count=1,.present=1,.stride=sizeof(PBValueInput)};memcpy(edit.id, field, 16);
            assert(probe_handle(pb_object_edit_begin(ctx, &s->database, &s->object, &edit, 1, sizeof(PBFieldInput)), &s->operation)==PB_OK);
            s->phase=EDIT;
        } else if(event.kind==PB_EVENT_DATA_CHANGED) {
            s->dirty=1;
            if(s->phase==IDLE) read_begin(ctx,s);
        }
    }
    const uint8_t *scene;
    uint32_t length;
    if(!s->opened) { scene=opening; length=sizeof(opening); }
    else if(s->phase==IDLE) { scene=s->checked?on:off; length=s->checked?sizeof(on):sizeof(off); }
    else { scene=s->checked?pending_on:pending_off; length=s->checked?sizeof(pending_on):sizeof(pending_off); }
    return pb_view_submit(ctx,scene,length);
}
static void destroy(void *state) { State *s=state; ++s->destroyed; }
static State state;
static void finished(void) {
    assert(state.initialized==1 && state.destroyed==1);
    puts("C Mac SDK: initialized once, destroyed once before process exit");
}
int main(int argc, char **argv) {
    assert(argc==2);
    PBApp app = {.size=sizeof(PBApp),.init=init,.turn=turn,.destroy=destroy,.user_data=&state};
    // Bad launch arguments must not consume the single main event loop.
    assert(pb_mac_run((const uint8_t *)"relative",8,(const uint8_t *)"C SDK",5,
        declaration,sizeof(declaration)-1,&app)==PB_INVALID_ARGUMENT);
    assert(atexit(finished)==0);
    PBStatus result=pb_mac_run((const uint8_t *)argv[1],(uint32_t)strlen(argv[1]),
        (const uint8_t *)"C SDK",5,declaration,sizeof(declaration)-1,&app);
    fprintf(stderr,"unexpected return from AppKit host: %d\n",result);
    return 1;
}
