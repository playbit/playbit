#include <playbit/sys/abi.h>

// Observe the scalar handle result while keeping each probe's status export compact.
static PBSysResult probe_handle(PBSysResult result, void* output) {
    if (result < 0) return result;
    if (result == 0) return PB_INTERNAL;
    *(uint64_t*)output = (uint64_t)result;
    return PB_OK;
}

#include <assert.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

typedef struct Probe {
    const uint8_t *off;
    const uint8_t *on;
    uint32_t off_len, on_len, view_bytes_limit;
    uint32_t initialized, turns, destroyed, submitted;
    int32_t fail_init, fail_turn;
    PBContext *context;
    uint64_t generation;
} Probe;
typedef struct State { Probe *probe; uint32_t checked; } State;

static PBStatus app_init(PBContext *context, void *data, void **output) {
    Probe *probe = data;
    State *state = calloc(1, sizeof(*state));
    if (!state) return PB_UNAVAILABLE;
    state->probe = probe;
    *output = state;
    probe->context = context;
    probe->initialized++;
    assert(pb_host_platform(context) == PB_PLATFORM_MACOS);
    assert(pb_host_platform(NULL) == PB_INVALID_ARGUMENT);
    assert(pb_host_limit(context, PB_LIMIT_QUERY_RECORDS) == 64);
    assert(pb_host_limit(context, PB_LIMIT_QUERY_VALUE_BYTES) == 1048576);
    assert(pb_host_limit(context, PB_LIMIT_VIEW_BYTES) == probe->view_bytes_limit);
    assert(pb_host_limit(context, 0xffffffff) == PB_UNSUPPORTED);
    assert(pb_host_limit(NULL, PB_LIMIT_QUERY_RECORDS) == PB_INVALID_ARGUMENT);
    PBIdentity identity = {.size=sizeof(PBIdentity)};
    assert(pb_identity(context, &identity) == PB_OK);
    assert(identity.generation != 0 && identity.flags == 2 && identity.reserved == 0);
    probe->generation = identity.generation;
    const uint8_t invalid_utf8[] = { 0xff };
    assert(pb_log(context, invalid_utf8, 1) == PB_INVALID_ARGUMENT);
    assert(pb_log(context, NULL, 1) == PB_INVALID_ARGUMENT);
    assert(pb_event_poll(context, NULL, NULL, 0) == PB_INVALID_ARGUMENT);
    assert(pb_identity(context, NULL) == PB_INVALID_ARGUMENT);
    assert(pb_identity(context, (PBIdentity *)((uint8_t *)&identity + 1)) == PB_INVALID_ARGUMENT);
    PBEvent untouched = {.size=sizeof(PBEvent),.generation=123,.kind=456,.target=789};
    assert(pb_event_poll(context, &untouched, (uint8_t *)&untouched, 1) == PB_INVALID_ARGUMENT);
    assert(untouched.generation == 123 && untouched.kind == 456 && untouched.target == 789);
    assert(pb_event_poll(context, (PBEvent *)((uint8_t *)&untouched + 1), NULL, 0) == PB_INVALID_ARGUMENT);
    assert(pb_wake_after(context, 0) == PB_INVALID_ARGUMENT);
    assert(pb_update_defer(context, 123) == PB_CONFLICT);
    assert(pb_log(context, (const uint8_t *)"C initialized", 13) == PB_OK);
    return probe->fail_init;
}
static PBStatus app_turn(PBContext *context, void *opaque) {
    State *state = opaque;
    Probe *probe = state->probe;
    probe->turns++;
    if (probe->fail_turn) return probe->fail_turn;
    for (uint32_t count = 0; count < 64; count++) {
        PBEvent event = {.size=sizeof(PBEvent),.generation=123,.kind=456,.target=789};
        int32_t length = pb_event_poll(context, &event, NULL, 0);
        if (length == PB_WOULD_BLOCK) break;
        assert(length >= 0);
        if (length > 0) {
            assert(event.generation == 123 && event.kind == 456 && event.target == 789);
            uint8_t bytes[64];
            assert(length <= (int32_t)sizeof(bytes));
            assert(pb_event_poll(context, &event, bytes, sizeof(bytes)) == length);
            assert(event.kind == PB_EVENT_SUBMIT && length == 4 && memcmp(bytes, "test", 4) == 0);
            probe->submitted++;
        }
        assert(event.generation == probe->generation);
        if (event.kind == PB_EVENT_CLICK) state->checked = !state->checked;
    }
    return pb_view_submit(context, state->checked ? probe->on : probe->off,
        state->checked ? probe->on_len : probe->off_len);
}
static void app_destroy(void *opaque) {
    if (!opaque) return;
    State *state = opaque;
    state->probe->destroyed++;
    free(state);
}
PBApp probe_app(Probe *probe) { return (PBApp){.size=sizeof(PBApp),.init=app_init,.turn=app_turn,.destroy=app_destroy,.user_data=probe}; }
int32_t probe_outside(Probe *probe) {
    assert(pb_host_platform(probe->context) == PB_CONFLICT);
    return pb_log(probe->context, (const uint8_t *)"outside", 7);
}
uint64_t probe_layout(uint32_t index) {
    const uint64_t layout[] = {
        sizeof(PBEvent), _Alignof(PBEvent), offsetof(PBEvent, kind), offsetof(PBEvent, target),
        sizeof(PBIdentity), _Alignof(PBIdentity), offsetof(PBIdentity, flags), offsetof(PBIdentity, reserved),
        sizeof(PBSessionInfo), _Alignof(PBSessionInfo), offsetof(PBSessionInfo, internal_id), offsetof(PBSessionInfo, key_secret),
        sizeof(PBApp), _Alignof(PBApp), offsetof(PBApp, user_data), sizeof(Probe), _Alignof(Probe)
    };
    assert(index < sizeof(layout) / sizeof(layout[0]));
    return layout[index];
}

typedef struct DataProbe {
    Probe base;
    PBId volume;
    uint8_t entry[15];
    PBId object, field, type;
    uint32_t checked;
} DataProbe;
typedef enum DataPhase {
    DATA_SCHEMA, DATA_ESTABLISH, DATA_IDS, DATA_CREATE, DATA_PROJECTED,
    DATA_READ, DATA_EDIT, DATA_IDLE
} DataPhase;
typedef struct DataState {
    DataProbe *probe;
    PBEntryHandle volume;
    PBOperationHandle operation;
    DataPhase phase;
    uint32_t checked_inputs;
} DataState;
static void data_read_begin(PBContext *context, DataState *state) {
    assert(probe_handle(pb_object_read_begin(context, &state->volume, &state->probe->object,
        &state->probe->field, 1), &state->operation) == PB_OK);
    state->phase = DATA_READ;
}
static void data_query_begin(PBContext *context, DataState *state, uint32_t projected) {
    PBQuery query = {.size=sizeof(PBQuery),.limit=2};memcpy(query.type_id, state->probe->type, 16);
    if (projected) {
        query.flags=2; query.fields=&state->probe->field; query.count=1;
    }
    assert(probe_handle(pb_object_query_begin(context, &state->volume, &query), &state->operation) == PB_OK);
    state->phase = projected ? DATA_PROJECTED : DATA_IDS;
}
static void data_schema_take(PBContext *context, DataState *state) {
    DataProbe *probe = state->probe;
    PBSchema sizes = {.size=sizeof(PBSchema),.types_stride=sizeof(PBSchemaType),.fields_stride=sizeof(PBSchemaField)};
    PBStatus status = pb_schema_read_take(context, state->operation, &sizes);
    if (status == PB_OK && sizes.types_count == 0) {
        PBSchemaFieldInput field = {.size=sizeof(PBSchemaFieldInput),.name_data=(const uint8_t *)"done",.name_length=4,.kind=PB_BOOLEAN};memcpy(field.id, probe->field, 16);
        PBSchemaTypeInput type = {.size=sizeof(PBSchemaTypeInput),.name_data=(const uint8_t *)"Item",.name_length=4,.fields=&field,.count=1,.stride=sizeof(PBSchemaFieldInput)};memcpy(type.id, probe->type, 16);
        field.kind = 99;
        assert(probe_handle(pb_schema_establish_begin(context, &state->volume, &type, 1, sizeof(PBSchemaTypeInput)), &state->operation)==PB_INVALID_ARGUMENT);
        field.kind = PB_BOOLEAN;
        assert(probe_handle(pb_schema_establish_begin(context, &state->volume, &type, 1, sizeof(PBSchemaTypeInput)), &state->operation)==PB_OK);
        state->phase = DATA_ESTABLISH;
        return;
    }
    assert(status == PB_BUFFER_TOO_SMALL);
    PBSchemaType type = {.size=sizeof(PBSchemaType)};
    PBSchemaField field = {.size=sizeof(PBSchemaField)};
    uint8_t names[16];
    PBSchema schema = {.size=sizeof(PBSchema),.types_data=&type,.types_capacity=1,.types_count=0,.types_stride=sizeof(PBSchemaType),.fields_data=&field,.fields_capacity=1,.fields_count=0,.fields_stride=sizeof(PBSchemaField),.bytes_data=names,.bytes_capacity=sizeof(names),.bytes_count=0};
    assert(pb_schema_read_take(context,state->operation,&schema)==PB_OK);
    assert(schema.types_count==1 && schema.fields_count==1 && schema.bytes_count==8);
    assert(type.fields_offset==0 && type.fields_count==1 && field.kind==PB_BOOLEAN);
    assert(memcmp(names+type.name_offset,"Item",4)==0 && memcmp(names+field.name_offset,"done",4)==0);
    data_query_begin(context, state, 0);
}
static void data_ids_take(PBContext *context, DataState *state) {
    DataProbe *probe = state->probe;
    PBObjectInfo objects[2] = {0};
    for (uint32_t i = 0; i < 2; i++) objects[i].size = sizeof(PBObjectInfo);
    PBPage page = {.size=sizeof(PBPage),.objects_data=objects,.objects_capacity=2,.objects_count=0,.objects_stride=sizeof(PBObjectInfo),.fields_stride=sizeof(PBField),.values_stride=sizeof(PBValue)};
    assert(pb_object_query_take(context,state->operation,&page)==PB_OK);
    assert(page.has_next==0 && page.projected==0 && page.objects_count<=1);
    if (page.objects_count==0) {
        PBValueInput value = {.size=sizeof(PBValueInput),.kind=PB_BOOLEAN};
        PBFieldInput change = {.size=sizeof(PBFieldInput),.values=&value,.count=1,.present=1,.stride=sizeof(PBValueInput)};memcpy(change.id, probe->field, 16);
        assert(probe_handle(pb_object_create_begin(context, &state->volume, &probe->type, &change, 1, sizeof(PBFieldInput)), &state->operation)==PB_OK);
        state->phase = DATA_CREATE;
    } else {
        memcpy(probe->object, objects[0].id, sizeof(PBId));
        data_query_begin(context, state, 1);
    }
}
static void data_projected_take(PBContext *context, DataState *state) {
    PBObjectInfo objects[2] = {0};
    for (uint32_t i = 0; i < 2; i++) objects[i].size = sizeof(PBObjectInfo);
    PBField field = {.size=sizeof(PBField)};
    PBValue value = {.size=sizeof(PBValue)};
    PBPage page = {.size=sizeof(PBPage),.objects_data=objects,.objects_capacity=2,.objects_count=0,.objects_stride=sizeof(PBObjectInfo),.fields_data=&field,.fields_capacity=1,.fields_count=0,.fields_stride=sizeof(PBField),.values_data=&value,.values_capacity=0,.values_count=0,.values_stride=sizeof(PBValue)};
    memset(&field,0xa5,sizeof(field));
    field.size = sizeof(field); field.written = 0;
    PBField untouched=field;
    page.next[0]=0x55;page.has_next=77;page.projected=77;
    assert(pb_object_query_take(context,state->operation,&page)==PB_BUFFER_TOO_SMALL);
    assert(page.objects_count==1 && page.fields_count==1 && page.values_count==1);
    assert(page.next[0]==0x55 && page.has_next==77 && page.projected==77);
    assert(memcmp(&field,&untouched,sizeof(field))==0);
    page.values_capacity=1;
    assert(pb_object_query_take(context,state->operation,&page)==PB_OK);
    assert(page.projected==1 && page.objects_count==1 && page.fields_count==1 && page.values_count==1);
    assert(memcmp(&objects[0].id,&state->probe->object,sizeof(PBId))==0 && value.kind==PB_BOOLEAN);
    data_read_begin(context, state);
}
static void data_read_take(PBContext *context, DataState *state) {
    DataProbe *probe = state->probe;
    PBObject short_output = {.size=sizeof(PBObject),.fields_stride=sizeof(PBField),.values_stride=sizeof(PBValue)};
    memset(&short_output.type_id, 0x55, sizeof(PBId));
    assert(pb_object_read_take(context, state->operation, &short_output) == PB_BUFFER_TOO_SMALL);
    assert(short_output.fields_count == 1 && short_output.values_count == 1 && short_output.bytes_count == 0);
    assert(short_output.type_id[0] == 0x55);
    PBField field = {.size=sizeof(PBField)};
    PBValue value = {.size=sizeof(PBValue)};
    PBObject object = {.size=sizeof(PBObject),.fields_data=&field,.fields_capacity=1,.fields_count=0,.fields_stride=sizeof(PBField),.values_data=&value,.values_capacity=1,.values_count=0,.values_stride=sizeof(PBValue)};
    assert(pb_object_read_take(context, state->operation, &object) == PB_OK);
    assert(object.fields_count == 1 && object.values_count == 1 && object.bytes_count == 0);
    assert(memcmp(&object.type_id, &probe->type, sizeof(PBId)) == 0);
    assert(memcmp(&field.id, &probe->field, sizeof(PBId)) == 0);
    assert(field.present == 1 && field.values_offset == 0 && field.values_count == 1);
    assert(value.kind == PB_BOOLEAN && value.scalar <= 1 && value.reserved == 0);
    probe->checked = (uint32_t)value.scalar;
    state->phase = DATA_IDLE;
    if (!state->checked_inputs) {
        PBValueInput invalid = {.size=sizeof(PBValueInput),.kind=PB_BOOLEAN,.scalar=2};
        PBFieldInput change = {.size=sizeof(PBFieldInput),.values=&invalid,.count=1,.present=1,.stride=sizeof(PBValueInput)};memcpy(change.id, probe->field, 16);
        assert(probe_handle(pb_object_edit_begin(context, &state->volume, &probe->object, &change, 1, sizeof(PBFieldInput)), &state->operation)==PB_INVALID_ARGUMENT);
        invalid.scalar = 0;
        PBEntryHandle stale = state->volume;
        stale.volume++;
        assert(probe_handle(pb_object_edit_begin(context, &stale, &probe->object, &change, 1, sizeof(PBFieldInput)), &state->operation)==PB_STALE_HANDLE);
        state->checked_inputs = 1;
    }
}
static PBStatus data_init(PBContext *context, void *opaque, void **output) {
    DataProbe *probe = opaque;
    DataState *state = calloc(1, sizeof(*state));
    if (!state) return PB_UNAVAILABLE;
    state->probe = probe;
    *output = state;
    probe->base.initialized++;
    PBVolumes sizes = {.size=sizeof(PBVolumes),.volumes_stride=sizeof(PBVolume),.tags_stride=sizeof(PBRange)};
    assert(pb_volumes(context, &sizes) == PB_BUFFER_TOO_SMALL);
    assert(sizes.volumes_count == 1 && sizes.tags_count == 1 && sizes.bytes_count <= 128);
    PBVolume volume = {.size=sizeof(PBVolume)};
    PBRange tag = {.size=sizeof(PBRange)};
    uint8_t bytes[128];
    PBVolumes volumes = {.size=sizeof(PBVolumes),.revision=0,.volumes_data=&volume,.volumes_capacity=1,.volumes_count=0,.volumes_stride=sizeof(PBVolume),.tags_data=&tag,.tags_capacity=1,.tags_count=0,.tags_stride=sizeof(PBRange),.bytes_data=bytes,.bytes_capacity=sizeof(bytes),.bytes_count=0};
    assert(pb_volumes(context, &volumes) == PB_OK);
    assert(volume.flags == (PB_VOLUME_ROOT | PB_VOLUME_PLAYBIT) && volume.rights == 7);
    assert(volume.name_count == 4 && memcmp(bytes + volume.name_offset, "Root", 4) == 0);
    assert(volume.tags_count == 1 && tag.count == 7 && memcmp(bytes + tag.offset, "fixture", 7) == 0);
    assert(memcmp(&volume.id, &probe->volume, sizeof(PBId)) == 0);
    assert(pb_file_dispose(context, volume.file) == PB_OK);
    state->volume.size = sizeof(state->volume);
    state->volume.volume = volume.handle;
    memcpy(state->volume.entry_id, probe->entry, 15);
    PBIdentity identity = {.size=sizeof(PBIdentity)};
    assert(pb_identity(context, &identity) == PB_OK);
    probe->base.generation = identity.generation;
    assert(probe_handle(pb_schema_read_begin(context, &state->volume), &state->operation) == PB_OK);
    state->phase = DATA_SCHEMA;
    return PB_OK;
}
static PBStatus data_turn(PBContext *context, void *opaque) {
    DataState *state = opaque;
    DataProbe *probe = state->probe;
    probe->base.turns++;
    for (uint32_t count = 0; count < 64; count++) {
        PBEvent event = {.size=sizeof(PBEvent)};
        PBOperationHandle operation;
        int32_t result = pb_event_poll(context, &event, (uint8_t *)&operation, sizeof(operation));
        if (result == PB_WOULD_BLOCK) break;
        assert(result >= 0 && result <= (int32_t)sizeof(operation));
        if (event.kind == PB_EVENT_DATA_READY || event.kind == PB_EVENT_OBJECT_CREATION_READY) {
            assert(result == sizeof(operation) && event.target == 0);
            assert(memcmp(&operation, &state->operation, sizeof(operation)) == 0);
            switch (state->phase) {
                case DATA_SCHEMA: data_schema_take(context, state); break;
                case DATA_ESTABLISH:
                    assert(pb_schema_establish_take(context, state->operation) == PB_OK);
                    assert(probe_handle(pb_schema_read_begin(context, &state->volume), &state->operation) == PB_OK);
                    state->phase = DATA_SCHEMA;
                    break;
                case DATA_IDS: data_ids_take(context, state); break;
                case DATA_CREATE:
                    assert(pb_object_create_poll(context, state->operation, &probe->object) == PB_OK);
                    data_query_begin(context, state, 1);
                    break;
                case DATA_PROJECTED: data_projected_take(context, state); break;
                case DATA_READ: data_read_take(context, state); break;
                case DATA_EDIT:
                    assert(pb_object_edit_take(context, state->operation) == PB_OK);
                    data_read_begin(context, state);
                    break;
                case DATA_IDLE: assert(0); break;
            }
        } else if (event.kind == PB_EVENT_CLICK) {
            assert(state->phase == DATA_IDLE);
            PBValueInput value = {.size=sizeof(PBValueInput),.kind=PB_BOOLEAN,.scalar=!probe->checked};
            PBFieldInput change = {.size=sizeof(PBFieldInput),.values=&value,.count=1,.present=1,.stride=sizeof(PBValueInput)};memcpy(change.id, probe->field, 16);
            PBStatus status = probe_handle(pb_object_edit_begin(context, &state->volume, &probe->object, &change, 1, sizeof(PBFieldInput)), &state->operation);
            if (status != PB_OK) return status;
            state->phase = DATA_EDIT;
        } else if (event.kind == PB_EVENT_DATA_CHANGED) {
            assert(state->phase == DATA_IDLE);
            data_read_begin(context, state);
        }
    }
    if (state->phase != DATA_IDLE) return PB_OK;
    return pb_view_submit(context, probe->checked ? probe->base.on : probe->base.off,
        probe->checked ? probe->base.on_len : probe->base.off_len);
}
static void data_destroy(void *opaque) {
    if (!opaque) return;
    DataState *state = opaque;
    state->probe->base.destroyed++;
    free(state);
}
PBApp data_probe_app(DataProbe *probe) { return (PBApp){.size=sizeof(PBApp),.init=data_init,.turn=data_turn,.destroy=data_destroy,.user_data=probe}; }
uint64_t data_probe_layout(uint32_t index) {
    const uint64_t layout[] = {
        sizeof(PBVolumeHandle), _Alignof(PBVolumeHandle), sizeof(PBRange),
        sizeof(PBVolume), offsetof(PBVolume, rights), sizeof(PBVolumes),
        sizeof(PBValueInput), offsetof(PBValueInput, data), sizeof(PBFieldInput),
        sizeof(PBValue), offsetof(PBValue, bytes_offset), sizeof(PBField), sizeof(PBObject),
        offsetof(PBObject, bytes_data), sizeof(DataProbe), offsetof(PBVolume, grant_id),
        offsetof(PBVolumes, revision), offsetof(PBVolumes, volumes_data)
    };
    assert(index < sizeof(layout)/sizeof(layout[0]));
    return layout[index];
}

uint64_t catalog_probe_layout(uint32_t index) {
    const uint64_t layout[] = {
        sizeof(const uint8_t *),sizeof(PBSchemaFieldInput),offsetof(PBSchemaFieldInput,reference_type),
        sizeof(PBSchemaTypeInput),offsetof(PBSchemaTypeInput,fields),sizeof(PBSchemaField),
        sizeof(PBSchemaType),sizeof(PBSchema),sizeof(PBQuery),offsetof(PBQuery,fields),
        sizeof(PBObjectInfo),sizeof(PBPage),offsetof(PBPage,objects_data)
    };
    assert(index<sizeof(layout)/sizeof(layout[0]));return layout[index];
}

// Read through the public C contract, including rejected outputs and retry semantics.
PBStatus probe_environment(PBContext *context, uint8_t seed, uint32_t participant_count,
                           uint32_t capability_count, uint32_t expected_deadline_count) {
    PBRange names[32] = {0};
    for (uint32_t i = 0; i < 32; i++) names[i].size = sizeof(PBRange);
    memset(names, 0x55, sizeof(names));
    for (uint32_t i = 0; i < 32; i++) { names[i].size = sizeof(PBRange); names[i].written = 0; }
    uint8_t bytes[1024];
    memset(bytes, 0x55, sizeof(bytes));
    PBCapabilities caps = {.size=sizeof(PBCapabilities),.names_data=names,.names_capacity=0,.names_count=99,.names_stride=sizeof(PBRange),.bytes_data=bytes,.bytes_capacity=sizeof(bytes),.bytes_count=99};
    assert(pb_capabilities(context, &caps) == PB_BUFFER_TOO_SMALL);
    assert(caps.names_count == capability_count && caps.bytes_count > 0);
    assert(names[0].offset == 0x55555555 && bytes[0] == 0x55);
    caps.names_capacity = 32;
    assert(pb_capabilities(context, &caps) == PB_OK);
    assert(caps.names_count == capability_count && names[0].count == 12);
    assert(memcmp(bytes + names[0].offset, "capabilities", 12) == 0);
    assert(names[11].count == 10 && memcmp(bytes + names[11].offset, "host.limit", 10) == 0);
    const char deadline_name[] = "event.poll.deadline";
    uint32_t deadline_count = 0;
    for (uint32_t i = 0; i < caps.names_count; i++) {
        if (names[i].count == sizeof(deadline_name) - 1 &&
            memcmp(bytes + names[i].offset, deadline_name, sizeof(deadline_name) - 1) == 0)
            deadline_count++;
    }
    assert(deadline_count == expected_deadline_count);
    assert(pb_capabilities(context, NULL) == PB_INVALID_ARGUMENT);
    caps.names_data = (PBRange *)&caps;
    assert(pb_capabilities(context, &caps) == PB_INVALID_ARGUMENT);
    caps.names_data = names;
    caps.names_capacity = 257;
    assert(pb_capabilities(context, &caps) == PB_LIMIT_EXCEEDED);

    PBArgument arguments[2] = {0};
    for (uint32_t i = 0; i < 2; i++) arguments[i].size = sizeof(PBArgument);
    memset(arguments, 0x55, sizeof(arguments));
    for (uint32_t i = 0; i < 2; i++) { arguments[i].size = sizeof(PBArgument); arguments[i].written = 0; }
    memset(bytes, 0x55, sizeof(bytes));
    PBArguments args = {.size=sizeof(PBArguments),.arguments_data=arguments,.arguments_capacity=2,.arguments_count=99,.arguments_stride=sizeof(PBArgument),.bytes_data=bytes,.bytes_capacity=0,.bytes_count=99};
    assert(pb_startup_arguments(context, &args) == PB_BUFFER_TOO_SMALL);
    assert(args.arguments_count == 2 && args.bytes_count == 12);
    assert(arguments[0].name_offset == 0x55555555 && bytes[0] == 0x55);
    args.bytes_capacity = sizeof(bytes);
    assert(pb_startup_arguments(context, &args) == PB_OK);
    assert(arguments[0].value_count == 0 && arguments[1].value_count == 3);
    assert(memcmp(bytes + arguments[1].value_offset, "\xe5\xb0\x8f", 3) == 0);
    args.bytes_data = (uint8_t *)arguments;
    assert(pb_startup_arguments(context, &args) == PB_INVALID_ARGUMENT);
    assert(pb_startup_arguments(context, NULL) == PB_INVALID_ARGUMENT);

    PBParticipant people[1] = {0};
    for (uint32_t i = 0; i < 1; i++) people[i].size = sizeof(PBParticipant);
    memset(people, 0x55, sizeof(people));
    for (uint32_t i = 0; i < 1; i++) { people[i].size = sizeof(PBParticipant); people[i].written = 0; }
    uint32_t presence_count = 99;
    assert(pb_presence(context, people, 0, sizeof(PBParticipant), &presence_count) == (participant_count ? PB_BUFFER_TOO_SMALL : PB_OK));
    assert(presence_count == participant_count && people[0].build_id[0] == 0x55);
    assert(pb_presence(context, people, 1, sizeof(PBParticipant), &presence_count) == PB_OK);
    if (participant_count) {
        assert(people[0].account_id[0] == seed);
        assert(people[0].device_id[0] == seed + 1);
        assert(people[0].instance_id[0] == seed + 2);
        assert(people[0].build_id[0] == seed + 3);
    }
    assert(pb_presence(context, people, 1, sizeof(PBParticipant), NULL) == PB_INVALID_ARGUMENT);
    assert(pb_presence(context, people, 1, sizeof(PBParticipant), (uint32_t *)people) == PB_INVALID_ARGUMENT);

    return PB_OK;
}
uint64_t probe_environment_layout(uint32_t index) {
    const uint64_t layout[] = {
        sizeof(PBCapabilities), _Alignof(PBCapabilities), offsetof(PBCapabilities, bytes_data),
        sizeof(PBArgument), _Alignof(PBArgument), offsetof(PBArgument, value_offset),
        sizeof(PBArguments), _Alignof(PBArguments), offsetof(PBArguments, bytes_data),
        sizeof(PBParticipant), _Alignof(PBParticipant), offsetof(PBParticipant, build_id),
        sizeof(PBSurface), _Alignof(PBSurface), offsetof(PBSurface, width),
        offsetof(PBSurface, height), offsetof(PBSurface, pixel_scale), offsetof(PBSurface, inputs),
    };
    assert(index < sizeof(layout) / sizeof(layout[0]));
    return layout[index];
}

// Independent native C consumer: private writes publish only at close.
static PBFileHandle file_open(PBContext *context, PBFileHandle root, uint32_t mode) {
    PBOperationHandle operation;
    PBFileHandle file;
    assert(probe_handle(pb_file_open_begin(context, root, (const uint8_t *)"probe.txt", 9, mode), &operation) == PB_OK);
    assert(probe_handle(pb_file_open_take(context, operation), &file) == PB_OK);
    assert(probe_handle(pb_file_open_take(context, operation), &file) == PB_STALE_HANDLE);
    return file;
}
static void file_close(PBContext *context, PBFileHandle file, uint32_t flags, uint32_t publication) {
    PBOperationHandle operation;
    PBFileCloseResult result = {.size=sizeof(PBFileCloseResult)};
    assert(probe_handle(pb_file_close_begin(context, file, flags), &operation) == PB_OK);
    assert(pb_file_close_take(context, operation, &result) == PB_OK);
    assert(result.error == 0 && result.publication == publication && result.recovery == 0);
}
PBStatus probe_file_calls(PBContext *context, PBFileHandle root) {
    const uint8_t content[] = {1, 2, 3, 4, 5};
    PBOperationHandle operation;
    PBFileHandle writer = file_open(context, root, PB_FILE_WRITE);
    assert(probe_handle(pb_file_write_begin(context, writer, 0, content, sizeof(content)), &operation) == PB_OK);
    assert(pb_file_write_take(context, operation) == PB_OK);
    assert(pb_file_write_take(context, operation) == PB_STALE_HANDLE);
    assert(probe_handle(pb_file_read_begin(context, writer, 0, 5), &operation) == PB_INVALID_ARGUMENT);
    assert(probe_handle(pb_file_write_begin(context, writer, 0, NULL, 5), &operation) == PB_INVALID_ARGUMENT);
    assert(probe_handle(pb_file_write_begin(context, writer, 0, content, 1048577), &operation) == PB_LIMIT_EXCEEDED);
    file_close(context, writer, 0, PB_FILE_PUBLISHED);
    assert(probe_handle(pb_file_write_begin(context, writer, 0, content, 5), &operation) == PB_STALE_HANDLE);
    PBFileHandle reader = file_open(context, root, PB_FILE_READ);
    uint8_t storage[8]; memset(storage, 0x55, sizeof(storage));
    uint32_t byte_count = 77;
    assert(probe_handle(pb_file_read_begin(context, reader, 0, 5), &operation) == PB_OK);
    assert(pb_file_read_take(context, operation, storage, 2, &byte_count) == PB_BUFFER_TOO_SMALL);
    assert(byte_count == 5 && storage[0] == 0x55);
    assert(pb_file_read_take(context, operation, (uint8_t *)&byte_count, sizeof(storage), &byte_count) == PB_INVALID_ARGUMENT);
    assert(pb_file_read_take(context, operation, storage, sizeof(storage), NULL) == PB_INVALID_ARGUMENT);
    assert(pb_file_read_take(context, operation, storage, sizeof(storage), &byte_count) == PB_OK);
    assert(byte_count == 5 && memcmp(storage, content, 5) == 0);
    assert(pb_file_read_take(context, operation, storage, sizeof(storage), &byte_count) == PB_STALE_HANDLE);
    assert(probe_handle(pb_file_read_begin(context, reader, 3, 5), &operation) == PB_OK);
    assert(pb_file_read_take(context, operation, storage, sizeof(storage), &byte_count) == PB_OK);
    assert(byte_count == 2 && storage[0] == 4 && storage[1] == 5);
    assert(probe_handle(pb_file_read_begin(context, reader, 6, 5), &operation) == PB_OK);
    assert(pb_file_read_take(context, operation, storage, sizeof(storage), &byte_count) == PB_OK && byte_count == 0);
    assert(probe_handle(pb_file_read_begin(context, reader, 0, 0), &operation) == PB_OK);
    assert(pb_file_read_take(context, operation, storage, sizeof(storage), &byte_count) == PB_OK && byte_count == 0);
    assert(probe_handle(pb_file_read_begin(context, reader, UINT64_MAX, 5), &operation) == PB_LIMIT_EXCEEDED);
    assert(probe_handle(pb_file_read_begin(context, reader, 0, 65537), &operation) == PB_LIMIT_EXCEEDED);
    PBFileHandle stale = reader; stale++;
    assert(probe_handle(pb_file_read_begin(context, stale, 0, 5), &operation) == PB_STALE_HANDLE);
    assert(probe_handle(pb_file_write_begin(context, reader, 0, content, 5), &operation) == PB_INVALID_ARGUMENT);
    assert(probe_handle(pb_file_status_begin(context, reader), &operation) == PB_OK);
    PBFileStatus status = {.size=sizeof(PBFileStatus)};
    assert(pb_file_status_take(context, operation, &status) == PB_OK);
    assert(status.kind == PB_FILE_KIND_FILE && status.file_size == 5 && (status.flags & PB_FILE_SIZE_VALID));
    file_close(context, reader, 0, PB_FILE_NOT_PUBLISHED);
    writer = file_open(context, root, PB_FILE_WRITE);
    assert(probe_handle(pb_file_write_begin(context, writer, 0, NULL, 0), &operation) == PB_OK);
    assert(pb_file_write_take(context, operation) == PB_OK);
    file_close(context, writer, PB_FILE_CLOSE_DISCARD, PB_FILE_NOT_PUBLISHED);
    return PB_OK;
}
PBStatus probe_persistent_file_begin(PBContext *context, PBFileHandle file,
    const uint8_t *bytes, uint32_t length, PBOperationHandle *output) {
    return probe_handle(pb_file_write_begin(context, file, 0, bytes, length), output);
}
PBStatus probe_persistent_file_take(PBContext *context, PBOperationHandle handle) {
    return pb_file_write_take(context, handle);
}
uint64_t probe_file_layout(uint32_t index) {
    const uint64_t layout[] = {
        sizeof(PBFileHandle), _Alignof(PBFileHandle), sizeof(uint32_t),
        sizeof(PBFileStatus), _Alignof(PBFileStatus), offsetof(PBFileStatus, modification_time),
        sizeof(PBFileCloseResult), _Alignof(PBFileCloseResult), offsetof(PBFileCloseResult, recovery),
        sizeof(PBFileListEntry), _Alignof(PBFileListEntry), offsetof(PBFileListEntry, name_data),
    };
    assert(index < sizeof(layout) / sizeof(layout[0]));
    return layout[index];
}

void probe_pending_begin(PBContext *context, PBEntryHandle volume, PBOperationHandle output[3]) {
    PBId type, field;
    memset(&type, 1, sizeof(type)); memset(&field, 2, sizeof(field));
    PBFileReference source = {.size=sizeof(PBFileReference),.volume=volume.volume};
    memset(source.entry_id, 1, 15);
    PBFileReference destination = source;
    memset(destination.entry_id, 2, 15);
    PBValueInput initial_value = {.size=sizeof(PBValueInput),.kind=PB_BOOLEAN,.scalar=1};
    PBFieldInput initial_field = {.size=sizeof(PBFieldInput),.values=&initial_value,.count=1,.present=1,.stride=sizeof(PBValueInput)};memcpy(initial_field.id, field, 16);
    assert(probe_handle(pb_object_create_begin(context, &volume, &type, &initial_field, 1, sizeof(PBFieldInput)), &output[0]) == PB_OK);
    assert(probe_handle(pb_file_copy_begin(context, &source, &destination), &output[1]) == PB_OK);
    assert(probe_handle(pb_file_select_begin(context), &output[2]) == PB_OK);
    for (uint32_t i = 0; i < 3; i++) {
        assert(output[i] > 0 && output[i] <= INT64_MAX);
    }
}
void probe_pending_wait(PBContext *context, const PBOperationHandle handles[3]) {
    PBId id; memset(&id, 0x55, sizeof(id));
    uint8_t copied[15]; memset(copied, 0x55, sizeof(copied));
    PBSelection selection = {.size=sizeof(PBSelection)}; memset(&selection, 0x55, sizeof(selection)); selection.size = sizeof(selection); selection.written = 0;
    assert(pb_object_create_poll(context, handles[0], NULL) == PB_INVALID_ARGUMENT);
    assert(pb_file_copy_poll(context, handles[1], NULL) == PB_INVALID_ARGUMENT);
    assert(pb_file_select_poll(context, handles[2], NULL) == PB_INVALID_ARGUMENT);
    assert(pb_object_create_poll(context, handles[1], &id) == PB_STALE_HANDLE);
    assert(pb_file_copy_poll(context, handles[2], &copied) == PB_STALE_HANDLE);
    assert(pb_file_select_poll(context, handles[0], &selection) == PB_STALE_HANDLE);
    PBOperationHandle invalid = handles[0]; invalid++;
    assert(pb_object_create_poll(context, invalid, &id) == PB_STALE_HANDLE);
    assert(pb_object_create_poll(context, handles[0], &id) == PB_WOULD_BLOCK);
    assert(pb_file_copy_poll(context, handles[1], &copied) == PB_WOULD_BLOCK);
    assert(pb_file_select_poll(context, handles[2], &selection) == PB_WOULD_BLOCK);
    assert(id[0] == 0x55 && id[15] == 0x55);
    assert(copied[0] == 0x55 && copied[14] == 0x55);
    assert(selection.present == 0x55555555 && selection.entry_id[14] == 0x55);
}
void probe_pending_finish(PBContext *context, const PBOperationHandle handles[3], uint32_t canceled, PBVolumeHandle volume) {
    PBId id;
    uint8_t copied[15];
    assert(pb_object_create_poll(context, handles[0], &id) == PB_OK && id[0] == 3);
    assert(pb_file_copy_poll(context, handles[1], &copied) == PB_OK && copied[0] == 2);
    PBSelection selected = {.size=sizeof(PBSelection)}; memset(&selected, 0x55, sizeof(selected)); selected.size = sizeof(selected); selected.written = 0;
    assert(pb_file_select_poll(context, handles[2], &selected) == PB_OK);
    assert(selected.reserved == 0 && selected.present == !canceled);
    if (canceled) {
        uint8_t zero[15] = {0};
        assert(selected.volume == 0 && selected.entry_reserved == 0 && memcmp(selected.entry_id, zero, sizeof(zero)) == 0);
    } else {
        assert(selected.volume == volume);
        assert(selected.entry_id[0] == 3 && selected.entry_reserved == 0);
    }
    assert(pb_object_create_poll(context, handles[0], &id) == PB_STALE_HANDLE);
    assert(pb_file_copy_poll(context, handles[1], &copied) == PB_STALE_HANDLE);
    assert(pb_file_select_poll(context, handles[2], &selected) == PB_STALE_HANDLE);
}
uint64_t probe_pending_layout(uint32_t index) {
    const uint64_t layout[] = {
        sizeof(PBOperationHandle), _Alignof(PBOperationHandle), sizeof(uint32_t),
        sizeof(PBSelection), _Alignof(PBSelection),
        offsetof(PBSelection, volume),
    };
    assert(index < sizeof(layout) / sizeof(layout[0])); return layout[index];
}
void probe_pending_errors(PBContext *context, const PBOperationHandle handles[3], const int32_t expected[3]) {
    PBId id; memset(&id, 0x55, sizeof(id));
    uint8_t copied[15]; memset(copied, 0x55, sizeof(copied));
    PBSelection selected = {.size=sizeof(PBSelection)}; memset(&selected, 0x55, sizeof(selected)); selected.size = sizeof(selected); selected.written = 0;
    assert(pb_object_create_poll(context, handles[0], &id) == expected[0]);
    assert(pb_file_copy_poll(context, handles[1], &copied) == expected[1]);
    assert(pb_file_select_poll(context, handles[2], &selected) == expected[2]);
    assert(id[0] == 0x55 && id[15] == 0x55);
    assert(copied[0] == 0x55 && copied[14] == 0x55);
    assert(selected.present == 0x55555555 && selected.entry_id[14] == 0x55);
    assert(pb_object_create_poll(context, handles[0], &id) == PB_STALE_HANDLE);
    assert(pb_file_copy_poll(context, handles[1], &copied) == PB_STALE_HANDLE);
    assert(pb_file_select_poll(context, handles[2], &selected) == PB_STALE_HANDLE);
}
void probe_authenticated_read(PBContext *context, const PBEntryHandle *volume,
    const PBFieldMetadata *expected, uint32_t value_count, uint32_t byte_count) {
    PBId object, field; memset(&object, 2, sizeof(object)); memset(&field, 4, sizeof(field));
    PBOperationHandle operation;
    assert(probe_handle(pb_object_field_read_authenticated_begin(context, volume, NULL, &field), &operation) == PB_INVALID_ARGUMENT);
    assert(probe_handle(pb_object_field_read_authenticated_begin(context, volume, &object, &field), &operation) == PB_OK);
    PBValue values[2] = {0};
    for (uint32_t i = 0; i < 2; i++) values[i].size = sizeof(PBValue); memset(values, 0x55, sizeof(values));
    for (uint32_t i = 0; i < 2; i++) { values[i].size = sizeof(PBValue); values[i].written = 0; }
    uint8_t bytes[32]; memset(bytes, 0x55, sizeof(bytes));
    PBFieldMetadata metadata;
    memset(&metadata, 0x55, sizeof(metadata));
    metadata.size = sizeof(metadata); metadata.written = 0;
    PBFieldMetadata untouched = metadata;
    PBAuthenticatedField result = {.size=sizeof(PBAuthenticatedField),.metadata=&metadata,
        .values_data=values,.values_count=99,.values_stride=sizeof(PBValue),
        .bytes_data=bytes,.bytes_count=99};
    if (value_count || byte_count) {
        assert(pb_object_field_read_authenticated_take(context, operation, &result) == PB_BUFFER_TOO_SMALL);
        assert(result.values_count == value_count && result.bytes_count == byte_count);
        assert(memcmp(&metadata, &untouched, sizeof(untouched)) == 0);
        assert(values[0].kind == 0x55555555 && bytes[0] == 0x55);
    }
    result.values_capacity = 2; result.bytes_capacity = sizeof(bytes);
    assert(pb_object_field_read_authenticated_take(context, operation, &result) == PB_OK);
    assert(metadata.size == sizeof(metadata) && metadata.written == sizeof(metadata));
    assert(memcmp((const uint8_t *)&metadata + 8, (const uint8_t *)expected + 8, sizeof(metadata) - 8) == 0);
    assert(result.values_count == value_count && result.bytes_count == byte_count);
    if (value_count) {
        assert(values[0].kind == PB_TEXT && values[0].bytes_count == 5 && memcmp(bytes,"hello",5) == 0);
        assert(values[1].kind == PB_INTEGER && values[1].scalar == 42);
    }
    assert(pb_object_field_read_authenticated_take(context, operation, &result) == PB_STALE_HANDLE);
    assert(probe_handle(pb_object_field_read_authenticated_begin(context, volume, &object, &field), &operation) == PB_OK);
    assert(pb_object_field_read_authenticated_take(context, operation, NULL) == PB_INVALID_ARGUMENT);
    result.bytes_data = (uint8_t *)&metadata;
    assert(pb_object_field_read_authenticated_take(context, operation, &result) == PB_INVALID_ARGUMENT);
    result.bytes_data = (uint8_t *)values;
    assert(pb_object_field_read_authenticated_take(context, operation, &result) == PB_INVALID_ARGUMENT);
    result.bytes_data = bytes; result.bytes_capacity = 1048577;
    assert(pb_object_field_read_authenticated_take(context, operation, &result) == PB_LIMIT_EXCEEDED);
    result.bytes_capacity = sizeof(bytes);
    assert(pb_object_field_read_authenticated_take(context, operation, &result) == PB_OK);
}
uint64_t probe_authenticated_layout(uint32_t index) {
    const uint64_t layout[] = {
        sizeof(PBFieldMetadata), _Alignof(PBFieldMetadata), offsetof(PBFieldMetadata, revision),
        offsetof(PBFieldMetadata, principal_id), offsetof(PBFieldMetadata, state),
        offsetof(PBFieldMetadata, provenance), offsetof(PBFieldMetadata, principal_kind),
        sizeof(PBAuthenticatedField), _Alignof(PBAuthenticatedField),
        offsetof(PBAuthenticatedField, values_data), offsetof(PBAuthenticatedField, bytes_data),
    };
    assert(index < sizeof(layout)/sizeof(layout[0])); return layout[index];
}

void probe_pending_withdrawn(PBContext *context, const PBOperationHandle handles[3]) {
    PBId id;
    assert(pb_object_create_poll(context, handles[0], &id) == PB_OK && id[0] == 3);
    uint8_t copied[15];
    assert(pb_file_copy_poll(context, handles[1], &copied) == PB_OK && copied[0] == 2);
    PBSelection selection = {.size=sizeof(PBSelection)}; memset(&selection, 0x55, sizeof(selection)); selection.size = sizeof(selection); selection.written = 0;
    assert(pb_file_select_poll(context, handles[2], &selection) == PB_ACCESS_DENIED);
    assert(selection.present == 0x55555555 && selection.entry_id[14] == 0x55);
    assert(pb_object_create_poll(context, handles[0], &id) == PB_STALE_HANDLE);
    assert(pb_file_copy_poll(context, handles[1], &copied) == PB_STALE_HANDLE);
    assert(pb_file_select_poll(context, handles[2], &selection) == PB_STALE_HANDLE);
}
