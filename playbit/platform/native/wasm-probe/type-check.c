// Frozen C field kinds complement independent compiler offset/width checks.
#include <stddef.h>
#include <playbit/sys/abi.h>
#include <playbit/native.h>

typedef uint8_t ExpectedBytes15[15];
typedef uint8_t ExpectedBytes16[16];
typedef uint8_t ExpectedBytes32[32];
typedef uint8_t ExpectedBytes256[256];
typedef PBStatus (*ExpectedInit)(PBContext *, void *, void **);
typedef PBStatus (*ExpectedTurn)(PBContext *, void *);
typedef void (*ExpectedDestroy)(void *);
typedef PBSysResult (*ExpectedDeadlinePoll)(PBContext *, PBEvent *, uint8_t *, uint32_t, uint64_t);

#ifdef __cplusplus
template <class T, class U> constexpr bool same_type = false;
template <class T> constexpr bool same_type<T, T> = true;
#define FIELD_TYPE(record, field, expected) \
    static_assert(same_type<decltype(((record *)0)->field), expected>, #record "." #field)
#else
#define FIELD_TYPE(record, field, expected) \
    _Static_assert(_Generic(&((record *)0)->field, expected *: 1, default: 0), #record "." #field)
#endif

#ifdef __cplusplus
static_assert(sizeof(PBSysResult) == 8 && PBSysResult(-1) < 0, "signed syscall result");
static_assert(sizeof(PBStatus) == 4, "callback status");
static_assert(same_type<decltype(pb_file_select_begin((PBContext *)0)), int64_t>, "handle result");
static_assert(same_type<decltype(&pb_random_fill), PBSysResult (*)(PBContext *, uint8_t *, uint32_t)>, "random output signature");
static_assert(same_type<decltype(&pb_event_poll_deadline), ExpectedDeadlinePoll>, "deadline poll signature");
#else
_Static_assert(sizeof(PBSysResult) == 8 && (PBSysResult)-1 < 0, "signed syscall result");
_Static_assert(sizeof(PBStatus) == 4, "callback status");
_Static_assert(_Generic(pb_file_select_begin((PBContext *)0), int64_t: 1, default: 0), "handle result");
_Static_assert(_Generic(&pb_random_fill, PBSysResult (*)(PBContext *, uint8_t *, uint32_t): 1, default: 0), "random output signature");
_Static_assert(_Generic(&pb_event_poll_deadline, ExpectedDeadlinePoll: 1, default: 0), "deadline poll signature");
#endif

FIELD_TYPE(PBEntryHandle, size, uint32_t);
FIELD_TYPE(PBEntryHandle, written, uint32_t);
FIELD_TYPE(PBEntryHandle, volume, uint64_t);
FIELD_TYPE(PBEntryHandle, entry_id, ExpectedBytes15);
FIELD_TYPE(PBEntryHandle, reserved, uint8_t);

FIELD_TYPE(PBRange, size, uint32_t);
FIELD_TYPE(PBRange, written, uint32_t);
FIELD_TYPE(PBRange, offset, uint32_t);
FIELD_TYPE(PBRange, count, uint32_t);

FIELD_TYPE(PBVolume, size, uint32_t);
FIELD_TYPE(PBVolume, written, uint32_t);
FIELD_TYPE(PBVolume, handle, uint64_t);
FIELD_TYPE(PBVolume, id, ExpectedBytes16);
FIELD_TYPE(PBVolume, name_offset, uint32_t);
FIELD_TYPE(PBVolume, name_count, uint32_t);
FIELD_TYPE(PBVolume, tags_offset, uint32_t);
FIELD_TYPE(PBVolume, tags_count, uint32_t);
FIELD_TYPE(PBVolume, rights, uint32_t);
FIELD_TYPE(PBVolume, flags, uint32_t);
FIELD_TYPE(PBVolume, file, uint64_t);
FIELD_TYPE(PBVolume, grant_id, ExpectedBytes16);

FIELD_TYPE(PBVolumes, size, uint32_t);
FIELD_TYPE(PBVolumes, written, uint32_t);
FIELD_TYPE(PBVolumes, revision, uint64_t);
FIELD_TYPE(PBVolumes, volumes_data, PBVolume *);
FIELD_TYPE(PBVolumes, volumes_capacity, uint32_t);
FIELD_TYPE(PBVolumes, volumes_count, uint32_t);
FIELD_TYPE(PBVolumes, volumes_stride, uint32_t);
FIELD_TYPE(PBVolumes, tags_data, PBRange *);
FIELD_TYPE(PBVolumes, tags_capacity, uint32_t);
FIELD_TYPE(PBVolumes, tags_count, uint32_t);
FIELD_TYPE(PBVolumes, tags_stride, uint32_t);
FIELD_TYPE(PBVolumes, bytes_data, uint8_t *);
FIELD_TYPE(PBVolumes, bytes_capacity, uint32_t);
FIELD_TYPE(PBVolumes, bytes_count, uint32_t);

FIELD_TYPE(PBValueInput, size, uint32_t);
FIELD_TYPE(PBValueInput, written, uint32_t);
FIELD_TYPE(PBValueInput, kind, uint32_t);
FIELD_TYPE(PBValueInput, length, uint32_t);
FIELD_TYPE(PBValueInput, scalar, uint64_t);
FIELD_TYPE(PBValueInput, id1, ExpectedBytes16);
FIELD_TYPE(PBValueInput, id2, ExpectedBytes16);
FIELD_TYPE(PBValueInput, entry_id, ExpectedBytes15);
FIELD_TYPE(PBValueInput, reserved, uint8_t);
FIELD_TYPE(PBValueInput, data, const uint8_t *);

FIELD_TYPE(PBFieldInput, size, uint32_t);
FIELD_TYPE(PBFieldInput, written, uint32_t);
FIELD_TYPE(PBFieldInput, id, ExpectedBytes16);
FIELD_TYPE(PBFieldInput, values, const PBValueInput *);
FIELD_TYPE(PBFieldInput, count, uint32_t);
FIELD_TYPE(PBFieldInput, stride, uint32_t);
FIELD_TYPE(PBFieldInput, present, uint32_t);

FIELD_TYPE(PBValue, size, uint32_t);
FIELD_TYPE(PBValue, written, uint32_t);
FIELD_TYPE(PBValue, kind, uint32_t);
FIELD_TYPE(PBValue, reserved, uint32_t);
FIELD_TYPE(PBValue, scalar, uint64_t);
FIELD_TYPE(PBValue, id1, ExpectedBytes16);
FIELD_TYPE(PBValue, id2, ExpectedBytes16);
FIELD_TYPE(PBValue, entry_id, ExpectedBytes15);
FIELD_TYPE(PBValue, entry_reserved, uint8_t);
FIELD_TYPE(PBValue, bytes_offset, uint32_t);
FIELD_TYPE(PBValue, bytes_count, uint32_t);

FIELD_TYPE(PBField, size, uint32_t);
FIELD_TYPE(PBField, written, uint32_t);
FIELD_TYPE(PBField, id, ExpectedBytes16);
FIELD_TYPE(PBField, values_offset, uint32_t);
FIELD_TYPE(PBField, values_count, uint32_t);
FIELD_TYPE(PBField, present, uint32_t);
FIELD_TYPE(PBField, reserved, uint32_t);

FIELD_TYPE(PBObject, size, uint32_t);
FIELD_TYPE(PBObject, written, uint32_t);
FIELD_TYPE(PBObject, type_id, ExpectedBytes16);
FIELD_TYPE(PBObject, fields_data, PBField *);
FIELD_TYPE(PBObject, fields_capacity, uint32_t);
FIELD_TYPE(PBObject, fields_count, uint32_t);
FIELD_TYPE(PBObject, fields_stride, uint32_t);
FIELD_TYPE(PBObject, values_data, PBValue *);
FIELD_TYPE(PBObject, values_capacity, uint32_t);
FIELD_TYPE(PBObject, values_count, uint32_t);
FIELD_TYPE(PBObject, values_stride, uint32_t);
FIELD_TYPE(PBObject, bytes_data, uint8_t *);
FIELD_TYPE(PBObject, bytes_capacity, uint32_t);
FIELD_TYPE(PBObject, bytes_count, uint32_t);

FIELD_TYPE(PBQuery, size, uint32_t);
FIELD_TYPE(PBQuery, written, uint32_t);
FIELD_TYPE(PBQuery, type_id, ExpectedBytes16);
FIELD_TYPE(PBQuery, after, ExpectedBytes16);
FIELD_TYPE(PBQuery, flags, uint32_t);
FIELD_TYPE(PBQuery, limit, uint32_t);
FIELD_TYPE(PBQuery, fields, const ExpectedBytes16 *);
FIELD_TYPE(PBQuery, count, uint32_t);
FIELD_TYPE(PBQuery, reserved, uint32_t);

FIELD_TYPE(PBObjectInfo, size, uint32_t);
FIELD_TYPE(PBObjectInfo, written, uint32_t);
FIELD_TYPE(PBObjectInfo, id, ExpectedBytes16);
FIELD_TYPE(PBObjectInfo, type_id, ExpectedBytes16);
FIELD_TYPE(PBObjectInfo, fields_offset, uint32_t);
FIELD_TYPE(PBObjectInfo, fields_count, uint32_t);

FIELD_TYPE(PBPage, size, uint32_t);
FIELD_TYPE(PBPage, written, uint32_t);
FIELD_TYPE(PBPage, next, ExpectedBytes16);
FIELD_TYPE(PBPage, has_next, uint32_t);
FIELD_TYPE(PBPage, projected, uint32_t);
FIELD_TYPE(PBPage, objects_data, PBObjectInfo *);
FIELD_TYPE(PBPage, objects_capacity, uint32_t);
FIELD_TYPE(PBPage, objects_count, uint32_t);
FIELD_TYPE(PBPage, objects_stride, uint32_t);
FIELD_TYPE(PBPage, fields_data, PBField *);
FIELD_TYPE(PBPage, fields_capacity, uint32_t);
FIELD_TYPE(PBPage, fields_count, uint32_t);
FIELD_TYPE(PBPage, fields_stride, uint32_t);
FIELD_TYPE(PBPage, values_data, PBValue *);
FIELD_TYPE(PBPage, values_capacity, uint32_t);
FIELD_TYPE(PBPage, values_count, uint32_t);
FIELD_TYPE(PBPage, values_stride, uint32_t);
FIELD_TYPE(PBPage, bytes_data, uint8_t *);
FIELD_TYPE(PBPage, bytes_capacity, uint32_t);
FIELD_TYPE(PBPage, bytes_count, uint32_t);

FIELD_TYPE(PBSchemaFieldInput, size, uint32_t);
FIELD_TYPE(PBSchemaFieldInput, written, uint32_t);
FIELD_TYPE(PBSchemaFieldInput, id, ExpectedBytes16);
FIELD_TYPE(PBSchemaFieldInput, name_data, const uint8_t *);
FIELD_TYPE(PBSchemaFieldInput, name_length, uint32_t);
FIELD_TYPE(PBSchemaFieldInput, kind, uint32_t);
FIELD_TYPE(PBSchemaFieldInput, flags, uint32_t);
FIELD_TYPE(PBSchemaFieldInput, reference_type, ExpectedBytes16);

FIELD_TYPE(PBSchemaTypeInput, size, uint32_t);
FIELD_TYPE(PBSchemaTypeInput, written, uint32_t);
FIELD_TYPE(PBSchemaTypeInput, id, ExpectedBytes16);
FIELD_TYPE(PBSchemaTypeInput, name_data, const uint8_t *);
FIELD_TYPE(PBSchemaTypeInput, name_length, uint32_t);
FIELD_TYPE(PBSchemaTypeInput, fields, const PBSchemaFieldInput *);
FIELD_TYPE(PBSchemaTypeInput, count, uint32_t);
FIELD_TYPE(PBSchemaTypeInput, stride, uint32_t);
FIELD_TYPE(PBSchemaTypeInput, flags, uint32_t);

FIELD_TYPE(PBSchemaField, size, uint32_t);
FIELD_TYPE(PBSchemaField, written, uint32_t);
FIELD_TYPE(PBSchemaField, id, ExpectedBytes16);
FIELD_TYPE(PBSchemaField, name_offset, uint32_t);
FIELD_TYPE(PBSchemaField, name_count, uint32_t);
FIELD_TYPE(PBSchemaField, kind, uint32_t);
FIELD_TYPE(PBSchemaField, flags, uint32_t);
FIELD_TYPE(PBSchemaField, reference_type, ExpectedBytes16);

FIELD_TYPE(PBSchemaType, size, uint32_t);
FIELD_TYPE(PBSchemaType, written, uint32_t);
FIELD_TYPE(PBSchemaType, id, ExpectedBytes16);
FIELD_TYPE(PBSchemaType, name_offset, uint32_t);
FIELD_TYPE(PBSchemaType, name_count, uint32_t);
FIELD_TYPE(PBSchemaType, fields_offset, uint32_t);
FIELD_TYPE(PBSchemaType, fields_count, uint32_t);
FIELD_TYPE(PBSchemaType, flags, uint32_t);
FIELD_TYPE(PBSchemaType, reserved, uint32_t);

FIELD_TYPE(PBSchema, size, uint32_t);
FIELD_TYPE(PBSchema, written, uint32_t);
FIELD_TYPE(PBSchema, types_data, PBSchemaType *);
FIELD_TYPE(PBSchema, types_capacity, uint32_t);
FIELD_TYPE(PBSchema, types_count, uint32_t);
FIELD_TYPE(PBSchema, types_stride, uint32_t);
FIELD_TYPE(PBSchema, fields_data, PBSchemaField *);
FIELD_TYPE(PBSchema, fields_capacity, uint32_t);
FIELD_TYPE(PBSchema, fields_count, uint32_t);
FIELD_TYPE(PBSchema, fields_stride, uint32_t);
FIELD_TYPE(PBSchema, bytes_data, uint8_t *);
FIELD_TYPE(PBSchema, bytes_capacity, uint32_t);
FIELD_TYPE(PBSchema, bytes_count, uint32_t);

FIELD_TYPE(PBCapabilities, size, uint32_t);
FIELD_TYPE(PBCapabilities, written, uint32_t);
FIELD_TYPE(PBCapabilities, names_data, PBRange *);
FIELD_TYPE(PBCapabilities, names_capacity, uint32_t);
FIELD_TYPE(PBCapabilities, names_count, uint32_t);
FIELD_TYPE(PBCapabilities, names_stride, uint32_t);
FIELD_TYPE(PBCapabilities, bytes_data, uint8_t *);
FIELD_TYPE(PBCapabilities, bytes_capacity, uint32_t);
FIELD_TYPE(PBCapabilities, bytes_count, uint32_t);

FIELD_TYPE(PBArgument, size, uint32_t);
FIELD_TYPE(PBArgument, written, uint32_t);
FIELD_TYPE(PBArgument, name_offset, uint32_t);
FIELD_TYPE(PBArgument, name_count, uint32_t);
FIELD_TYPE(PBArgument, value_offset, uint32_t);
FIELD_TYPE(PBArgument, value_count, uint32_t);

FIELD_TYPE(PBArguments, size, uint32_t);
FIELD_TYPE(PBArguments, written, uint32_t);
FIELD_TYPE(PBArguments, arguments_data, PBArgument *);
FIELD_TYPE(PBArguments, arguments_capacity, uint32_t);
FIELD_TYPE(PBArguments, arguments_count, uint32_t);
FIELD_TYPE(PBArguments, arguments_stride, uint32_t);
FIELD_TYPE(PBArguments, bytes_data, uint8_t *);
FIELD_TYPE(PBArguments, bytes_capacity, uint32_t);
FIELD_TYPE(PBArguments, bytes_count, uint32_t);

FIELD_TYPE(PBParticipant, size, uint32_t);
FIELD_TYPE(PBParticipant, written, uint32_t);
FIELD_TYPE(PBParticipant, account_id, ExpectedBytes16);
FIELD_TYPE(PBParticipant, device_id, ExpectedBytes16);
FIELD_TYPE(PBParticipant, instance_id, ExpectedBytes16);
FIELD_TYPE(PBParticipant, build_id, ExpectedBytes32);
FIELD_TYPE(PBParticipant, display_name, ExpectedBytes256);

FIELD_TYPE(PBFileReference, size, uint32_t);
FIELD_TYPE(PBFileReference, written, uint32_t);
FIELD_TYPE(PBFileReference, volume, uint64_t);
FIELD_TYPE(PBFileReference, entry_id, ExpectedBytes15);
FIELD_TYPE(PBFileReference, reserved, uint8_t);

FIELD_TYPE(PBFileStatus, size, uint32_t);
FIELD_TYPE(PBFileStatus, written, uint32_t);
FIELD_TYPE(PBFileStatus, kind, uint32_t);
FIELD_TYPE(PBFileStatus, flags, uint32_t);
FIELD_TYPE(PBFileStatus, file_size, uint64_t);
FIELD_TYPE(PBFileStatus, modification_time, int64_t);

FIELD_TYPE(PBFileCloseResult, size, uint32_t);
FIELD_TYPE(PBFileCloseResult, written, uint32_t);
FIELD_TYPE(PBFileCloseResult, error, int32_t);
FIELD_TYPE(PBFileCloseResult, publication, uint32_t);
FIELD_TYPE(PBFileCloseResult, recovery, uint64_t);

FIELD_TYPE(PBFileListEntry, size, uint32_t);
FIELD_TYPE(PBFileListEntry, written, uint32_t);
FIELD_TYPE(PBFileListEntry, outcome, uint32_t);
FIELD_TYPE(PBFileListEntry, kind, uint32_t);
FIELD_TYPE(PBFileListEntry, name_data, uint8_t *);
FIELD_TYPE(PBFileListEntry, name_capacity, uint32_t);
FIELD_TYPE(PBFileListEntry, name_count, uint32_t);

FIELD_TYPE(PBSelection, size, uint32_t);
FIELD_TYPE(PBSelection, written, uint32_t);
FIELD_TYPE(PBSelection, present, uint32_t);
FIELD_TYPE(PBSelection, reserved, uint32_t);
FIELD_TYPE(PBSelection, volume, uint64_t);
FIELD_TYPE(PBSelection, entry_id, ExpectedBytes15);
FIELD_TYPE(PBSelection, entry_reserved, uint8_t);

FIELD_TYPE(PBEvent, size, uint32_t);
FIELD_TYPE(PBEvent, written, uint32_t);
FIELD_TYPE(PBEvent, generation, uint64_t);
FIELD_TYPE(PBEvent, kind, uint32_t);
FIELD_TYPE(PBEvent, target, uint32_t);
FIELD_TYPE(PBEvent, object_id, uint32_t);
FIELD_TYPE(PBIdentity, size, uint32_t);
FIELD_TYPE(PBIdentity, written, uint32_t);
FIELD_TYPE(PBIdentity, generation, uint64_t);
FIELD_TYPE(PBIdentity, account_id, ExpectedBytes16);
FIELD_TYPE(PBIdentity, device_id, ExpectedBytes16);
FIELD_TYPE(PBIdentity, instance_id, ExpectedBytes16);
FIELD_TYPE(PBIdentity, session_id, ExpectedBytes32);
FIELD_TYPE(PBIdentity, flags, uint32_t);
FIELD_TYPE(PBIdentity, reserved, uint32_t);
FIELD_TYPE(PBIdentity, display_name, ExpectedBytes256);

FIELD_TYPE(PBApp, size, uint32_t);
FIELD_TYPE(PBApp, written, uint32_t);
FIELD_TYPE(PBApp, init, ExpectedInit);
FIELD_TYPE(PBApp, turn, ExpectedTurn);
FIELD_TYPE(PBApp, destroy, ExpectedDestroy);
FIELD_TYPE(PBApp, user_data, void *);

FIELD_TYPE(PBFieldMetadata, size, uint32_t);
FIELD_TYPE(PBFieldMetadata, written, uint32_t);
FIELD_TYPE(PBFieldMetadata, volume_id, ExpectedBytes16);
FIELD_TYPE(PBFieldMetadata, object_id, ExpectedBytes16);
FIELD_TYPE(PBFieldMetadata, type_id, ExpectedBytes16);
FIELD_TYPE(PBFieldMetadata, field_id, ExpectedBytes16);
FIELD_TYPE(PBFieldMetadata, revision, ExpectedBytes32);
FIELD_TYPE(PBFieldMetadata, principal_id, ExpectedBytes16);
FIELD_TYPE(PBFieldMetadata, device_id, ExpectedBytes16);
FIELD_TYPE(PBFieldMetadata, state, uint32_t);
FIELD_TYPE(PBFieldMetadata, provenance, uint32_t);
FIELD_TYPE(PBFieldMetadata, principal_kind, uint32_t);
FIELD_TYPE(PBFieldMetadata, reserved, uint32_t);
FIELD_TYPE(PBFieldMetadata, entry_id, ExpectedBytes15);
FIELD_TYPE(PBFieldMetadata, entry_reserved, uint8_t);

FIELD_TYPE(PBAuthenticatedField, size, uint32_t);
FIELD_TYPE(PBAuthenticatedField, written, uint32_t);
FIELD_TYPE(PBAuthenticatedField, metadata, PBFieldMetadata *);
FIELD_TYPE(PBAuthenticatedField, values_data, PBValue *);
FIELD_TYPE(PBAuthenticatedField, values_capacity, uint32_t);
FIELD_TYPE(PBAuthenticatedField, values_count, uint32_t);
FIELD_TYPE(PBAuthenticatedField, values_stride, uint32_t);
FIELD_TYPE(PBAuthenticatedField, bytes_data, uint8_t *);
FIELD_TYPE(PBAuthenticatedField, bytes_capacity, uint32_t);
FIELD_TYPE(PBAuthenticatedField, bytes_count, uint32_t);

FIELD_TYPE(PBKvQuery, size, uint32_t);
FIELD_TYPE(PBKvQuery, written, uint32_t);
FIELD_TYPE(PBKvQuery, root_level, uint32_t);
FIELD_TYPE(PBKvQuery, root_digest, ExpectedBytes32);
FIELD_TYPE(PBKvQuery, has_root, uint32_t);
FIELD_TYPE(PBKvQuery, limit, uint32_t);
FIELD_TYPE(PBKvQuery, reserved, uint32_t);

FIELD_TYPE(PBKvRead, size, uint32_t);
FIELD_TYPE(PBKvRead, written, uint32_t);
FIELD_TYPE(PBKvRead, offset, uint64_t);
FIELD_TYPE(PBKvRead, limit, uint32_t);
FIELD_TYPE(PBKvRead, has_root, uint32_t);
FIELD_TYPE(PBKvRead, root_level, uint32_t);
FIELD_TYPE(PBKvRead, root_digest, ExpectedBytes32);
FIELD_TYPE(PBKvRead, reserved, uint32_t);

// Compile-only layout checks for C and C++ consumers.
#ifdef __cplusplus
#define PREFIX_ASSERT(test) static_assert(test)
#define PREFIX_ALIGN(type) alignof(type)
#else
#define PREFIX_ASSERT(test) _Static_assert(test, #test)
#define PREFIX_ALIGN(type) _Alignof(type)
#endif
PREFIX_ASSERT(sizeof(PBTraceEvent) == 40);
PREFIX_ASSERT(sizeof(PBInputSource) == 20);
PREFIX_ASSERT(sizeof(PBPointerInput) == 56);
PREFIX_ASSERT(sizeof(PBPenInput) == 92);
PREFIX_ASSERT(sizeof(PBKeyboardInput) == 60);
PREFIX_ASSERT(sizeof(PBScrollInput) == 44);
PREFIX_ASSERT(sizeof(PBGestureInput) == 48);
PREFIX_ASSERT(sizeof(PBGamepadInput) == 36);
PREFIX_ASSERT(PB_EVENT_POINTER == 11);
PREFIX_ASSERT(PB_EVENT_KEY == 12);
PREFIX_ASSERT(PB_EVENT_WINDOW_POINTER == 22);
PREFIX_ASSERT(PB_EVENT_WINDOW_KEYBOARD == 23);
PREFIX_ASSERT(PB_EVENT_WINDOW_SCROLL == 24);
PREFIX_ASSERT(PB_EVENT_WINDOW_GESTURE == 25);
PREFIX_ASSERT(PB_EVENT_WINDOW_GAMEPAD == 26);

PREFIX_ASSERT(offsetof(PBTraceEvent, reference_eid) == 8);
PREFIX_ASSERT(offsetof(PBTraceEvent, value) == 16);
PREFIX_ASSERT(offsetof(PBTraceEvent, reference_tid) == 24);
PREFIX_ASSERT(offsetof(PBTraceEvent, metadata_len) == 28);
PREFIX_ASSERT(offsetof(PBTraceEvent, name) == 32);
PREFIX_ASSERT(offsetof(PBTraceEvent, kind) == 34);

// Published native lifecycle prefixes; later fields cannot repurpose these.
PREFIX_ASSERT(sizeof(PBEngineVolume) >= 304);
PREFIX_ASSERT(PREFIX_ALIGN(PBEngineVolume) == 4);
FIELD_TYPE(PBEngineVolume, size, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineVolume, size) == 0);
FIELD_TYPE(PBEngineVolume, written, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineVolume, written) == 4);
FIELD_TYPE(PBEngineVolume, id, ExpectedBytes16);
PREFIX_ASSERT(offsetof(PBEngineVolume, id) == 8);
FIELD_TYPE(PBEngineVolume, owner, ExpectedBytes16);
PREFIX_ASSERT(offsetof(PBEngineVolume, owner) == 24);
FIELD_TYPE(PBEngineVolume, name, ExpectedBytes256);
PREFIX_ASSERT(offsetof(PBEngineVolume, name) == 40);
FIELD_TYPE(PBEngineVolume, name_length, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineVolume, name_length) == 296);
FIELD_TYPE(PBEngineVolume, share_key_with_service, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineVolume, share_key_with_service) == 300);
PREFIX_ASSERT(sizeof(PBEngineChange) >= 56);
PREFIX_ASSERT(PREFIX_ALIGN(PBEngineChange) == 8);
FIELD_TYPE(PBEngineChange, size, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChange, size) == 0);
FIELD_TYPE(PBEngineChange, written, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChange, written) == 4);
FIELD_TYPE(PBEngineChange, volume, ExpectedBytes16);
PREFIX_ASSERT(offsetof(PBEngineChange, volume) == 8);
FIELD_TYPE(PBEngineChange, sequence, uint64_t);
PREFIX_ASSERT(offsetof(PBEngineChange, sequence) == 24);
FIELD_TYPE(PBEngineChange, kind, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChange, kind) == 32);
FIELD_TYPE(PBEngineChange, database, ExpectedBytes16);
PREFIX_ASSERT(offsetof(PBEngineChange, database) == 36);
FIELD_TYPE(PBEngineChange, reserved, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChange, reserved) == 52);
PREFIX_ASSERT(sizeof(PBEngineChanges) >= 56);
PREFIX_ASSERT(PREFIX_ALIGN(PBEngineChanges) == 8);
FIELD_TYPE(PBEngineChanges, size, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChanges, size) == 0);
FIELD_TYPE(PBEngineChanges, written, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChanges, written) == 4);
FIELD_TYPE(PBEngineChanges, epoch, ExpectedBytes16);
PREFIX_ASSERT(offsetof(PBEngineChanges, epoch) == 8);
FIELD_TYPE(PBEngineChanges, gap_generation, uint64_t);
PREFIX_ASSERT(offsetof(PBEngineChanges, gap_generation) == 24);
FIELD_TYPE(PBEngineChanges, account_generation, uint64_t);
PREFIX_ASSERT(offsetof(PBEngineChanges, account_generation) == 32);
FIELD_TYPE(PBEngineChanges, closed, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChanges, closed) == 40);
FIELD_TYPE(PBEngineChanges, records, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChanges, records) == 44);
FIELD_TYPE(PBEngineChanges, remaining, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChanges, remaining) == 48);
FIELD_TYPE(PBEngineChanges, reserved, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineChanges, reserved) == 52);
PREFIX_ASSERT(sizeof(PBEngineAppStep) >= 24);
PREFIX_ASSERT(PREFIX_ALIGN(PBEngineAppStep) == 8);
FIELD_TYPE(PBEngineAppStep, size, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineAppStep, size) == 0);
FIELD_TYPE(PBEngineAppStep, written, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineAppStep, written) == 4);
FIELD_TYPE(PBEngineAppStep, wake_after_ns, uint64_t);
PREFIX_ASSERT(offsetof(PBEngineAppStep, wake_after_ns) == 8);
FIELD_TYPE(PBEngineAppStep, flags, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineAppStep, flags) == 16);
FIELD_TYPE(PBEngineAppStep, reserved, uint32_t);
PREFIX_ASSERT(offsetof(PBEngineAppStep, reserved) == 20);
FIELD_TYPE(PBEngineAppOutput, size, uint32_t);
FIELD_TYPE(PBEngineAppOutput, written, uint32_t);
FIELD_TYPE(PBEngineAppOutput, view, PBEngineAppView);
FIELD_TYPE(PBEngineAppOutput, log, PBEngineAppLog);
FIELD_TYPE(PBEngineAppOutput, user_data, void *);
PREFIX_ASSERT(offsetof(PBEngineAppOutput, size) == 0);
PREFIX_ASSERT(offsetof(PBEngineAppOutput, written) == 4);
PREFIX_ASSERT(offsetof(PBEngineAppOutput, view) == 8);
#if UINTPTR_MAX == UINT64_MAX
PREFIX_ASSERT(sizeof(PBEngineAppOutput) >= 32);
PREFIX_ASSERT(PREFIX_ALIGN(PBEngineAppOutput) == 8);
PREFIX_ASSERT(offsetof(PBEngineAppOutput, log) == 16);
PREFIX_ASSERT(offsetof(PBEngineAppOutput, user_data) == 24);
#else
PREFIX_ASSERT(sizeof(PBEngineAppOutput) >= 20);
PREFIX_ASSERT(PREFIX_ALIGN(PBEngineAppOutput) == 4);
PREFIX_ASSERT(offsetof(PBEngineAppOutput, log) == 12);
PREFIX_ASSERT(offsetof(PBEngineAppOutput, user_data) == 16);
#endif

FIELD_TYPE(PBBloblogBlob, size, uint32_t);
FIELD_TYPE(PBBloblogBlob, written, uint32_t);
FIELD_TYPE(PBBloblogBlob, data, const uint8_t *);
FIELD_TYPE(PBBloblogBlob, length, uint32_t);
PREFIX_ASSERT(offsetof(PBBloblogBlob, size) == 0);
PREFIX_ASSERT(offsetof(PBBloblogBlob, written) == 4);
PREFIX_ASSERT(offsetof(PBBloblogBlob, data) == 8);
#if UINTPTR_MAX == UINT64_MAX
PREFIX_ASSERT(sizeof(PBBloblogBlob) >= 24);
PREFIX_ASSERT(PREFIX_ALIGN(PBBloblogBlob) == 8);
PREFIX_ASSERT(offsetof(PBBloblogBlob, length) == 16);
#else
PREFIX_ASSERT(sizeof(PBBloblogBlob) >= 16);
PREFIX_ASSERT(PREFIX_ALIGN(PBBloblogBlob) == 4);
PREFIX_ASSERT(offsetof(PBBloblogBlob, length) == 12);
#endif

FIELD_TYPE(PBSessionInfo, size, uint32_t);
FIELD_TYPE(PBSessionInfo, written, uint32_t);
FIELD_TYPE(PBSessionInfo, flags, uint32_t);
FIELD_TYPE(PBSessionInfo, reserved, uint32_t);
FIELD_TYPE(PBSessionInfo, internal_id, ExpectedBytes32);
FIELD_TYPE(PBSessionInfo, key_secret, ExpectedBytes16);
PREFIX_ASSERT(sizeof(PBSessionInfo) >= 64);
PREFIX_ASSERT(PREFIX_ALIGN(PBSessionInfo) == 4);
PREFIX_ASSERT(offsetof(PBSessionInfo, size) == 0);
PREFIX_ASSERT(offsetof(PBSessionInfo, written) == 4);
PREFIX_ASSERT(offsetof(PBSessionInfo, flags) == 8);
PREFIX_ASSERT(offsetof(PBSessionInfo, reserved) == 12);
PREFIX_ASSERT(offsetof(PBSessionInfo, internal_id) == 16);
PREFIX_ASSERT(offsetof(PBSessionInfo, key_secret) == 48);
