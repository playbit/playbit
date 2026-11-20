#define _DARWIN_C_SOURCE
#include <playbit/native.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <time.h>
#include <pthread.h>
#include "scenes.h"

typedef struct Host {
    PBEngine *engine;
    unsigned wakes, observed, worker_wakes, records;
    pthread_t owner;
    pthread_mutex_t mutex;
    pthread_cond_t ready;
} Host;
typedef enum Phase { RESOLVE, ESTABLISH, QUERY, CREATE, READ, EDIT, IDLE } Phase;
typedef struct App {
    Host *host;
    PBEntryHandle database;
    PBId object;
    PBOperationHandle operation;
    Phase phase;
    uint64_t generation;
    unsigned initialized, turns, destroyed, views, changes, checked, fail, reentries, dirty, opened;
} App;
static const PBId type = {1}, field = {2};
static void reject_reentry(App *a) {
    PBId id;
    assert(pb_engine_account(a->host->engine,&id)==PB_CONFLICT);
    assert(pb_engine_app_detach(a->host->engine,a->generation)==PB_CONFLICT);
    assert(pb_engine_close(&a->host->engine)==PB_CONFLICT && a->host->engine);
    ++a->reentries;
}
static void wake(void *data) {
    Host *h=data;
    assert(pthread_mutex_lock(&h->mutex)==0);
    ++h->wakes;
    if(!pthread_equal(h->owner,pthread_self())) ++h->worker_wakes;
    assert(pthread_cond_signal(&h->ready)==0);
    assert(pthread_mutex_unlock(&h->mutex)==0);
}
static void wait_wake(Host *h) {
    struct timespec until;
    assert(clock_gettime(CLOCK_REALTIME,&until)==0); until.tv_sec+=5;
    assert(pthread_mutex_lock(&h->mutex)==0);
    while(h->wakes==h->observed) assert(pthread_cond_timedwait(&h->ready,&h->mutex,&until)==0);
    h->observed=h->wakes;
    assert(pthread_mutex_unlock(&h->mutex)==0);
}
static void change(void *data, const PBEngineChange *event, const uint8_t *bytes, uint32_t length) {
    Host *host=data;
    assert(event->sequence>0 && (length==0 || bytes));
    ++host->records;
}
static void drain(Host *h) {
    PBEngineChanges status = {.size=sizeof(PBEngineChanges)};
    unsigned turns=0;
    do {
        assert(pb_engine_drain(h->engine,change,h,&status)==PB_OK);
        assert(status.records<=8 && !status.closed && !status.gap_generation);
        assert(++turns<=64);
    } while(status.remaining);
}
static void read_begin(PBContext *ctx,App *a) {
    assert(probe_handle(pb_object_read_begin(ctx,&a->database,&a->object,&field,1), &a->operation)==PB_OK);
    a->phase=READ; a->dirty=0;
}
static PBStatus init(PBContext *ctx, void *data, void **state) {
    App *a=data; *state=a; ++a->initialized;
    reject_reentry(a);
    PBIdentity identity = {.size=sizeof(PBIdentity)};
    assert(pb_identity(ctx,&identity)==PB_OK && identity.flags==0);
    assert(identity.generation==a->generation);
    PBVolume volume = {.size=sizeof(PBVolume)}; uint8_t bytes[256];
    PBVolumes volumes = {.size=sizeof(PBVolumes),.volumes_data=&volume,.volumes_capacity=1,.volumes_count=0,.volumes_stride=sizeof(PBVolume),.tags_stride=sizeof(PBRange),.bytes_data=bytes,.bytes_capacity=sizeof(bytes),.bytes_count=0};
    assert(pb_volumes(ctx,&volumes)==PB_OK && volumes.volumes_count==1 && volume.flags==(PB_VOLUME_ROOT | PB_VOLUME_PLAYBIT));
    a->database.size = sizeof(a->database);
    assert(pb_log(ctx,(const uint8_t *)"initialized",11)==PB_OK);
    if(a->fail) return PB_UNAVAILABLE;
    assert(pb_file_dispose(ctx, volume.file) == PB_OK);
    assert(probe_handle(pb_entry_resolve_begin(ctx, volume.handle, (const uint8_t *)"records", 7), &a->operation) == PB_OK);
    a->phase=RESOLVE;
    return PB_OK;
}
static PBStatus turn(PBContext *ctx, void *state) {
    App *a=state; ++a->turns; reject_reentry(a);
    for (unsigned count=0;count<256;++count) {
        PBEvent event = {.size=sizeof(PBEvent)}; PBOperationHandle operation;
        int32_t result=pb_event_poll(ctx,&event,(uint8_t *)&operation,sizeof(operation));
        if (result==PB_WOULD_BLOCK) break;
        assert(result>=0 && result<=(int32_t)sizeof(operation) && event.generation==a->generation);
        if(event.kind==PB_EVENT_DATA_READY || event.kind==PB_EVENT_OBJECT_CREATION_READY) {
            assert(result==sizeof(operation) && event.target==0);
            assert(memcmp(&operation,&a->operation,sizeof(operation))==0);
            switch(a->phase) {
                case RESOLVE: {
                    assert(pb_entry_resolve_take(ctx, a->operation, &a->database) == PB_OK);
                    PBSchemaFieldInput f = {.size=sizeof(PBSchemaFieldInput),.name_data=(const uint8_t *)"done",.name_length=4,.kind=PB_BOOLEAN};
                    memcpy(f.id, field, sizeof(PBId));
                    PBSchemaTypeInput t = {.size=sizeof(PBSchemaTypeInput),.name_data=(const uint8_t *)"Item",.name_length=4,.fields=&f,.count=1,.stride=sizeof(PBSchemaFieldInput)};
                    memcpy(t.id, type, sizeof(PBId));
                    assert(probe_handle(pb_schema_establish_begin(ctx, &a->database, &t, 1, sizeof(PBSchemaTypeInput)), &a->operation) == PB_OK);
                    a->phase=ESTABLISH;
                    break;
                }
                case ESTABLISH: {
                    assert(pb_schema_establish_take(ctx,a->operation)==PB_OK);
                    PBQuery query = {.size=sizeof(PBQuery),.limit=2};memcpy(query.type_id, type, 16);
                    assert(probe_handle(pb_object_query_begin(ctx,&a->database,&query), &a->operation)==PB_OK);
                    a->phase=QUERY; break;
                }
                case QUERY: {
                    PBObjectInfo objects[2] = {0};
    for (uint32_t i = 0; i < 2; i++) objects[i].size = sizeof(PBObjectInfo); PBPage page = {.size=sizeof(PBPage),.objects_data=objects,.objects_capacity=2,.objects_count=0,.objects_stride=sizeof(PBObjectInfo),.fields_stride=sizeof(PBField),.values_stride=sizeof(PBValue)};
                    assert(pb_object_query_take(ctx,a->operation,&page)==PB_OK && page.objects_count<=1);
                    if(page.objects_count) { memcpy(a->object, objects[0].id, sizeof(PBId)); read_begin(ctx,a); }
                    else {
                        PBValueInput value = {.size=sizeof(PBValueInput),.kind=PB_BOOLEAN}; PBFieldInput initial = {.size=sizeof(PBFieldInput),.values=&value,.count=1,.present=1,.stride=sizeof(PBValueInput)};memcpy(initial.id, field, 16);
                        assert(probe_handle(pb_object_create_begin(ctx, &a->database, &type, &initial, 1, sizeof(PBFieldInput)), &a->operation)==PB_OK);
                        a->phase=CREATE;
                    }
                    break;
                }
                case CREATE:
                    assert(pb_object_create_poll(ctx,a->operation,&a->object)==PB_OK);
                    read_begin(ctx,a); break;
                case READ: {
                    PBField f = {.size=sizeof(PBField)}; PBValue v = {.size=sizeof(PBValue)};
                    PBObject record = {.size=sizeof(PBObject),.fields_data=&f,.fields_capacity=1,.fields_count=0,.fields_stride=sizeof(PBField),.values_data=&v,.values_capacity=1,.values_count=0,.values_stride=sizeof(PBValue)};
                    PBStatus status=pb_object_read_take(ctx,a->operation,&record);
                    if(status==PB_STALE_HANDLE) { read_begin(ctx,a); break; }
                    assert(status==PB_OK && record.fields_count==1 && record.values_count==1);
                    assert(v.kind==PB_BOOLEAN && v.scalar<=1);
                    a->checked=(unsigned)v.scalar; a->phase=IDLE; a->opened=1;
                    if(a->dirty) read_begin(ctx,a);
                    break;
                }
                case EDIT:
                    assert(pb_object_edit_take(ctx,a->operation)==PB_OK);
                    read_begin(ctx,a); break;
                case IDLE: assert(0); break;
            }
        } else if(event.kind==PB_EVENT_CLICK && a->phase==IDLE) {
            PBValueInput value = {.size=sizeof(PBValueInput),.kind=PB_BOOLEAN,.scalar=!a->checked};
            PBFieldInput edit = {.size=sizeof(PBFieldInput),.values=&value,.count=1,.present=1,.stride=sizeof(PBValueInput)};memcpy(edit.id, field, 16);
            assert(probe_handle(pb_object_edit_begin(ctx, &a->database, &a->object, &edit, 1, sizeof(PBFieldInput)), &a->operation)==PB_OK);
            a->phase=EDIT;
        } else if(event.kind==PB_EVENT_DATA_CHANGED) {
            ++a->changes; a->dirty=1;
            if(a->phase==IDLE) read_begin(ctx,a);
        } else assert(event.kind==PB_EVENT_STARTED || event.kind==PB_EVENT_CLICK);
    }
    const uint8_t *scene; uint32_t length;
    if(!a->opened) { scene=opening; length=sizeof(opening); }
    else if(a->phase==IDLE) { scene=a->checked?on:off; length=a->checked?sizeof(on):sizeof(off); }
    else { scene=a->checked?pending_on:pending_off; length=a->checked?sizeof(pending_on):sizeof(pending_off); }
    return pb_view_submit(ctx,scene,length);
}
static void destroy(void *state) { App *a=state; ++a->destroyed; reject_reentry(a); }
static void view(void *data,uint64_t generation,const uint8_t *bytes,uint32_t length) {
    App *a=data; assert(generation==a->generation); reject_reentry(a);
    const uint8_t *expected; uint32_t size;
    if(!a->opened) { expected=opening; size=sizeof(opening); }
    else if(a->phase==IDLE) { expected=a->checked?on:off; size=a->checked?sizeof(on):sizeof(off); }
    else { expected=a->checked?pending_on:pending_off; size=a->checked?sizeof(pending_on):sizeof(pending_off); }
    assert(length==size && memcmp(bytes,expected,length)==0);
    ++a->views;
}
static void log_text(void *data,uint64_t generation,const uint8_t *bytes,uint32_t length) {
    App *a=data; assert(generation==a->generation); reject_reentry(a);
    assert(length==11 && memcmp(bytes,"initialized",11)==0);
}
static PBEngineAppOutput output(App *a) { return (PBEngineAppOutput){.size=sizeof(PBEngineAppOutput),.view=view,.log=log_text,.user_data=a}; }
static uint32_t flags(App *a) {
    PBEngineAppStep step = {.size=sizeof(PBEngineAppStep)};
    assert(pb_engine_app_step(a->host->engine,a->generation,&step)==PB_OK);
    assert(step.reserved==0 && step.wake_after_ns==0);
    return step.flags;
}
static void advance(App *a) {
    PBEngineAppStep step = {.size=sizeof(PBEngineAppStep)}; PBEngineAppOutput out=output(a);
    assert(pb_engine_app_advance(a->host->engine,a->generation,&out,&step)==PB_OK);
    assert(step.reserved==0 && step.wake_after_ns==0);
}
// Wait outside callbacks; each successful readiness check executes one turn.
static void settle(Host *host,App *apps,unsigned count) {
    for(unsigned round=0;round<512;++round) {
        drain(host);
        unsigned advanced=0;
        for(unsigned i=0;i<count;++i) if(flags(&apps[i])&PB_ENGINE_APP_READY) { advance(&apps[i]); advanced=1; }
        drain(host);
        unsigned pending=0;
        for(unsigned i=0;i<count;++i) if(apps[i].phase!=IDLE || flags(&apps[i])) pending=1;
        if(!pending) return;
        if(!advanced) wait_wake(host);
    }
    assert(0 && "finite C owner continuations exceeded fixture bound");
}
static void attach(App *a,const PBId *root) {
    PBApp app = {.size=sizeof(PBApp),.init=init,.turn=turn,.destroy=destroy,.user_data=a};
    assert(pb_engine_app_attach(a->host->engine,root,&app,&a->generation)==PB_OK);
    assert(a->generation && !a->initialized && flags(a)==PB_ENGINE_APP_READY);
}
static uint64_t nanos(void) {
    struct timespec t; assert(clock_gettime(CLOCK_MONOTONIC,&t)==0);
    return (uint64_t)t.tv_sec*1000000000+(uint64_t)t.tv_nsec;
}
static int compare(const void *a,const void *b) {
    uint64_t x=*(const uint64_t *)a,y=*(const uint64_t *)b; return (x>y)-(x<y);
}
int main(int argc,char **argv) {
    if(argc==2 && strcmp(argv[1],"--layout")==0) {
        printf("%zu,%zu,%zu,%zu,%zu,%zu\n",sizeof(PBEngineAppOutput),_Alignof(PBEngineAppOutput),offsetof(PBEngineAppOutput,user_data),sizeof(PBEngineAppStep),_Alignof(PBEngineAppStep),offsetof(PBEngineAppStep,flags));
        return 0;
    }
    assert(argc==3);
    Host hosts[2]={{0}}; PBId roots[2];
    for(unsigned i=0;i<2;++i) {
        hosts[i].owner=pthread_self();
        assert(pthread_mutex_init(&hosts[i].mutex,NULL)==0);
        assert(pthread_cond_init(&hosts[i].ready,NULL)==0);
    }
    uint64_t begin=nanos();
    for(unsigned i=0;i<2;++i) {
        assert(pb_engine_open((const uint8_t *)argv[i+1],(uint32_t)strlen(argv[i+1]),wake,&hosts[i],&hosts[i].engine)==PB_OK);
        char fixture_path[4096];
        assert(snprintf(fixture_path, sizeof(fixture_path), "%s/c-probe-root.bin", argv[i+1]) < (int)sizeof(fixture_path));
        FILE *fixture = fopen(fixture_path, "rb");
        assert(fixture);
        assert(fread(roots[i], 1, sizeof(PBId), fixture) == sizeof(PBId));
        assert(fgetc(fixture) == EOF && !ferror(fixture));
        assert(fclose(fixture) == 0);
        drain(&hosts[i]);
    }
    App apps[4]={{.host=&hosts[0]},{.host=&hosts[0]},{.host=&hosts[0]},{.host=&hosts[0]}};
    App other={.host=&hosts[1]}; attach(&other,&roots[1]); settle(&hosts[1],&other,1);
    for(unsigned i=0;i<4;++i) { attach(&apps[i],&roots[0]); settle(&hosts[0],apps,i+1); }
    uint64_t startup=nanos()-begin;
    App extra={.host=&hosts[0]}; PBApp extra_table = {.size=sizeof(PBApp),.init=init,.turn=turn,.destroy=destroy,.user_data=&extra}; uint64_t unchanged=123;
    assert(pb_engine_app_attach(hosts[0].engine,&roots[0],&extra_table,&unchanged)==PB_LIMIT_EXCEEDED && unchanged==123);
    PBEngineAppStep step = {.size=sizeof(PBEngineAppStep)};
    assert(pb_engine_app_step(hosts[1].engine,apps[0].generation,&step)==PB_STALE_HANDLE);
    PBEvent event = {.size=sizeof(PBEvent),.generation=apps[0].generation,.kind=PB_EVENT_CLICK,.target=2};
    assert(pb_engine_app_input(hosts[0].engine,&event,NULL,0)==PB_INVALID_ARGUMENT);
    event.kind=PB_EVENT_DATA_CHANGED; event.target=0;
    assert(pb_engine_app_input(hosts[0].engine,&event,NULL,0)==PB_INVALID_ARGUMENT);
    event.kind=PB_EVENT_CLICK;event.target=1;
    uint64_t samples[1001];
    for(unsigned n=0;n<1001;++n) {
        begin=nanos();
        assert(pb_engine_app_input(hosts[0].engine,&event,NULL,0)==PB_OK);
        PBEngineAppOutput out=output(&apps[0]);
        unsigned turns=apps[0].turns;
        assert(pb_engine_app_advance(hosts[0].engine,apps[0].generation,&out,NULL)==PB_INVALID_ARGUMENT && apps[0].turns==turns);
        settle(&hosts[0],apps,4);
        samples[n]=nanos()-begin;
        for(unsigned i=0;i<4;++i) assert(apps[i].checked==((n+1)%2));
        assert(!flags(&other) && !other.checked);
    }
    qsort(samples,1001,sizeof(samples[0]),compare);
    printf("C native 4-app commit/feed/scene_ns n=1001 p50=%llu p95=%llu p99=%llu max=%llu startup_ns=%llu\n",(unsigned long long)samples[500],(unsigned long long)samples[950],(unsigned long long)samples[990],(unsigned long long)samples[1000],(unsigned long long)startup);
    uint64_t retired=apps[3].generation;
    assert(pb_engine_app_detach(hosts[0].engine,retired)==PB_OK && apps[3].destroyed==1);
    assert(pb_engine_app_detach(hosts[0].engine,retired)==PB_STALE_HANDLE);
    extra.fail=1; attach(&extra,&roots[0]);
    PBEngineAppOutput out=output(&extra);
    assert(pb_engine_app_advance(hosts[0].engine,extra.generation,&out,&step)==PB_UNAVAILABLE);
    drain(&hosts[0]); assert(!flags(&extra));
    assert(pb_engine_app_advance(hosts[0].engine,extra.generation,&out,&step)==PB_CONFLICT && extra.initialized==1);
    assert(pb_engine_close(&hosts[0].engine)==PB_OK && !hosts[0].engine);
    for(unsigned i=0;i<4;++i) assert(apps[i].destroyed==1);
    assert(extra.destroyed==1);
    assert(pb_engine_open((const uint8_t *)argv[1],(uint32_t)strlen(argv[1]),wake,&hosts[0],&hosts[0].engine)==PB_OK);
    App reopened={.host=&hosts[0]}; attach(&reopened,&roots[0]); settle(&hosts[0],&reopened,1);
    assert(reopened.checked==1 && memcmp(&reopened.object,&apps[0].object,sizeof(PBId))==0);
    assert(reopened.generation!=retired);
    assert(pb_engine_close(&hosts[0].engine)==PB_OK && reopened.destroyed==1);
    assert(pb_engine_close(&hosts[1].engine)==PB_OK && other.destroyed==1);
    for(unsigned i=0;i<2;++i) {
        assert(hosts[i].worker_wakes>0);
        assert(pthread_cond_destroy(&hosts[i].ready)==0);
        assert(pthread_mutex_destroy(&hosts[i].mutex)==0);
    }
    puts("C native ownership, scoped edits, feed scheduling, callback reentry, failed init and durable reopen passed");
    return 0;
}
