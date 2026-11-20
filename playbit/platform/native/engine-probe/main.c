#define _DARWIN_C_SOURCE
#include <playbit/engine.h>
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <time.h>
#include <sys/stat.h>

typedef struct Probe {
    PBEngine *engine;
    uint32_t wakes, records, reentries, closed;
    uint64_t last_sequence;
} Probe;
static void wake(void *data) {
    Probe *p = data;
    assert(!p->closed);
    ++p->wakes;
    if (p->engine) {
        PBId account;
        assert(pb_engine_account(p->engine, &account) == PB_CONFLICT);
        assert(pb_engine_close(&p->engine) == PB_CONFLICT);
        assert(p->engine);
        ++p->reentries;
    }
}
static void change(void *data, const PBEngineChange *event,
                   const uint8_t *bytes, uint32_t length) {
    Probe *p = data;
    assert(event->sequence > p->last_sequence);
    assert(event->kind >= PB_ENGINE_DATABASE && event->kind <= PB_ENGINE_VOLUME_AVAILABLE);
    assert(length == 0 || bytes);
    p->last_sequence = event->sequence;
    ++p->records;
    PBEngineChanges result = {.size=sizeof(PBEngineChanges)};
    assert(pb_engine_drain(p->engine, change, data, &result) == PB_CONFLICT);
    assert(pb_engine_close(&p->engine) == PB_CONFLICT);
}
static void drain(Probe *p) {
    PBEngineChanges batch = {.size=sizeof(PBEngineChanges)};
    uint32_t turns = 0;
    do {
        uint32_t before = p->records, wakes = p->wakes;
        assert(pb_engine_drain(p->engine, change, p, &batch) == PB_OK);
        assert(batch.records <= 8 && batch.records == p->records - before);
        assert(batch.closed == 0 && batch.gap_generation == 0 && batch.reserved == 0);
        if (batch.remaining) assert(p->wakes > wakes);
        assert(++turns <= 64);
    } while (batch.remaining);
}
static void *wrong_thread(void *data) {
    Probe *p = data;
    PBId id;
    assert(pb_engine_account(p->engine, &id) == PB_CONFLICT);
    PBEngine *copy = p->engine;
    assert(pb_engine_close(&copy) == PB_CONFLICT && copy == p->engine);
    return NULL;
}
static void open_engine(Probe *p, const char *path) {
    p->closed = 0;
    assert(pb_engine_open((const uint8_t *)path, (uint32_t)strlen(path), wake, p, &p->engine) == PB_OK);
    assert(p->engine);
    char socket[4096];
    assert(snprintf(socket,sizeof(socket),"%s/control/engine.sock",path) < (int)sizeof(socket));
    struct stat st;
    assert(stat(socket,&st) != 0);
}
static void close_engine(Probe *p) {
    uint32_t wakes = p->wakes;
    assert(pb_engine_close(&p->engine) == PB_OK && !p->engine);
    p->closed = 1;
    assert(p->wakes == wakes);
    assert(pb_engine_close(&p->engine) == PB_INVALID_ARGUMENT);
}
static void layouts(void) {
    printf("%zu,%zu,%zu,%zu,%zu,%zu,%zu,%zu,%zu,%zu,%zu\n",
           sizeof(PBEngineVolume), _Alignof(PBEngineVolume), offsetof(PBEngineVolume,name),
           offsetof(PBEngineVolume,name_length), sizeof(PBEngineChange),
           _Alignof(PBEngineChange), offsetof(PBEngineChange,sequence),
           sizeof(PBEngineChanges), _Alignof(PBEngineChanges),
           offsetof(PBEngineChanges,gap_generation), offsetof(PBEngineChanges,remaining));
}
static uint64_t nanos(void) {
    struct timespec t;
    assert(clock_gettime(CLOCK_MONOTONIC,&t)==0);
    return (uint64_t)t.tv_sec*1000000000+(uint64_t)t.tv_nsec;
}
static int compare_time(const void *a, const void *b) {
    uint64_t x=*(const uint64_t *)a, y=*(const uint64_t *)b;
    return (x>y)-(x<y);
}
int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1],"--layout")==0) { layouts(); return 0; }
    assert(argc==3); // Two new isolated profile paths, retained for inspection.
    Probe first={0}, second={0};
    PBEngine *sentinel=(PBEngine *)(uintptr_t)8;
    assert(pb_engine_open(NULL,0,wake,&first,&sentinel)==PB_INVALID_ARGUMENT);
    assert(sentinel==(PBEngine *)(uintptr_t)8);
    assert(pb_engine_open((const uint8_t *)"relative",8,wake,&first,&sentinel)==PB_INVALID_ARGUMENT);
    assert(pb_engine_open((const uint8_t *)argv[1],(uint32_t)strlen(argv[1]),NULL,&first,&sentinel)==PB_INVALID_ARGUMENT);
    assert(sentinel==(PBEngine *)(uintptr_t)8);
    uint64_t begin=nanos();
    open_engine(&first,argv[1]);
    uint64_t open_ns=nanos()-begin;
    open_engine(&second,argv[2]);
    PBId a,b;
    assert(pb_engine_account(first.engine,&a)==PB_OK);
    assert(pb_engine_account(second.engine,&b)==PB_OK);
    assert(memcmp(&a,&b,sizeof(a))!=0);
    pthread_t thread;
    assert(pthread_create(&thread,NULL,wrong_thread,&first)==0);
    assert(pthread_join(thread,NULL)==0);
    PBEngineVolume volume = {.size=sizeof(PBEngineVolume)};
    memset(&volume,0xA5,sizeof(volume));
    volume.size = sizeof(volume); volume.written = 0;
    PBEngineVolume untouched=volume;
    assert(pb_engine_volume_create(first.engine,(const uint8_t *)"Valid",5,NULL)==PB_INVALID_ARGUMENT);
    assert(pb_engine_volume_create(first.engine,(const uint8_t *)" ",1,&volume)==PB_INVALID_ARGUMENT);
    const uint8_t invalid[]={0xff};
    assert(pb_engine_volume_create(first.engine,invalid,1,&volume)==PB_INVALID_ARGUMENT);
    assert(memcmp(&volume,&untouched,sizeof(volume))==0);
    memcpy(volume.name,"alias",5); untouched=volume;
    assert(pb_engine_volume_create(first.engine,volume.name,5,&volume)==PB_INVALID_ARGUMENT);
    assert(memcmp(&volume,&untouched,sizeof(volume))==0);
    begin=nanos();
    assert(pb_engine_volume_create(first.engine,(const uint8_t *)"C engine",8,&volume)==PB_OK);
    uint64_t create_ns=nanos()-begin;
    assert(volume.name_length==8 && memcmp(volume.name,"C engine",8)==0);
    assert(volume.share_key_with_service==1 && memcmp(&volume.owner,&a,sizeof(a))==0);
    assert(first.wakes>0 && first.reentries>0);
    assert(pb_engine_volume_read(second.engine,&volume.id,&untouched)==PB_NOT_FOUND);
    uint32_t records=first.records;
    assert(pb_engine_drain(first.engine,change,&first,NULL)==PB_INVALID_ARGUMENT);
    assert(first.records==records);
    drain(&first);
    assert(first.records>0 && second.records==0);
    PBEngineVolume extra = {.size=sizeof(PBEngineVolume)};
    for (unsigned i=0;i<32;++i)
        assert(pb_engine_volume_create(first.engine,(const uint8_t *)"queued",6,&extra)==PB_OK);
    drain(&first);
    assert(pb_engine_volume_create(first.engine,(const uint8_t *)"pending close",13,&extra)==PB_OK);
    close_engine(&first); // Queued payloads and final feed wake are retired.
    first.last_sequence=0;
    begin=nanos(); open_engine(&first,argv[1]); uint64_t reopen_ns=nanos()-begin;
    PBId restored;
    assert(pb_engine_account(first.engine,&restored)==PB_OK && memcmp(&a,&restored,sizeof(a))==0);
    PBEngineVolume read = {.size=sizeof(PBEngineVolume)};
    assert(pb_engine_volume_read(first.engine,&volume.id,&read)==PB_OK);
    assert(memcmp(&read,&volume,sizeof(read))==0);
    assert(pb_engine_volume_read(first.engine,&extra.id,&read)==PB_OK);
    uint64_t reads[1000];
    for (unsigned i=0;i<1000;++i) {
        begin=nanos();
        assert(pb_engine_volume_read(first.engine,&volume.id,&read)==PB_OK);
        reads[i]=nanos()-begin;
    }
    qsort(reads,1000,sizeof(reads[0]),compare_time);
    printf("C engine read_ns n=1000 p50=%llu p95=%llu p99=%llu max=%llu\n",
           (unsigned long long)reads[499],(unsigned long long)reads[949],
           (unsigned long long)reads[989],(unsigned long long)reads[999]);
    close_engine(&first);
    assert(pb_engine_account(second.engine,&restored)==PB_OK && memcmp(&b,&restored,sizeof(b))==0);
    close_engine(&second);
    printf("C engine lifecycle passed; open_ns=%llu create_ns=%llu reopen_ns=%llu records=%u reentries=%u\n",
           (unsigned long long)open_ns,(unsigned long long)create_ns,(unsigned long long)reopen_ns,
           first.records,first.reentries);
    return 0;
}
